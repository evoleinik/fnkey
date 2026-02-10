//! fnkey.ai - Hold Fn key, speak, paste transcribed text
//!
//! Usage:
//!   export GROQ_API_KEY="your-key"
//!   open FnKey.app

use std::collections::HashMap;
use std::env;
use std::ffi::c_void;
use std::io::Cursor;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use arboard::Clipboard;
use cocoa::appkit::{NSApp, NSApplication, NSApplicationActivationPolicyAccessory, NSMenu};
use cocoa::base::{id, nil};
use cocoa::foundation::{NSAutoreleasePool, NSString};
use core_foundation::runloop::{kCFRunLoopCommonModes, CFRunLoop};
use core_graphics::event::{
    CGEvent, CGEventFlags, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
    CGEventType,
};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::Stream;
use hound::{WavSpec, WavWriter};
use objc::declare::ClassDecl;
use objc::runtime::{Object, Sel};
use objc::{class, msg_send, sel, sel_impl};

// ============================================================================
// Keyboard layout detection (for non-Latin layouts like Russian)
// ============================================================================

/// Cached keycode map - built once on first access
static KEYCODE_MAP: OnceLock<HashMap<char, u16>> = OnceLock::new();

/// Opaque type for keyboard layout data structure
#[repr(C)]
struct UCKeyboardLayout {
    _opaque: [u8; 0],
}

// FFI declarations for Carbon/CoreServices APIs
#[link(name = "Carbon", kind = "framework")]
extern "C" {
    fn TISCopyCurrentASCIICapableKeyboardLayoutInputSource() -> *const c_void;
    fn TISGetInputSourceProperty(input_source: *const c_void, property_key: *const c_void) -> *const c_void;
    fn LMGetKbdType() -> u32;
    static kTISPropertyUnicodeKeyLayoutData: *const c_void;
}

#[link(name = "CoreServices", kind = "framework")]
extern "C" {
    fn UCKeyTranslate(
        key_layout_ptr: *const UCKeyboardLayout,
        virtual_key_code: u16,
        key_action: u16,
        modifier_key_state: u32,
        keyboard_type: u32,
        key_translate_options: u32,
        dead_key_state: *mut u32,
        max_string_length: usize,
        actual_string_length: *mut usize,
        unicode_string: *mut u16,
    ) -> i32;
}

const KUC_KEY_ACTION_DISPLAY: u16 = 3;
const QWERTY_V_KEYCODE: u16 = 9;

/// Build a lookup table mapping lowercase characters to their keycodes
fn build_char_to_keycode_map() -> HashMap<char, u16> {
    let mut map = HashMap::new();

    unsafe {
        let input_source = TISCopyCurrentASCIICapableKeyboardLayoutInputSource();
        if input_source.is_null() {
            return map;
        }

        let layout_data_ref = TISGetInputSourceProperty(input_source, kTISPropertyUnicodeKeyLayoutData);
        if layout_data_ref.is_null() {
            core_foundation::base::CFRelease(input_source);
            return map;
        }

        // Get the layout data bytes
        let layout_data: core_foundation::data::CFData =
            core_foundation::base::TCFType::wrap_under_get_rule(layout_data_ref as *const _);
        let layout_ptr = layout_data.bytes().as_ptr() as *const UCKeyboardLayout;
        let kbd_type = LMGetKbdType();

        // Iterate through keycodes 0-127 to build reverse lookup
        for keycode in 0u16..128 {
            let mut dead_key_state: u32 = 0;
            let mut char_buf: [u16; 4] = [0; 4];
            let mut actual_len: usize = 0;

            let result = UCKeyTranslate(
                layout_ptr,
                keycode,
                KUC_KEY_ACTION_DISPLAY,
                0,
                kbd_type,
                0,
                &mut dead_key_state,
                char_buf.len(),
                &mut actual_len,
                char_buf.as_mut_ptr(),
            );

            if result == 0 && actual_len == 1 {
                if let Some(ch) = char::from_u32(u32::from(char_buf[0])) {
                    map.entry(ch.to_ascii_lowercase()).or_insert(keycode);
                }
            }
        }

        core_foundation::base::CFRelease(input_source);
    }

    map
}

/// Get the keycode for 'v' in the current keyboard layout.
/// Falls back to QWERTY keycode (9) if lookup fails.
fn get_paste_keycode() -> u16 {
    let map = KEYCODE_MAP.get_or_init(build_char_to_keycode_map);
    map.get(&'v').copied().unwrap_or(QWERTY_V_KEYCODE)
}

