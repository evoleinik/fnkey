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
use cocoa::appkit::{
    NSApp, NSApplication, NSApplicationActivationPolicyAccessory, NSBackingStoreBuffered,
    NSColor, NSMenu, NSView, NSWindow, NSWindowStyleMask,
};
use cocoa::base::{id, nil, NO};
use cocoa::foundation::{NSAutoreleasePool, NSPoint, NSRect, NSSize, NSString};
use core_foundation::runloop::{kCFRunLoopCommonModes, CFRunLoop};
use core_graphics::event::{
    CGEvent, CGEventFlags, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
    CGEventType,
};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::Stream;
use hound::{WavSpec, WavWriter};
use objc::runtime::Object;
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

// Fn key flag in CGEventFlags
const FN_KEY_FLAG: u64 = 0x800000;
// Option/Alt key flag
const OPTION_KEY_FLAG: u64 = 0x80000;

struct AppState {
    audio_buffer: Arc<Mutex<Vec<f32>>>,
    api_key: String,
    use_fn_key: AtomicBool,
    sample_rate: u32,
}

// Global status item pointer for updating from callbacks
static mut STATUS_ITEM: *mut Object = std::ptr::null_mut();
// Global audio stream (not Send, so can't be in Arc)
static mut AUDIO_STREAM: Option<Stream> = None;