// ============================================================================
// Main application
// ============================================================================

// Modifier key flags in CGEventFlags
const FN_KEY_FLAG: u64 = 0x800000;
const OPTION_KEY_FLAG: u64 = 0x80000;
const CONTROL_KEY_FLAG: u64 = 0x40000;
const SHIFT_KEY_FLAG: u64 = 0x20000;
const COMMAND_KEY_FLAG: u64 = 0x100000;

// ============================================================================
// Configuration
// ============================================================================

fn default_api_key() -> String {
    String::new()
}
fn default_transcription_url() -> String {
    "https://api.groq.com/openai/v1/audio/transcriptions".to_string()
}
fn default_polish_url() -> String {
    "https://api.groq.com/openai/v1/chat/completions".to_string()
}
fn default_whisper_model() -> String {
    "whisper-large-v3".to_string()
}
fn default_polish_model() -> String {
    "llama-3.3-70b-versatile".to_string()
}
fn default_hotkey() -> String {
    "fn".to_string()
}

#[derive(serde::Deserialize, Clone)]
struct Config {
    #[serde(default = "default_api_key")]
    api_key: String,
    #[serde(default = "default_transcription_url")]
    transcription_url: String,
    #[serde(default = "default_polish_url")]
    polish_url: String,
    #[serde(default = "default_whisper_model")]
    whisper_model: String,
    #[serde(default = "default_polish_model")]
    polish_model: String,
    #[serde(default = "default_hotkey")]
    hotkey: String,
}

impl Config {
    /// Returns the CGEventFlags bitmask for the configured hotkey
    fn hotkey_flag(&self) -> u64 {
        match self.hotkey.as_str() {
            "option" => OPTION_KEY_FLAG,
            "control" => CONTROL_KEY_FLAG,
            "shift" => SHIFT_KEY_FLAG,
            "command" => COMMAND_KEY_FLAG,
            _ => FN_KEY_FLAG, // "fn" or any unrecognized value
        }
    }

    /// Returns the modifier flag used to trigger polish mode.
    /// Normally Ctrl, but if hotkey is already Ctrl, use Shift instead.
    fn polish_flag(&self) -> u64 {
        if self.hotkey == "control" {
            SHIFT_KEY_FLAG
        } else {
            CONTROL_KEY_FLAG
        }
    }
}

/// Load configuration from TOML file, legacy api_key file, or environment variable.
/// Always returns a Config — creates a default config.toml if nothing exists.
fn load_config() -> Config {
    if let Some(home) = env::var_os("HOME") {
        let config_dir = std::path::Path::new(&home).join(".config").join("fnkey");

        // Try config.toml first
        let toml_path = config_dir.join("config.toml");
        if let Ok(contents) = std::fs::read_to_string(&toml_path) {
            if let Ok(config) = toml::from_str::<Config>(&contents) {
                return config;
            }
        }

        // Try legacy api_key file
        let key_path = config_dir.join("api_key");
        if let Ok(key) = std::fs::read_to_string(&key_path) {
            let key = key.trim();
            if !key.is_empty() {
                return Config {
                    api_key: key.to_string(),
                    transcription_url: default_transcription_url(),
                    polish_url: default_polish_url(),
                    whisper_model: default_whisper_model(),
                    polish_model: default_polish_model(),
                    hotkey: default_hotkey(),
                };
            }
        }

        // Try environment variable
        if let Ok(key) = env::var("GROQ_API_KEY") {
            return Config {
                api_key: key,
                transcription_url: default_transcription_url(),
                polish_url: default_polish_url(),
                whisper_model: default_whisper_model(),
                polish_model: default_polish_model(),
                hotkey: default_hotkey(),
            };
        }

        // No config found — create a default config.toml for the user to edit
        let _ = std::fs::create_dir_all(&config_dir);
        let default_toml = r#"# FnKey configuration — edit and relaunch
# api_key = "your-api-key"
# transcription_url = "https://your-server/v1/audio/transcriptions"
# polish_url = "https://your-server/v1/chat/completions"
# whisper_model = "whisper-large-v3"
# polish_model = "llama-3.3-70b-versatile"
# hotkey = "fn"
"#;
        let _ = std::fs::write(&toml_path, default_toml);
    }

    // Return defaults — app will launch but transcription won't work until configured
    Config {
        api_key: default_api_key(),
        transcription_url: default_transcription_url(),
        polish_url: default_polish_url(),
        whisper_model: default_whisper_model(),
        polish_model: default_polish_model(),
        hotkey: default_hotkey(),
    }
}

struct AppState {
    audio_buffer: Arc<Mutex<Vec<f32>>>,
    config: Config,
    sample_rate: std::sync::atomic::AtomicU32,
}

// Global status item pointer for updating from callbacks
static mut STATUS_ITEM: *mut Object = std::ptr::null_mut();
// Global audio stream (not Send, so can't be in Arc)
static mut AUDIO_STREAM: Option<Stream> = None;

fn main() {
    let config = load_config();

    // Request Input Monitoring permission (non-blocking — app continues either way)
    check_input_monitoring_permission();

    let state = Arc::new(AppState {
        audio_buffer: Arc::new(Mutex::new(Vec::new())),
        config,
        sample_rate: std::sync::atomic::AtomicU32::new(48000), // Default, will be updated
    });

    // Initialize NSApplication
    unsafe {
        let _pool = NSAutoreleasePool::new(nil);
        let app = NSApp();
        app.setActivationPolicy_(NSApplicationActivationPolicyAccessory);

        // Create menu bar status item
        create_status_item();
    }

    // Start event tap for key detection
    run_event_tap(state);
}

fn check_input_monitoring_permission() {
    unsafe {
        #[link(name = "CoreGraphics", kind = "framework")]
        extern "C" {
            fn CGPreflightListenEventAccess() -> bool;
            fn CGRequestListenEventAccess() -> bool;
        }

        if !CGPreflightListenEventAccess() {
            // Request permission - shows system dialog on first run
            CGRequestListenEventAccess();
        }
    }
}

fn show_alert(title: &str, message: &str) {
    unsafe {
        let _pool = NSAutoreleasePool::new(nil);

        let alert: id = msg_send![class!(NSAlert), new];
        let title_str = NSString::alloc(nil).init_str(title);
        let msg_str = NSString::alloc(nil).init_str(message);

        let _: () = msg_send![alert, setMessageText: title_str];
        let _: () = msg_send![alert, setInformativeText: msg_str];
        let _: () = msg_send![alert, runModal];
    }
}

/// Objective-C callback: open config.toml in default editor
extern "C" fn open_settings(_this: &Object, _cmd: Sel, _sender: id) {
    if let Some(home) = env::var_os("HOME") {
        let config_path = std::path::Path::new(&home)
            .join(".config")
            .join("fnkey")
            .join("config.toml");
        // Ensure file exists
        let _ = std::fs::create_dir_all(config_path.parent().unwrap());
        if !config_path.exists() {
            let default_toml = r#"# FnKey configuration — edit and relaunch
# api_key = "your-api-key"
# transcription_url = "https://your-server/v1/audio/transcriptions"
# polish_url = "https://your-server/v1/chat/completions"
# whisper_model = "whisper-large-v3"
# polish_model = "llama-3.3-70b-versatile"
# hotkey = "fn"
"#;
            let _ = std::fs::write(&config_path, default_toml);
        }
        unsafe {
            let workspace: id = msg_send![class!(NSWorkspace), sharedWorkspace];
            let path_str = NSString::alloc(nil).init_str(config_path.to_str().unwrap());
            let url: id = msg_send![class!(NSURL), fileURLWithPath: path_str];
            let _: bool = msg_send![workspace, openURL: url];
        }
    }
}

/// Register a helper class with an openSettings: action
unsafe fn register_menu_delegate() -> id {
    let superclass = class!(NSObject);
    let mut decl = ClassDecl::new("FnKeyMenuDelegate", superclass).unwrap();
    decl.add_method(
        sel!(openSettings:),
        open_settings as extern "C" fn(&Object, Sel, id),
    );
    let cls = decl.register();
    let obj: id = msg_send![cls, new];
    let _: () = msg_send![obj, retain];
    obj
}