fn main() {
    let api_key = env::var("GROQ_API_KEY").unwrap_or_else(|_| {
        show_alert("GROQ_API_KEY not set", "Please set GROQ_API_KEY environment variable before running FnKey.");
        std::process::exit(1);
    });

    // Check Input Monitoring permission
    if !check_input_monitoring_permission() {
        std::process::exit(1);
    }

    let state = Arc::new(AppState {
        audio_buffer: Arc::new(Mutex::new(Vec::new())),
        api_key,
        use_fn_key: AtomicBool::new(true),
        sample_rate: 16000,
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

fn check_input_monitoring_permission() -> bool {
    unsafe {
        // CGPreflightListenEventAccess and CGRequestListenEventAccess
        #[link(name = "CoreGraphics", kind = "framework")]
        extern "C" {
            fn CGPreflightListenEventAccess() -> bool;
            fn CGRequestListenEventAccess() -> bool;
        }

        if CGPreflightListenEventAccess() {
            return true;
        }

        // Request permission - this shows system dialog
        if CGRequestListenEventAccess() {
            return true;
        }

        show_alert(
            "Input Monitoring Required",
            "FnKey needs Input Monitoring permission to detect the Fn key.\n\nPlease grant access in System Settings → Privacy & Security → Input Monitoring, then relaunch FnKey.",
        );
        false
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

unsafe fn create_status_item() {
    let status_bar: id = msg_send![class!(NSStatusBar), systemStatusBar];
    let status_item: id = msg_send![status_bar, statusItemWithLength: -1.0_f64]; // NSVariableStatusItemLength
    let _: () = msg_send![status_item, retain];
    STATUS_ITEM = status_item as *mut Object;

    // Set initial title
    let title = NSString::alloc(nil).init_str("○");
    let button: id = msg_send![status_item, button];
    let _: () = msg_send![button, setTitle: title];

    // Create menu
    let menu: id = NSMenu::new(nil);

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
    let fn_detected = Arc::new(AtomicBool::new(false));
    let was_pressed = Arc::new(AtomicBool::new(false));

    let fn_detected_clone = Arc::clone(&fn_detected);
    let was_pressed_clone = Arc::clone(&was_pressed);

    let tap = CGEventTap::new(
        CGEventTapLocation::HID,
        CGEventTapPlacement::HeadInsertEventTap,
        CGEventTapOptions::ListenOnly,
        vec![CGEventType::FlagsChanged],
        move |_, _, event| {
            let flags = event.get_flags().bits();

            // Check Fn key first, then Option as fallback
            let fn_pressed = (flags & FN_KEY_FLAG) != 0;
            let option_pressed = (flags & OPTION_KEY_FLAG) != 0;

            let use_fn = state_for_callback.use_fn_key.load(Ordering::SeqCst);
            let key_pressed = if use_fn { fn_pressed } else { option_pressed };

            // Detect if Fn key works (first time detection)
            if fn_pressed && !fn_detected_clone.load(Ordering::SeqCst) {
                fn_detected_clone.store(true, Ordering::SeqCst);
            }

            let prev_pressed = was_pressed_clone.load(Ordering::SeqCst);

            // Handle key state changes
            if key_pressed && !prev_pressed {
                // Key pressed - start recording
                start_recording(&state_for_callback);
            } else if !key_pressed && prev_pressed {
                // Key released - stop recording and transcribe
                stop_recording(&state_for_callback);
            }

            was_pressed_clone.store(key_pressed, Ordering::SeqCst);
            None
        },
    )
    .expect("Failed to create event tap - check Input Monitoring permissions");

    let source = tap
        .mach_port
        .create_runloop_source(0)
        .expect("Failed to create runloop source");

    let run_loop = CFRunLoop::get_current();
    run_loop.add_source(&source, unsafe { kCFRunLoopCommonModes });

    tap.enable();

    // Fallback timer: if no Fn detected in 5 seconds, switch to Option
    let state_fallback = Arc::clone(&state);
    let fn_detected_fallback = Arc::clone(&fn_detected);
    thread::spawn(move || {
        thread::sleep(Duration::from_secs(5));
        if !fn_detected_fallback.load(Ordering::SeqCst) && state_fallback.use_fn_key.load(Ordering::SeqCst) {
            state_fallback.use_fn_key.store(false, Ordering::SeqCst);
        }
    });

    unsafe {
        NSApp().run();
    }
}

fn start_recording(state: &Arc<AppState>) {
    // Clear buffer
    {
        let mut buffer = state.audio_buffer.lock().unwrap();
        buffer.clear();
    }

    // Start audio capture
    let host = cpal::default_host();
    let device = match host.default_input_device() {
        Some(d) => d,
        None => return,
    };

    let config = cpal::StreamConfig {
        channels: 1,
        sample_rate: cpal::SampleRate(state.sample_rate),
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

    if let Some(ref s) = stream {
        let _ = s.play();
    }

    // Store stream to keep it alive (unsafe because Stream is not Sync)
    unsafe {
        AUDIO_STREAM = stream;
    }

    update_status_icon(true);
    show_indicator(true);
}

fn stop_recording(state: &Arc<AppState>) {
    // Pause the stream first (keeps data)
    unsafe {
        if let Some(ref s) = AUDIO_STREAM {
            let _ = s.pause();
        }
    }

    // Small delay to let final audio data arrive
    thread::sleep(Duration::from_millis(50));

    // Get audio buffer BEFORE dropping stream
    let audio_data: Vec<f32> = {
        let buffer = state.audio_buffer.lock().unwrap();
        buffer.clone()
    };

    // Now drop the stream (releases microphone)
    unsafe {
        AUDIO_STREAM = None;
    }

    update_status_icon(false);
    show_indicator(false);

    if audio_data.is_empty() {
        return;
    }

    // Transcribe in background
    let api_key = state.api_key.clone();
    let sample_rate = state.sample_rate;
    thread::spawn(move || {
        transcribe_and_paste(audio_data, sample_rate, &api_key);
    });
}

fn transcribe_and_paste(audio: Vec<f32>, sample_rate: u32, api_key: &str) {
    let wav_data = match encode_wav(&audio, sample_rate) {
        Ok(data) => data,
        Err(_) => return,
    };

    let client = reqwest::blocking::Client::new();
    let form = reqwest::blocking::multipart::Form::new()
        .text("model", "whisper-large-v3-turbo")
        .text("response_format", "text")
        .part(
            "file",
            reqwest::blocking::multipart::Part::bytes(wav_data)
                .file_name("audio.wav")
                .mime_str("audio/wav")
                .unwrap(),
        );

    let response = client
        .post("https://api.groq.com/openai/v1/audio/transcriptions")
        .header("Authorization", format!("Bearer {}", api_key))
        .multipart(form)
        .timeout(Duration::from_secs(30))
        .send();

    if let Ok(resp) = response {
        if resp.status().is_success() {
            if let Ok(text) = resp.text() {
                let text = text.trim();
                if !text.is_empty() {
                    if let Ok(mut clipboard) = Clipboard::new() {
                        if clipboard.set_text(text).is_ok() {
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

fn encode_wav(samples: &[f32], sample_rate: u32) -> Result<Vec<u8>, hound::Error> {
    let spec = WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };

    let mut cursor = Cursor::new(Vec::new());
    {
        let mut writer = WavWriter::new(&mut cursor, spec)?;
        for &sample in samples {
            let sample_i16 = (sample * 32767.0) as i16;
            writer.write_sample(sample_i16)?;
        }
        writer.finalize()?;
    }

    Ok(cursor.into_inner())
}

fn show_indicator(show: bool) {
    unsafe {
        let _pool = NSAutoreleasePool::new(nil);

        static mut WINDOW: *mut Object = std::ptr::null_mut();

        if show {
            if WINDOW.is_null() {
                // Create window
                let screen_frame: NSRect = msg_send![cocoa::appkit::NSScreen::mainScreen(nil), frame];
                let window_size = 20.0;
                let margin = 10.0;

                let frame = NSRect::new(
                    NSPoint::new(
                        screen_frame.size.width - window_size - margin,
                        screen_frame.size.height - window_size - margin - 25.0,
                    ),
                    NSSize::new(window_size, window_size),
                );

                let window: id = NSWindow::alloc(nil).initWithContentRect_styleMask_backing_defer_(
                    frame,
                    NSWindowStyleMask::NSBorderlessWindowMask,
                    NSBackingStoreBuffered,
                    NO,
                );

                window.setLevel_(25);
                window.setOpaque_(NO);
                window.setBackgroundColor_(NSColor::clearColor(nil));
                window.setIgnoresMouseEvents_(true);

                // Create red circle view
                let view: id = NSView::alloc(nil).initWithFrame_(NSRect::new(
                    NSPoint::new(0.0, 0.0),
                    NSSize::new(window_size, window_size),
                ));
                view.setWantsLayer(true);
                let layer: id = msg_send![view, layer];
                let red: id = NSColor::colorWithRed_green_blue_alpha_(nil, 1.0, 0.2, 0.2, 1.0);
                let cg_color: id = msg_send![red, CGColor];
                let _: () = msg_send![layer, setBackgroundColor: cg_color];
                let _: () = msg_send![layer, setCornerRadius: window_size / 2.0];

                window.setContentView_(view);
                WINDOW = window as *mut Object;
            }

            let _: () = msg_send![WINDOW as id, orderFront: nil];
        } else if !WINDOW.is_null() {
            let _: () = msg_send![WINDOW as id, orderOut: nil];
        }
    }
}