unsafe fn create_status_item() {
    let status_bar: id = msg_send![class!(NSStatusBar), systemStatusBar];
    let status_item: id = msg_send![status_bar, statusItemWithLength: -1.0_f64]; // NSVariableStatusItemLength
    let _: () = msg_send![status_item, retain];
    STATUS_ITEM = status_item as *mut Object;

    // Set initial title
    let title = NSString::alloc(nil).init_str("○");
    let button: id = msg_send![status_item, button];
    let _: () = msg_send![button, setTitle: title];

    // Register menu delegate for Settings action
    let delegate = register_menu_delegate();

    // Create menu
    let menu: id = NSMenu::new(nil);

    // Settings item
    let settings_title = NSString::alloc(nil).init_str("Settings...");
    let settings_key = NSString::alloc(nil).init_str(",");
    let settings_item: id = msg_send![class!(NSMenuItem), alloc];
    let settings_item: id = msg_send![settings_item, initWithTitle: settings_title action: sel!(openSettings:) keyEquivalent: settings_key];
    let _: () = msg_send![settings_item, setTarget: delegate];
    let _: () = msg_send![menu, addItem: settings_item];

    // Separator
    let separator: id = msg_send![class!(NSMenuItem), separatorItem];
    let _: () = msg_send![menu, addItem: separator];

    // Quit item
    let quit_title = NSString::alloc(nil).init_str("Quit FnKey");
    let quit_key = NSString::alloc(nil).init_str("q");
    let quit_item: id = msg_send![class!(NSMenuItem), alloc];
    let quit_item: id = msg_send![quit_item, initWithTitle: quit_title action: sel!(terminate:) keyEquivalent: quit_key];
    let _: () = msg_send![quit_item, setTarget: NSApp()];
    let _: () = msg_send![menu, addItem: quit_item];

    let _: () = msg_send![status_item, setMenu: menu];
}

fn update_status_icon(recording: bool) {
    unsafe {
        if STATUS_ITEM.is_null() {
            return;
        }
        let title = if recording { "●" } else { "○" };
        let title_str = NSString::alloc(nil).init_str(title);
        let button: id = msg_send![STATUS_ITEM as id, button];
        let _: () = msg_send![button, setTitle: title_str];
    }
}

fn run_event_tap(state: Arc<AppState>) {
    let state_for_callback = Arc::clone(&state);
    let was_pressed = Arc::new(AtomicBool::new(false));
    let polish_latched = Arc::new(AtomicBool::new(false));

    let was_pressed_clone = Arc::clone(&was_pressed);
    let polish_latched_clone = Arc::clone(&polish_latched);

    let hotkey_flag = state.config.hotkey_flag();
    let polish_flag = state.config.polish_flag();

    let tap = CGEventTap::new(
        CGEventTapLocation::HID,
        CGEventTapPlacement::HeadInsertEventTap,
        CGEventTapOptions::ListenOnly,
        vec![CGEventType::FlagsChanged],
        move |_, _, event| {
            let flags = event.get_flags().bits();

            let key_pressed = (flags & hotkey_flag) != 0;
            let polish_held = (flags & polish_flag) != 0;

            let prev_pressed = was_pressed_clone.load(Ordering::SeqCst);

            if key_pressed && !prev_pressed {
                // Key pressed - start recording, reset polish latch
                polish_latched_clone.store(false, Ordering::SeqCst);
                start_recording(&state_for_callback);
            } else if !key_pressed && prev_pressed {
                // Key released - stop recording and transcribe
                let polish = polish_latched_clone.load(Ordering::SeqCst);
                stop_recording(&state_for_callback, polish);
            }

            // Latch polish modifier if held anytime during recording
            if key_pressed && polish_held {
                polish_latched_clone.store(true, Ordering::SeqCst);
            }

            was_pressed_clone.store(key_pressed, Ordering::SeqCst);
            None
        },
    );

    match tap {
        Ok(tap) => {
            let source = tap
                .mach_port
                .create_runloop_source(0)
                .expect("Failed to create runloop source");

            let run_loop = CFRunLoop::get_current();
            run_loop.add_source(&source, unsafe { kCFRunLoopCommonModes });

            tap.enable();

            // tap + source must stay alive while the run loop is running
            unsafe { NSApp().run(); }
        }
        Err(_) => {
            show_alert(
                "Input Monitoring Required",
                "FnKey can't detect hotkey presses.\n\nGo to System Settings → Privacy & Security → Input Monitoring, remove FnKey, re-add it, then relaunch.",
            );
            // Still run the app so the menu bar icon (Settings/Quit) is usable
            unsafe { NSApp().run(); }
        }
    }
}

fn init_audio_stream(state: &Arc<AppState>) {
    let host = cpal::default_host();
    let device = match host.default_input_device() {
        Some(d) => d,
        None => return,
    };

    // Use device's default config instead of hardcoded 16kHz
    let supported_config = match device.default_input_config() {
        Ok(c) => c,
        Err(_) => return,
    };

    let actual_sample_rate = supported_config.sample_rate().0;
    state.sample_rate.store(actual_sample_rate, Ordering::SeqCst);

    let config = cpal::StreamConfig {
        channels: 1,  // Force mono
        sample_rate: supported_config.sample_rate(),
        buffer_size: cpal::BufferSize::Default,
    };

    let buffer = Arc::clone(&state.audio_buffer);
    let stream = device
        .build_input_stream(
            &config,
            move |data: &[f32], _: &cpal::InputCallbackInfo| {
                let mut buf = buffer.lock().unwrap();
                buf.extend_from_slice(data);
            },
            |err| eprintln!("Audio error: {}", err),
            None,
        )
        .ok();

    // Store stream but don't start it yet (keeps mic indicator off)
    unsafe {
        AUDIO_STREAM = stream;
    }
}

fn start_recording(state: &Arc<AppState>) {
    // Clear buffer
    {
        let mut buffer = state.audio_buffer.lock().unwrap();
        buffer.clear();
    }

    // Create stream on first use, reuse on subsequent uses
    unsafe {
        if AUDIO_STREAM.is_none() {
            init_audio_stream(state);
        }
        if let Some(ref s) = AUDIO_STREAM {
            let _ = s.play();
        }
    }

    update_status_icon(true);
}

fn stop_recording(state: &Arc<AppState>, polish: bool) {
    // Pause the stream first (keeps data)
    unsafe {
        if let Some(ref s) = AUDIO_STREAM {
            let _ = s.pause();
        }
    }

    // Small delay to let final audio data arrive
    thread::sleep(Duration::from_millis(50));

    // Get audio buffer
    let audio_data: Vec<f32> = {
        let buffer = state.audio_buffer.lock().unwrap();
        buffer.clone()
    };

    update_status_icon(false);

    if audio_data.is_empty() {
        return;
    }

    // Transcribe in background
    let config = state.config.clone();
    let sample_rate = state.sample_rate.load(Ordering::SeqCst);
    thread::spawn(move || {
        transcribe_and_paste(audio_data, sample_rate, &config, polish);
    });
}

fn transcribe_and_paste(audio: Vec<f32>, sample_rate: u32, config: &Config, polish: bool) {
    let wav_data = match encode_wav(&audio, sample_rate) {
        Ok(data) => data,
        Err(_) => return,
    };

    let client = reqwest::blocking::Client::new();
    let form = reqwest::blocking::multipart::Form::new()
        .text("model", config.whisper_model.clone())
        .text("response_format", "text")
        .part(
            "file",
            reqwest::blocking::multipart::Part::bytes(wav_data)
                .file_name("audio.wav")
                .mime_str("audio/wav")
                .unwrap(),
        );

    let response = client
        .post(&config.transcription_url)
        .header("Authorization", format!("Bearer {}", config.api_key))
        .multipart(form)
        .timeout(Duration::from_secs(30))
        .send();

    if let Ok(resp) = response {
        if resp.status().is_success() {
            if let Ok(raw) = resp.text() {
                // Handle both plain text and JSON responses
                // Some servers (e.g. vLLM) return {"text":"..."} even with response_format=text
                let text = if raw.trim_start().starts_with('{') {
                    serde_json::from_str::<serde_json::Value>(raw.trim())
                        .ok()
                        .and_then(|v| v.get("text")?.as_str().map(String::from))
                        .unwrap_or_else(|| raw.trim().to_string())
                } else {
                    raw.trim().to_string()
                };

                if !text.is_empty() {
                    // Apply polish if requested, fallback to raw on error
                    let final_text = if polish {
                        polish_text(&text, config).unwrap_or_else(|| text.clone())
                    } else {
                        text
                    };

                    if let Ok(mut clipboard) = Clipboard::new() {
                        if clipboard.set_text(&final_text).is_ok() {
                            paste_with_cgevent();
                        }
                    }
                }
            }
        }
    }
}

fn paste_with_cgevent() {
    if let Ok(source) = CGEventSource::new(CGEventSourceStateID::HIDSystemState) {
        // Get layout-aware keycode for 'v' (works with Dvorak, Colemak, Russian, etc.)
        let v_keycode = get_paste_keycode();

        if let Ok(key_down) = CGEvent::new_keyboard_event(source.clone(), v_keycode, true) {
            if let Ok(key_up) = CGEvent::new_keyboard_event(source, v_keycode, false) {
                // Add Command modifier
                key_down.set_flags(CGEventFlags::CGEventFlagCommand);
                key_up.set_flags(CGEventFlags::CGEventFlagCommand);

                // Post events
                key_down.post(CGEventTapLocation::HID);
                key_up.post(CGEventTapLocation::HID);
            }
        }
    }
}

// ============================================================================
// LLM Polish (spoken -> written style)
// ============================================================================

#[derive(serde::Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(serde::Deserialize)]
struct ChatChoice {
    message: ChatMessage,
}

#[derive(serde::Deserialize)]
struct ChatMessage {
    content: String,
}

/// Polish transcribed text using LLM to convert spoken style to written prose.
/// Returns None on any error (caller should fall back to raw text).
fn polish_text(text: &str, config: &Config) -> Option<String> {
    let client = reqwest::blocking::Client::new();

    let body = serde_json::json!({
        "model": config.polish_model,
        "messages": [
            {
                "role": "system",
                "content": "Clean up this voice message for texting. Remove filler words (um, uh, like, you know). Fix punctuation and sentence structure. Break up run-on sentences. Keep it casual. No trailing period. Output ONLY the cleaned text - no explanations, no quotes."
            },
            {
                "role": "user",
                "content": text
            }
        ],
        "temperature": 0.2
    });

    let response = client
        .post(&config.polish_url)
        .header("Authorization", format!("Bearer {}", config.api_key))
        .header("Content-Type", "application/json")
        .json(&body)
        .timeout(Duration::from_secs(30))
        .send()
        .ok()?;

    if !response.status().is_success() {
        return None;
    }

    let chat_response: ChatResponse = response.json().ok()?;
    chat_response.choices.first().map(|c| c.message.content.clone())
}

/// Enhance audio quality before transcription.
/// Ported from Ito's audio preprocessing pipeline.
/// - Removes DC offset
/// - Applies high-pass filter (~80 Hz) to remove rumble
/// - Peak normalizes to ~-3 dBFS with capped gain
fn enhance_audio(samples: &[f32], sample_rate: u32) -> Vec<f32> {
    if samples.is_empty() {
        return Vec::new();
    }

    // 1. DC offset removal
    let mean: f32 = samples.iter().sum::<f32>() / samples.len() as f32;
    let dc_removed: Vec<f32> = samples.iter().map(|&s| s - mean).collect();

    // 2. High-pass filter (~80 Hz) - first-order filter
    let fc = 80.0_f32;
    let a = (-2.0 * std::f32::consts::PI * fc / sample_rate as f32).exp();

    let mut filtered = Vec::with_capacity(dc_removed.len());
    let mut prev_x = 0.0_f32;
    let mut prev_y = 0.0_f32;

    for &x in &dc_removed {
        let y = a * (prev_y + x - prev_x);
        filtered.push(y);
        prev_x = x;
        prev_y = y;
    }

    // 3. Peak normalization to ~-3 dBFS, cap max gain to +12 dB
    let peak = filtered.iter().map(|&s| s.abs()).fold(1.0_f32, f32::max);
    let target = 0.707_f32; // ~-3 dBFS (0.707 ≈ 10^(-3/20))
    let raw_gain = target / peak;
    let gain = raw_gain.min(4.0); // Cap at ~+12 dB

    // Apply gain only if it would make a meaningful difference
    if gain > 1.05 {
        filtered.iter().map(|&s| (s * gain).clamp(-1.0, 1.0)).collect()
    } else {
        filtered.iter().map(|&s| s.clamp(-1.0, 1.0)).collect()
    }
}

fn encode_wav(samples: &[f32], sample_rate: u32) -> Result<Vec<u8>, hound::Error> {
    // Enhance audio before encoding
    let enhanced = enhance_audio(samples, sample_rate);

    let spec = WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };

    let mut cursor = Cursor::new(Vec::new());
    {
        let mut writer = WavWriter::new(&mut cursor, spec)?;
        for &sample in &enhanced {
            let sample_i16 = (sample * 32767.0) as i16;
            writer.write_sample(sample_i16)?;
        }
        writer.finalize()?;
    }

    Ok(cursor.into_inner())
}
