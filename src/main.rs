//! fnkey.ai - Hold Fn key, speak, paste transcribed text
//!
//! Streams audio to Deepgram via WebSocket for real-time transcription.
//! Falls back to Groq Whisper batch API if Deepgram key not configured.
//!
//! Config files (~/.config/fnkey/):
//!   deepgram_key  - Deepgram API key (streaming, preferred)
//!   deepgram_host - Deepgram server (default api.deepgram.com, in the US).
//!                   Regions: api.eu.deepgram.com, api.in.deepgram.com, api.au.deepgram.com
//!   api_key       - Groq API key (batch fallback + polish)
//!   live_paste    - "1" pastes each finished phrase while Fn is held. Menu: "Paste while speaking"

use std::collections::HashMap;
use std::env;
use std::ffi::c_void;
use std::io::Cursor;
use std::io::Write as IoWrite;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use arboard::Clipboard;
use cocoa::appkit::{NSApp, NSApplication, NSApplicationActivationPolicyAccessory, NSMenu};
use cocoa::base::{id, nil};
use cocoa::foundation::{NSAutoreleasePool, NSString};
use core_foundation::runloop::{kCFRunLoopCommonModes, CFRunLoop};
use core_graphics::event::{
    CGEvent, CGEventFlags, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
    CGEventType, EventField,
};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::Stream;
use hound::{WavSpec, WavWriter};
use objc::declare::ClassDecl;
use objc::runtime::{Class, Object, Sel};
use objc::{class, msg_send, sel, sel_impl};
use tungstenite::protocol::Message;

// ============================================================================
// Keyboard layout detection (for non-Latin layouts like Russian)
// ============================================================================

static KEYCODE_MAP: OnceLock<HashMap<char, u16>> = OnceLock::new();

#[repr(C)]
struct UCKeyboardLayout {
    _opaque: [u8; 0],
}

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
        let layout_data: core_foundation::data::CFData =
            core_foundation::base::TCFType::wrap_under_get_rule(layout_data_ref as *const _);
        let layout_ptr = layout_data.bytes().as_ptr() as *const UCKeyboardLayout;
        let kbd_type = LMGetKbdType();
        for keycode in 0u16..128 {
            let mut dead_key_state: u32 = 0;
            let mut char_buf: [u16; 4] = [0; 4];
            let mut actual_len: usize = 0;
            let result = UCKeyTranslate(
                layout_ptr, keycode, KUC_KEY_ACTION_DISPLAY, 0, kbd_type, 0,
                &mut dead_key_state, char_buf.len(), &mut actual_len, char_buf.as_mut_ptr(),
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

fn get_paste_keycode() -> u16 {
    let map = KEYCODE_MAP.get_or_init(build_char_to_keycode_map);
    map.get(&'v').copied().unwrap_or(QWERTY_V_KEYCODE)
}

// ============================================================================
// Main application
// ============================================================================

const FN_KEY_FLAG: u64 = 0x800000;
const OPTION_KEY_FLAG: u64 = 0x80000;
/// Shift, Control, Option, Command and Fn. Caps Lock is left out because it stays on.
const MODIFIER_FLAGS: u64 = 0x20000 | 0x40000 | 0x80000 | 0x100000 | 0x800000;
const DEEPGRAM_SAMPLE_RATE: u32 = 16000;
const DEFAULT_DEEPGRAM_HOST: &str = "api.deepgram.com";
/// A press shorter than this is a tap, not dictation.
const MIN_HOLD: Duration = Duration::from_millis(300);
/// How long to wait for Deepgram's text after the key is released.
const DEEPGRAM_WAIT: Duration = Duration::from_secs(10);
/// Least time between live pastes, so the app reads the clipboard before it changes again.
const PASTE_GAP: Duration = Duration::from_millis(200);
/// Marks key events FnKey posts itself, so its own listener ignores them.
const OWN_EVENT_TAG: i64 = 0x464E_4B59;

/// Who puts text into the app for one press. Set once, so text is never pasted twice.
const OWNER_NONE: u8 = 0;
const OWNER_LIVE: u8 = 1;
const OWNER_GROQ: u8 = 2;

/// Messages sent from audio callback / event tap to the WebSocket thread
enum WsCommand {
    /// Raw PCM audio chunk (already resampled to 16kHz, i16 LE bytes)
    Audio(Vec<u8>),
    /// Stop streaming, finalize, paste result
    Stop,
    /// The press was a tap or a shortcut: close quietly and report nothing
    Cancel,
}

/// Progress of one Deepgram stream, shared with the thread that waits for its text
struct DgStats {
    host: String,
    pressed: Instant,
    /// Milliseconds from key press to an open WebSocket, u64::MAX until then
    connect_ms: AtomicU64,
    chunks: AtomicU32,
    bytes: AtomicUsize,
    msgs: AtomicU32,
    /// OWNER_NONE until live paste or the Groq fallback claims the output
    owner: AtomicU8,
}

impl DgStats {
    fn new(host: String, pressed: Instant) -> Self {
        DgStats {
            host,
            pressed,
            connect_ms: AtomicU64::new(u64::MAX),
            chunks: AtomicU32::new(0),
            bytes: AtomicUsize::new(0),
            msgs: AtomicU32::new(0),
            owner: AtomicU8::new(OWNER_NONE),
        }
    }

    /// True if live paste may paste: it already owns the output or just claimed it
    fn claim_live(&self) -> bool {
        match self.owner.compare_exchange(OWNER_NONE, OWNER_LIVE, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => true,
            Err(current) => current == OWNER_LIVE,
        }
    }

    /// True if Groq may paste: nothing has been pasted live
    fn claim_groq(&self) -> bool {
        self.owner
            .compare_exchange(OWNER_NONE, OWNER_GROQ, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    fn live_pasted(&self) -> bool {
        self.owner.load(Ordering::SeqCst) == OWNER_LIVE
    }
}

/// Final phrases waiting to be pasted while Fn is held
#[derive(Default)]
struct LivePaste {
    pending: String,
    pasted_any: bool,
    last: Option<Instant>,
}

impl LivePaste {
    fn add(&mut self, phrase: &str) {
        if self.pasted_any || !self.pending.is_empty() {
            self.pending.push(' ');
        }
        self.pending.push_str(phrase);
    }

    /// The waiting text, if any, once PASTE_GAP has passed since the last paste
    fn take_due(&mut self, now: Instant) -> Option<String> {
        if self.pending.is_empty() || self.last.map_or(false, |t| now.duration_since(t) < PASTE_GAP) {
            return None;
        }
        self.last = Some(now);
        self.pasted_any = true;
        Some(std::mem::take(&mut self.pending))
    }

    /// Everything still waiting, after sleeping out the gap
    fn take_rest(&mut self) -> Option<String> {
        if let Some(t) = self.last {
            let since = t.elapsed();
            if since < PASTE_GAP {
                thread::sleep(PASTE_GAP - since);
            }
        }
        self.take_due(Instant::now())
    }
}

/// Deepgram region from the config file: a bare hostname, though a pasted wss:// URL works too
fn parse_deepgram_host(raw: Option<String>) -> String {
    raw.map(|h| {
        h.trim_start_matches("wss://")
            .trim_start_matches("https://")
            .trim_end_matches('/')
            .to_string()
    })
    .filter(|h| !h.is_empty())
    .unwrap_or_else(|| DEFAULT_DEEPGRAM_HOST.to_string())
}

/// A press is dictation only when it is long enough and no other key was used
fn is_dictation(held: Duration, combo: bool) -> bool {
    held >= MIN_HOLD && !combo
}

/// True when a modifier other than the trigger is down (fn+Shift, Option+Command)
fn other_modifier_held(flags: u64, trigger_flag: u64) -> bool {
    flags & MODIFIER_FLAGS & !trigger_flag != 0
}

impl DgStats {
    fn summary(&self) -> String {
        let connect = match self.connect_ms.load(Ordering::SeqCst) {
            u64::MAX => "not connected".to_string(),
            ms => format!("connected in {}ms", ms),
        };
        format!(
            "{} {}, {}ms since press, {} chunks/{}KB sent, {} msgs recv'd",
            self.host,
            connect,
            self.pressed.elapsed().as_millis(),
            self.chunks.load(Ordering::SeqCst),
            self.bytes.load(Ordering::SeqCst) / 1024,
            self.msgs.load(Ordering::SeqCst)
        )
    }
}

/// Result from Deepgram streaming thread
enum DgResult {
    /// Deepgram answered. Empty means it heard no speech.
    Ok(String),
    /// Connection or streaming error
    Err(String),
}

/// What to do once Deepgram has answered, or failed to
enum Next {
    Paste(String),
    /// The text was already pasted live: only the optional Return is left
    ReturnOnly,
    /// Deepgram heard no speech. Whisper on the same audio invents stock
    /// phrases like "Thank you for watching!", so nothing is pasted.
    /// Also used when live paste already put part of the text in the app.
    Nothing,
    Groq,
}

/// Groq is only for when Deepgram could not answer (no answer means a timeout)
/// and nothing was pasted live, which Groq would paste a second time.
fn after_deepgram(answer: Option<DgResult>, live_pasted: bool) -> Next {
    match answer {
        Some(DgResult::Ok(text)) if text.is_empty() => Next::Nothing,
        Some(DgResult::Ok(_)) if live_pasted => Next::ReturnOnly,
        Some(DgResult::Ok(text)) => Next::Paste(text),
        Some(DgResult::Err(_)) | None if live_pasted => Next::Nothing,
        Some(DgResult::Err(_)) | None => Next::Groq,
    }
}

struct AppState {
    audio_buffer: Arc<Mutex<Vec<f32>>>,
    /// Shadow buffer: keeps all audio for Groq fallback if Deepgram fails
    shadow_buffer: Arc<Mutex<Vec<f32>>>,
    groq_key: Option<String>,
    deepgram_key: Option<String>,
    keywords: Vec<String>,
    use_fn_key: AtomicBool,
    sample_rate: std::sync::atomic::AtomicU32,
    /// Channel to send commands to the active WebSocket thread
    ws_tx: Mutex<Option<mpsc::Sender<WsCommand>>>,
    /// Channel to receive result from Deepgram thread
    dg_result_rx: Mutex<Option<mpsc::Receiver<DgResult>>>,
    /// Whether a Deepgram stream is active
    ws_active: Arc<AtomicBool>,
    deepgram_host: String,
    /// When the trigger key went down, None while idle
    pressed_at: Mutex<Option<Instant>>,
    /// Set when another key is used during the press (fn+Delete, fn+arrow)
    combo: AtomicBool,
    /// Progress of the active Deepgram stream
    dg_stats: Mutex<Option<Arc<DgStats>>>,
}

static mut STATUS_ITEM: *mut Object = std::ptr::null_mut();
static mut AUDIO_STREAM: Option<Stream> = None;
static AUTO_RETURN: AtomicBool = AtomicBool::new(false);
static mut AUTO_RETURN_ITEM: *mut Object = std::ptr::null_mut();
/// Paste each final phrase while Fn is held. Read on every press, so the menu toggle applies at once.
static LIVE_PASTE: AtomicBool = AtomicBool::new(false);
static mut LIVE_PASTE_ITEM: *mut Object = std::ptr::null_mut();

fn read_config_file(name: &str) -> Option<String> {
    let home = env::var_os("HOME")?;
    let path = std::path::Path::new(&home).join(".config").join("fnkey").join(name);
    let key = std::fs::read_to_string(&path).ok()?;
    let key = key.trim();
    if key.is_empty() { None } else { Some(key.to_string()) }
}

fn log_error(msg: &str) {
    if let Some(home) = env::var_os("HOME") {
        let path = std::path::Path::new(&home).join(".config").join("fnkey").join("error.log");
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
            let _ = writeln!(f, "[{}] {}", now, msg);
        }
    }
    eprintln!("[fnkey] {}", msg);
}

fn show_notification(msg: &str) {
    unsafe {
        let _pool = NSAutoreleasePool::new(nil);
        let center: id = msg_send![class!(NSUserNotificationCenter), defaultUserNotificationCenter];
        let notification: id = msg_send![class!(NSUserNotification), new];
        let title = NSString::alloc(nil).init_str("FnKey");
        let body = NSString::alloc(nil).init_str(msg);
        let _: () = msg_send![notification, setTitle: title];
        let _: () = msg_send![notification, setInformativeText: body];
        let _: () = msg_send![center, deliverNotification: notification];
    }
}

fn main() {
    let deepgram_key = read_config_file("deepgram_key")
        .or_else(|| env::var("DEEPGRAM_API_KEY").ok());
    let groq_key = read_config_file("api_key")
        .or_else(|| env::var("GROQ_API_KEY").ok());

    if deepgram_key.is_none() && groq_key.is_none() {
        show_alert(
            "No API key configured",
            "Please create ~/.config/fnkey/deepgram_key with your Deepgram API key.\n\n\
             Example:\n  mkdir -p ~/.config/fnkey\n  echo 'your_key' > ~/.config/fnkey/deepgram_key\n\n\
             Get a key at https://console.deepgram.com (includes $200 free credit)"
        );
        std::process::exit(1);
    }

    if !check_input_monitoring_permission() {
        std::process::exit(1);
    }

    // Load menu settings
    AUTO_RETURN.store(read_config_file("auto_return").map_or(false, |v| v == "1"), Ordering::SeqCst);
    LIVE_PASTE.store(read_config_file("live_paste").map_or(false, |v| v == "1"), Ordering::SeqCst);

    // Load custom keywords for transcription accuracy
    let keywords = read_config_file("keywords")
        .map(|content| {
            content
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .collect()
        })
        .unwrap_or_default();

    let deepgram_host = parse_deepgram_host(read_config_file("deepgram_host"));

    let state = Arc::new(AppState {
        audio_buffer: Arc::new(Mutex::new(Vec::new())),
        shadow_buffer: Arc::new(Mutex::new(Vec::new())),
        groq_key,
        deepgram_key,
        keywords,
        use_fn_key: AtomicBool::new(true),
        sample_rate: std::sync::atomic::AtomicU32::new(48000),
        ws_tx: Mutex::new(None),
        dg_result_rx: Mutex::new(None),
        ws_active: Arc::new(AtomicBool::new(false)),
        deepgram_host,
        pressed_at: Mutex::new(None),
        combo: AtomicBool::new(false),
        dg_stats: Mutex::new(None),
    });

    unsafe {
        let _pool = NSAutoreleasePool::new(nil);
        let app = NSApp();
        app.setActivationPolicy_(NSApplicationActivationPolicyAccessory);
        create_status_item();
    }

    run_event_tap(state);
}

// ============================================================================
// Deepgram streaming — runs entirely on a background thread
// ============================================================================

/// Spawn a background thread that:
/// 1. Connects WebSocket to Deepgram
/// 2. Reads audio from rx channel, sends to WS
/// 3. Reads transcripts from WS
/// 4. On Stop command: closes stream, pastes result
fn spawn_deepgram_thread(
    key: String,
    rx: mpsc::Receiver<WsCommand>,
    keywords: Vec<String>,
    result_tx: mpsc::Sender<DgResult>,
    stats: Arc<DgStats>,
    live: bool,
) {
    thread::spawn(move || {
        let mut live = if live { Some(LivePaste::default()) } else { None };
        let mut url = format!(
            "wss://{}/v1/listen?\
             encoding=linear16&sample_rate={}&channels=1&\
             interim_results=true&endpointing=300&\
             punctuate=true&smart_format=true&model=nova-3&\
             language=multi",
            stats.host, DEEPGRAM_SAMPLE_RATE
        );
        for kw in &keywords {
            url.push_str(&format!("&keyterm={}", urlencoding::encode(kw)));
        }
        let request = tungstenite::http::Request::builder()
            .uri(&url)
            .header("Authorization", format!("Token {}", key))
            .header("Host", stats.host.as_str())
            .header("Connection", "Upgrade")
            .header("Upgrade", "websocket")
            .header("Sec-WebSocket-Version", "13")
            .header("Sec-WebSocket-Key", tungstenite::handshake::client::generate_key())
            .body(())
            .unwrap();

        let (mut ws, _response) = match tungstenite::connect(request) {
            Ok(pair) => pair,
            Err(e) => {
                let msg = format!("Deepgram connect failed: {}", e);
                log_error(&msg);
                let _ = result_tx.send(DgResult::Err(msg));
                // Drain remaining commands so senders don't block
                for _ in rx.iter() {}
                return;
            }
        };
        stats.connect_ms.store(stats.pressed.elapsed().as_millis() as u64, Ordering::SeqCst);

        // Set WebSocket to non-blocking so we can interleave send/recv
        if let tungstenite::stream::MaybeTlsStream::NativeTls(ref s) = ws.get_ref() {
            let _ = s.get_ref().set_nonblocking(true);
        } else if let tungstenite::stream::MaybeTlsStream::Plain(ref s) = ws.get_ref() {
            let _ = s.set_nonblocking(true);
        }

        let mut transcript = String::new();
        let mut running = true;
        let mut ws_error: Option<String> = None;
        let mut got_stop = false;

        while running {
            // 1. Check for commands from audio callback / event tap
            match rx.try_recv() {
                Ok(WsCommand::Audio(bytes)) => {
                    let len = bytes.len();
                    if let Err(e) = ws.send(Message::Binary(bytes)) {
                        // EAGAIN/WouldBlock = send buffer momentarily full, skip chunk
                        if let tungstenite::Error::Io(ref io_err) = e {
                            if io_err.kind() == std::io::ErrorKind::WouldBlock {
                                continue;
                            }
                        }
                        let msg = format!("Deepgram send error: {}: {}", e, stats.summary());
                        log_error(&msg);
                        ws_error = Some(msg);
                        running = false;
                    } else {
                        stats.chunks.fetch_add(1, Ordering::SeqCst);
                        stats.bytes.fetch_add(len, Ordering::SeqCst);
                    }
                }
                Ok(WsCommand::Cancel) => {
                    let _ = ws.close(None);
                    return;
                }
                Ok(WsCommand::Stop) => {
                    got_stop = true;
                    // Send CloseStream, then drain remaining transcripts
                    let close_msg = serde_json::json!({"type": "CloseStream"});
                    let _ = ws.send(Message::Text(close_msg.to_string()));

                    // Switch to blocking for final drain
                    if let tungstenite::stream::MaybeTlsStream::NativeTls(ref s) = ws.get_ref() {
                        let _ = s.get_ref().set_nonblocking(false);
                        let _ = s.get_ref().set_read_timeout(Some(Duration::from_secs(3)));
                    } else if let tungstenite::stream::MaybeTlsStream::Plain(ref s) = ws.get_ref() {
                        let _ = s.set_nonblocking(false);
                        let _ = s.set_read_timeout(Some(Duration::from_secs(3)));
                    }

                    // Read remaining final transcripts
                    loop {
                        match ws.read() {
                            Ok(Message::Text(text)) => {
                                stats.msgs.fetch_add(1, Ordering::SeqCst);
                                let phrase = accumulate_transcript(&text, &mut transcript);
                                if let (Some(lp), Some(p)) = (live.as_mut(), phrase) {
                                    lp.add(&p);
                                }
                            }
                            Ok(Message::Close(frame)) => {
                                if let Some(ref f) = frame {
                                    if f.code != tungstenite::protocol::frame::coding::CloseCode::Normal {
                                        log_error(&format!("Deepgram close frame: code={}, reason='{}'", f.code, f.reason));
                                    }
                                }
                                break;
                            }
                            Err(_) => break,
                            _ => {}
                        }
                    }
                    let _ = ws.close(None);
                    running = false;
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    ws_error = Some(format!("Deepgram: channel disconnected: {}", stats.summary()));
                    running = false;
                }
            }

            // 2. Try to read transcript from WebSocket (non-blocking)
            match ws.read() {
                Ok(Message::Text(text)) => {
                    stats.msgs.fetch_add(1, Ordering::SeqCst);
                    let phrase = accumulate_transcript(&text, &mut transcript);
                    if let (Some(lp), Some(p)) = (live.as_mut(), phrase) {
                        lp.add(&p);
                    }
                }
                Ok(Message::Close(frame)) => {
                    if !got_stop {
                        // Server closed before we sent Stop — unexpected
                        let reason = frame.as_ref()
                            .map(|f| format!("code={}, reason='{}'", f.code, f.reason))
                            .unwrap_or_else(|| "no close frame".to_string());
                        let msg = format!("Deepgram server closed early: {}: {}", reason, stats.summary());
                        log_error(&msg);
                        ws_error = Some(msg);
                    }
                    running = false;
                }
                Err(tungstenite::Error::Io(ref e))
                    if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(tungstenite::Error::ConnectionClosed) => {
                    if !got_stop {
                        let msg = format!("Deepgram connection dropped: {}", stats.summary());
                        log_error(&msg);
                        ws_error = Some(msg);
                    }
                    running = false;
                }
                Err(e) => {
                    let msg = format!("Deepgram WebSocket error: {}: {}", e, stats.summary());
                    log_error(&msg);
                    ws_error = Some(msg);
                    running = false;
                }
                _ => {}
            }

            // Live paste: put finished phrases in the app while Fn is still held
            if let Some(lp) = live.as_mut() {
                if let Some(text) = lp.take_due(Instant::now()) {
                    if stats.claim_live() {
                        paste_text(&text);
                    }
                }
            }

            // Small sleep to avoid busy-spinning
            thread::sleep(Duration::from_millis(5));
        }

        // Live paste: the phrases that arrived after release go in before the result is sent
        if let Some(lp) = live.as_mut() {
            if let Some(text) = lp.take_rest() {
                if stats.claim_live() {
                    paste_text(&text);
                }
            }
        }

        // Send result back — if we got a transcript, use it even if WS errored
        let text = transcript.trim().to_string();
        if !text.is_empty() {
            if result_tx.send(DgResult::Ok(text)).is_err() {
                // The waiter already gave up, so Deepgram was slow rather than wrong
                let fate = if stats.live_pasted() { "was pasted live" } else { "was dropped" };
                log_error(&format!(
                    "Deepgram text arrived after the {}s wait and {}: {}",
                    DEEPGRAM_WAIT.as_secs(), fate, stats.summary()
                ));
            }
        } else if let Some(err) = ws_error {
            let _ = result_tx.send(DgResult::Err(err));
        } else {
            log_error(&format!("Deepgram heard no speech: {}", stats.summary()));
            let _ = result_tx.send(DgResult::Ok(String::new()));
        }
    });
}

/// Adds a final phrase to the transcript and returns it. Drafts return None.
fn accumulate_transcript(json_text: &str, transcript: &mut String) -> Option<String> {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(json_text) {
        let is_final = v.get("is_final").and_then(|f| f.as_bool()).unwrap_or(false);
        let text = v.get("channel")
            .and_then(|c| c.get("alternatives"))
            .and_then(|a| a.get(0))
            .and_then(|a| a.get("transcript"))
            .and_then(|t| t.as_str())
            .unwrap_or("");
        if is_final && !text.is_empty() {
            if !transcript.is_empty() {
                transcript.push(' ');
            }
            transcript.push_str(text);
            return Some(text.to_string());
        }
    }
    None
}

/// Simple linear resampling
fn resample(samples: &[f32], from_rate: u32, to_rate: u32) -> Vec<f32> {
    if from_rate == to_rate || samples.is_empty() {
        return samples.to_vec();
    }
    let ratio = from_rate as f64 / to_rate as f64;
    let out_len = (samples.len() as f64 / ratio) as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let src_idx = i as f64 * ratio;
        let idx = src_idx as usize;
        let frac = src_idx - idx as f64;
        let s = if idx + 1 < samples.len() {
            samples[idx] as f64 * (1.0 - frac) + samples[idx + 1] as f64 * frac
        } else {
            samples[idx.min(samples.len() - 1)] as f64
        };
        out.push(s as f32);
    }
    out
}

// ============================================================================
// Groq batch fallback
// ============================================================================

fn transcribe_groq(audio: Vec<f32>, sample_rate: u32, api_key: &str, keywords: &[String]) -> Option<String> {
    let wav_data = encode_wav(&audio, sample_rate).ok()?;
    let client = reqwest::blocking::Client::new();
    let mut form = reqwest::blocking::multipart::Form::new()
        .text("model", "whisper-large-v3")
        .text("response_format", "text")
        .part(
            "file",
            reqwest::blocking::multipart::Part::bytes(wav_data)
                .file_name("audio.wav")
                .mime_str("audio/wav")
                .unwrap(),
        );
    if !keywords.is_empty() {
        form = form.text("prompt", keywords.join(", "));
    }
    let response = client
        .post("https://api.groq.com/openai/v1/audio/transcriptions")
        .header("Authorization", format!("Bearer {}", api_key))
        .multipart(form)
        .timeout(Duration::from_secs(30))
        .send()
        .ok()?;
    if !response.status().is_success() { return None; }
    let text = response.text().ok()?;
    let text = text.trim();
    if text.is_empty() { None } else { Some(text.to_string()) }
}

// ============================================================================
// Recording lifecycle — all non-blocking from event tap's perspective
// ============================================================================

fn init_audio_stream(state: &Arc<AppState>) {
    let host = cpal::default_host();
    let device = match host.default_input_device() {
        Some(d) => d,
        None => return,
    };
    let supported_config = match device.default_input_config() {
        Ok(c) => c,
        Err(_) => return,
    };
    let actual_sample_rate = supported_config.sample_rate().0;
    state.sample_rate.store(actual_sample_rate, Ordering::SeqCst);

    let config = cpal::StreamConfig {
        channels: 1,
        sample_rate: supported_config.sample_rate(),
        buffer_size: cpal::BufferSize::Default,
    };

    let buffer = Arc::clone(&state.audio_buffer);
    let shadow = Arc::clone(&state.shadow_buffer);

    let stream = device
        .build_input_stream(
            &config,
            move |data: &[f32], _: &cpal::InputCallbackInfo| {
                let mut buf = buffer.lock().unwrap();
                buf.extend_from_slice(data);
                let mut shd = shadow.lock().unwrap();
                shd.extend_from_slice(data);
            },
            |err| {
                log_error(&format!("Audio error: {}", err));
            },
            None,
        )
        .ok();

    unsafe {
        AUDIO_STREAM = stream;
    }
}

/// Called from event tap — must be non-blocking
fn start_recording(state: &Arc<AppState>) {
    let pressed = Instant::now();
    *state.pressed_at.lock().unwrap() = Some(pressed);
    state.combo.store(false, Ordering::SeqCst);

    // Clear buffers
    {
        let mut buffer = state.audio_buffer.lock().unwrap();
        buffer.clear();
    }
    {
        let mut shadow = state.shadow_buffer.lock().unwrap();
        shadow.clear();
    }

    // Init audio stream on first use
    unsafe {
        if AUDIO_STREAM.is_none() {
            init_audio_stream(state);
        }
        if let Some(ref s) = AUDIO_STREAM {
            let _ = s.play();
        }
    }

    update_status_icon(true);

    // Spawn Deepgram streaming in background (non-blocking)
    if let Some(ref dg_key) = state.deepgram_key {
        let (tx, rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        {
            let mut ws_tx = state.ws_tx.lock().unwrap();
            *ws_tx = Some(tx);
        }
        {
            let mut drx = state.dg_result_rx.lock().unwrap();
            *drx = Some(result_rx);
        }
        state.ws_active.store(true, Ordering::SeqCst);

        let key = dg_key.clone();
        let kw = state.keywords.clone();
        let stats = Arc::new(DgStats::new(state.deepgram_host.clone(), pressed));
        *state.dg_stats.lock().unwrap() = Some(Arc::clone(&stats));
        spawn_deepgram_thread(key, rx, kw, result_tx, stats, LIVE_PASTE.load(Ordering::SeqCst));

        // Spawn audio forwarder: drains buffer, resamples, sends to WS thread
        let buffer = Arc::clone(&state.audio_buffer);
        let ws_active = Arc::clone(&state.ws_active);
        let ws_tx_ref = state.ws_tx.lock().unwrap().clone();
        if let Some(tx) = ws_tx_ref {
            let sr = state.sample_rate.load(Ordering::SeqCst);
            thread::spawn(move || {
                while ws_active.load(Ordering::SeqCst) {
                    let chunk: Vec<f32> = {
                        let mut buf = buffer.lock().unwrap();
                        if buf.is_empty() {
                            drop(buf);
                            thread::sleep(Duration::from_millis(20));
                            continue;
                        }
                        buf.drain(..).collect()
                    };

                    let resampled = if sr != DEEPGRAM_SAMPLE_RATE {
                        resample(&chunk, sr, DEEPGRAM_SAMPLE_RATE)
                    } else {
                        chunk
                    };

                    let mut bytes = Vec::with_capacity(resampled.len() * 2);
                    for &sample in &resampled {
                        let s = (sample * 32767.0).clamp(-32768.0, 32767.0) as i16;
                        bytes.extend_from_slice(&s.to_le_bytes());
                    }

                    if tx.send(WsCommand::Audio(bytes)).is_err() {
                        break;
                    }
                    thread::sleep(Duration::from_millis(20));
                }
            });
        }
    }
}

/// Called from event tap — must be non-blocking
fn stop_recording(state: &Arc<AppState>) {
    // Pause audio
    unsafe {
        if let Some(ref s) = AUDIO_STREAM {
            let _ = s.pause();
        }
    }

    update_status_icon(false);

    // A tap or a shortcut like fn+Delete is not dictation: drop it without a word
    let held = state.pressed_at.lock().unwrap().take().map_or(Duration::ZERO, |t| t.elapsed());
    if !is_dictation(held, state.combo.load(Ordering::SeqCst)) {
        state.ws_active.store(false, Ordering::SeqCst);
        if let Some(tx) = state.ws_tx.lock().unwrap().take() {
            let _ = tx.send(WsCommand::Cancel);
        }
        state.dg_result_rx.lock().unwrap().take();
        state.dg_stats.lock().unwrap().take();
        return;
    }

    let was_streaming = state.ws_active.load(Ordering::SeqCst);

    // Grab shadow buffer for potential Groq fallback
    let shadow_audio: Vec<f32> = {
        let shd = state.shadow_buffer.lock().unwrap();
        shd.clone()
    };
    let sample_rate = state.sample_rate.load(Ordering::SeqCst);
    let groq_key = state.groq_key.clone();
    let keywords = state.keywords.clone();

    if was_streaming {
        // Signal the WS thread to stop
        state.ws_active.store(false, Ordering::SeqCst);

        let ws_tx = state.ws_tx.lock().unwrap().take();
        if let Some(tx) = ws_tx {
            // Drain any remaining audio in the buffer
            let remaining: Vec<f32> = {
                let mut buf = state.audio_buffer.lock().unwrap();
                buf.drain(..).collect()
            };
            if !remaining.is_empty() {
                let sr = state.sample_rate.load(Ordering::SeqCst);
                let resampled = if sr != DEEPGRAM_SAMPLE_RATE {
                    resample(&remaining, sr, DEEPGRAM_SAMPLE_RATE)
                } else {
                    remaining
                };
                let mut bytes = Vec::with_capacity(resampled.len() * 2);
                for &sample in &resampled {
                    let s = (sample * 32767.0).clamp(-32768.0, 32767.0) as i16;
                    bytes.extend_from_slice(&s.to_le_bytes());
                }
                let _ = tx.send(WsCommand::Audio(bytes));
            }
            // Tell WS thread to finalize
            let _ = tx.send(WsCommand::Stop);
        }

        // Wait for Deepgram result in background, fallback to Groq if needed
        let result_rx = state.dg_result_rx.lock().unwrap().take();
        let stats = state.dg_stats.lock().unwrap().take();
        thread::spawn(move || {
            let answer = match result_rx.map(|rx| rx.recv_timeout(DEEPGRAM_WAIT)) {
                Some(Ok(answer)) => Some(answer),
                Some(Err(_)) => {
                    let detail = stats.as_ref().map_or(String::new(), |s| s.summary());
                    log_error(&format!(
                        "Deepgram: no result after waiting {}s: {}",
                        DEEPGRAM_WAIT.as_secs(), detail
                    ));
                    None
                }
                None => None,
            };
            if let Some(DgResult::Err(ref e)) = answer {
                log_error(&format!("Deepgram failed: {}", e));
            }

            let live_pasted = stats.as_ref().map_or(false, |s| s.live_pasted());
            match after_deepgram(answer, live_pasted) {
                Next::Paste(text) => {
                    if paste_text(&text) {
                        maybe_return();
                    }
                }
                Next::ReturnOnly => maybe_return(),
                Next::Nothing => {}
                Next::Groq => {
                    // A live paste can land between the check above and here. The claim settles it.
                    if !stats.as_ref().map_or(true, |s| s.claim_groq()) {
                        return;
                    }
                    if let Some(ref key) = groq_key {
                        if !shadow_audio.is_empty() {
                            show_notification("Deepgram failed, using Groq fallback");
                            log_error("Falling back to Groq Whisper");
                            if let Some(text) = transcribe_groq(shadow_audio, sample_rate, key, &keywords) {
                                if paste_text(&text) {
                                    maybe_return();
                                }
                            } else {
                                show_notification("Transcription failed (both backends)");
                                log_error("Groq fallback also failed");
                            }
                        }
                    } else if !shadow_audio.is_empty() {
                        show_notification("Deepgram failed, no Groq key for fallback");
                        log_error("Deepgram failed, no Groq key configured for fallback");
                    }
                }
            }
        });
    } else {
        // Groq-only mode (no Deepgram key configured)
        if shadow_audio.is_empty() {
            return;
        }
        thread::spawn(move || {
            if let Some(ref key) = groq_key {
                if let Some(text) = transcribe_groq(shadow_audio, sample_rate, key, &keywords) {
                    if paste_text(&text) {
                        maybe_return();
                    }
                }
            }
        });
    }
}

// ============================================================================
// UI, permissions, event tap
// ============================================================================

fn check_input_monitoring_permission() -> bool {
    unsafe {
        #[link(name = "CoreGraphics", kind = "framework")]
        extern "C" {
            fn CGPreflightListenEventAccess() -> bool;
            fn CGRequestListenEventAccess() -> bool;
        }
        if CGPreflightListenEventAccess() {
            return true;
        }
        if CGRequestListenEventAccess() {
            return true;
        }
        show_alert(
            "Input Monitoring Required",
            "FnKey needs Input Monitoring permission to detect the Fn key.\n\n\
             Please grant access in System Settings → Privacy & Security → Input Monitoring, then relaunch FnKey.",
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

/// Flips a menu setting, ticks its menu item and saves it as ~/.config/fnkey/<file>
unsafe fn toggle_setting(flag: &AtomicBool, item: *mut Object, file: &str) {
    let new_val = !flag.load(Ordering::SeqCst);
    flag.store(new_val, Ordering::SeqCst);
    if !item.is_null() {
        let state: i64 = if new_val { 1 } else { 0 };
        let _: () = msg_send![item as id, setState: state];
    }
    if let Some(home) = env::var_os("HOME") {
        let path = std::path::Path::new(&home).join(".config").join("fnkey").join(file);
        if new_val {
            let _ = std::fs::write(&path, "1");
        } else {
            let _ = std::fs::remove_file(&path);
        }
    }
}

extern "C" fn toggle_auto_return(_this: &Object, _cmd: Sel, _sender: id) {
    unsafe { toggle_setting(&AUTO_RETURN, AUTO_RETURN_ITEM, "auto_return") }
}

extern "C" fn toggle_live_paste(_this: &Object, _cmd: Sel, _sender: id) {
    unsafe { toggle_setting(&LIVE_PASTE, LIVE_PASTE_ITEM, "live_paste") }
}

extern "C" fn edit_keywords(_this: &Object, _cmd: Sel, _sender: id) {
    if let Some(home) = env::var_os("HOME") {
        let path = std::path::Path::new(&home)
            .join(".config")
            .join("fnkey")
            .join("keywords");
        // Create file with example if it doesn't exist
        if !path.exists() {
            let _ = std::fs::create_dir_all(path.parent().unwrap());
            let _ = std::fs::write(&path, "# Custom keywords (one per line)\n# Improves transcription accuracy for these terms\nAnthropic\nClaude\n");
        }
        let _ = std::process::Command::new("open").arg("-t").arg(&path).spawn();
    }
}

fn register_menu_handler_class() {
    let superclass = Class::get("NSObject").unwrap();
    let mut decl = ClassDecl::new("FnKeyMenuHandler", superclass).unwrap();
    unsafe {
        decl.add_method(
            sel!(toggleAutoReturn:),
            toggle_auto_return as extern "C" fn(&Object, Sel, id),
        );
        decl.add_method(
            sel!(toggleLivePaste:),
            toggle_live_paste as extern "C" fn(&Object, Sel, id),
        );
        decl.add_method(
            sel!(editKeywords:),
            edit_keywords as extern "C" fn(&Object, Sel, id),
        );
    }
    decl.register();
}

/// Adds a menu item with a checkmark that calls `action` on the handler
unsafe fn add_toggle_item(menu: id, handler: id, title: &str, action: Sel, on: bool) -> *mut Object {
    let title = NSString::alloc(nil).init_str(title);
    let empty_key = NSString::alloc(nil).init_str("");
    let item: id = msg_send![class!(NSMenuItem), alloc];
    let item: id = msg_send![item, initWithTitle: title action: action keyEquivalent: empty_key];
    let _: () = msg_send![item, setTarget: handler];
    if on {
        let _: () = msg_send![item, setState: 1_i64];
    }
    let _: () = msg_send![menu, addItem: item];
    item as *mut Object
}

unsafe fn create_status_item() {
    register_menu_handler_class();

    let status_bar: id = msg_send![class!(NSStatusBar), systemStatusBar];
    let status_item: id = msg_send![status_bar, statusItemWithLength: -1.0_f64];
    let _: () = msg_send![status_item, retain];
    STATUS_ITEM = status_item as *mut Object;
    let title = NSString::alloc(nil).init_str("○");
    let button: id = msg_send![status_item, button];
    let _: () = msg_send![button, setTitle: title];
    let menu: id = NSMenu::new(nil);

    // Setting toggles
    let handler_class = Class::get("FnKeyMenuHandler").unwrap();
    let handler: id = msg_send![handler_class, new];
    let _: () = msg_send![handler, retain];
    let empty_key = NSString::alloc(nil).init_str("");
    AUTO_RETURN_ITEM = add_toggle_item(
        menu, handler, "Press Return after paste", sel!(toggleAutoReturn:), AUTO_RETURN.load(Ordering::SeqCst),
    );
    LIVE_PASTE_ITEM = add_toggle_item(
        menu, handler, "Paste while speaking", sel!(toggleLivePaste:), LIVE_PASTE.load(Ordering::SeqCst),
    );

    // Edit Keywords
    let keywords_title = NSString::alloc(nil).init_str("Edit Keywords…");
    let keywords_item: id = msg_send![class!(NSMenuItem), alloc];
    let keywords_item: id = msg_send![keywords_item, initWithTitle: keywords_title action: sel!(editKeywords:) keyEquivalent: empty_key];
    let _: () = msg_send![keywords_item, setTarget: handler];
    let _: () = msg_send![menu, addItem: keywords_item];

    // Separator
    let separator: id = msg_send![class!(NSMenuItem), separatorItem];
    let _: () = msg_send![menu, addItem: separator];

    // Quit
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
        vec![CGEventType::FlagsChanged, CGEventType::KeyDown],
        move |_, event_type, event| {
            // A key typed during the press makes it a shortcut (fn+Delete, fn+arrow).
            // FnKey's own live-paste Cmd+V is tagged and does not count.
            if matches!(event_type, CGEventType::KeyDown) {
                let own = event.get_integer_value_field(EventField::EVENT_SOURCE_USER_DATA) == OWN_EVENT_TAG;
                if !own && was_pressed_clone.load(Ordering::SeqCst) {
                    state_for_callback.combo.store(true, Ordering::SeqCst);
                }
                return None;
            }

            let flags = event.get_flags().bits();
            let fn_pressed = (flags & FN_KEY_FLAG) != 0;
            let option_pressed = (flags & OPTION_KEY_FLAG) != 0;
            let use_fn = state_for_callback.use_fn_key.load(Ordering::SeqCst);
            let key_pressed = if use_fn { fn_pressed } else { option_pressed };

            if fn_pressed && !fn_detected_clone.load(Ordering::SeqCst) {
                fn_detected_clone.store(true, Ordering::SeqCst);
            }

            let prev_pressed = was_pressed_clone.load(Ordering::SeqCst);

            if key_pressed && !prev_pressed {
                start_recording(&state_for_callback);
            } else if !key_pressed && prev_pressed {
                stop_recording(&state_for_callback);
            }

            // Another modifier held with the trigger (fn+Shift, Option+Command) is a shortcut too
            let trigger_flag = if use_fn { FN_KEY_FLAG } else { OPTION_KEY_FLAG };
            if key_pressed && other_modifier_held(flags, trigger_flag) {
                state_for_callback.combo.store(true, Ordering::SeqCst);
            }

            was_pressed_clone.store(key_pressed, Ordering::SeqCst);
            None
        },
    )
    .expect("Failed to create event tap - check Input Monitoring permissions");

    let source = tap.mach_port.create_runloop_source(0)
        .expect("Failed to create runloop source");
    let run_loop = CFRunLoop::get_current();
    run_loop.add_source(&source, unsafe { kCFRunLoopCommonModes });
    tap.enable();

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

fn paste_with_cgevent() {
    if let Ok(source) = CGEventSource::new(CGEventSourceStateID::HIDSystemState) {
        let v_keycode = get_paste_keycode();
        if let Ok(key_down) = CGEvent::new_keyboard_event(source.clone(), v_keycode, true) {
            if let Ok(key_up) = CGEvent::new_keyboard_event(source, v_keycode, false) {
                key_down.set_flags(CGEventFlags::CGEventFlagCommand);
                key_up.set_flags(CGEventFlags::CGEventFlagCommand);
                key_down.set_integer_value_field(EventField::EVENT_SOURCE_USER_DATA, OWN_EVENT_TAG);
                key_up.set_integer_value_field(EventField::EVENT_SOURCE_USER_DATA, OWN_EVENT_TAG);
                key_down.post(CGEventTapLocation::HID);
                key_up.post(CGEventTapLocation::HID);
            }
        }
    }
}

fn press_return() {
    let _ = std::process::Command::new("osascript")
        .arg("-e")
        .arg("tell application \"System Events\" to key code 36")
        .output();
}

/// Puts text on the clipboard and sends Cmd+V. False if the clipboard refused it.
fn paste_text(text: &str) -> bool {
    let Ok(mut clipboard) = Clipboard::new() else { return false };
    if clipboard.set_text(text).is_err() {
        return false;
    }
    paste_with_cgevent();
    true
}

/// Presses Return when "Press Return after paste" is on
fn maybe_return() {
    if AUTO_RETURN.load(Ordering::SeqCst) {
        // Let the paste finish processing before sending Return
        thread::sleep(Duration::from_millis(50));
        press_return();
    }
}

// ============================================================================
// Audio encoding (for Groq fallback only)
// ============================================================================

fn enhance_audio(samples: &[f32], sample_rate: u32) -> Vec<f32> {
    if samples.is_empty() {
        return Vec::new();
    }
    let mean: f32 = samples.iter().sum::<f32>() / samples.len() as f32;
    let dc_removed: Vec<f32> = samples.iter().map(|&s| s - mean).collect();
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
    let peak = filtered.iter().map(|&s| s.abs()).fold(1.0_f32, f32::max);
    let target = 0.707_f32;
    let raw_gain = target / peak;
    let gain = raw_gain.min(4.0);
    if gain > 1.05 {
        filtered.iter().map(|&s| (s * gain).clamp(-1.0, 1.0)).collect()
    } else {
        filtered.iter().map(|&s| s.clamp(-1.0, 1.0)).collect()
    }
}

fn encode_wav(samples: &[f32], sample_rate: u32) -> Result<Vec<u8>, hound::Error> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_defaults_to_us_and_accepts_pasted_urls() {
        assert_eq!(parse_deepgram_host(None), "api.deepgram.com");
        assert_eq!(parse_deepgram_host(Some("api.in.deepgram.com".into())), "api.in.deepgram.com");
        assert_eq!(parse_deepgram_host(Some("wss://api.in.deepgram.com/".into())), "api.in.deepgram.com");
        assert_eq!(parse_deepgram_host(Some("https://api.eu.deepgram.com".into())), "api.eu.deepgram.com");
        assert_eq!(parse_deepgram_host(Some("wss://".into())), "api.deepgram.com");
    }

    #[test]
    fn taps_and_shortcuts_are_not_dictation() {
        assert!(!is_dictation(Duration::from_millis(150), false));
        assert!(!is_dictation(Duration::from_millis(299), false));
        assert!(is_dictation(Duration::from_millis(300), false));
        assert!(is_dictation(Duration::from_secs(5), false));
        assert!(!is_dictation(Duration::from_secs(5), true));
    }

    #[test]
    fn other_modifiers_make_a_shortcut() {
        let (shift, cmd, caps) = (0x20000, 0x100000, 0x10000);
        assert!(!other_modifier_held(FN_KEY_FLAG, FN_KEY_FLAG));
        assert!(other_modifier_held(FN_KEY_FLAG | shift, FN_KEY_FLAG));
        assert!(!other_modifier_held(FN_KEY_FLAG | caps, FN_KEY_FLAG));
        assert!(!other_modifier_held(OPTION_KEY_FLAG, OPTION_KEY_FLAG));
        assert!(other_modifier_held(OPTION_KEY_FLAG | cmd, OPTION_KEY_FLAG));
    }

    #[test]
    fn no_speech_never_reaches_groq() {
        assert!(matches!(after_deepgram(Some(DgResult::Ok(String::new())), false), Next::Nothing));
        assert!(matches!(after_deepgram(Some(DgResult::Ok("hi".into())), false), Next::Paste(t) if t == "hi"));
        assert!(matches!(after_deepgram(Some(DgResult::Err("dropped".into())), false), Next::Groq));
        assert!(matches!(after_deepgram(None, false), Next::Groq));
    }

    #[test]
    fn live_text_is_never_pasted_twice() {
        assert!(matches!(after_deepgram(Some(DgResult::Ok("hi".into())), true), Next::ReturnOnly));
        assert!(matches!(after_deepgram(Some(DgResult::Err("dropped".into())), true), Next::Nothing));
        assert!(matches!(after_deepgram(None, true), Next::Nothing));
        assert!(matches!(after_deepgram(Some(DgResult::Ok(String::new())), true), Next::Nothing));
    }

    #[test]
    fn live_paste_and_groq_cannot_both_own_a_press() {
        let s = DgStats::new("h".into(), Instant::now());
        assert!(s.claim_live());
        assert!(s.claim_live());
        assert!(!s.claim_groq());
        let s = DgStats::new("h".into(), Instant::now());
        assert!(s.claim_groq());
        assert!(!s.claim_live());
        assert!(!s.live_pasted());
    }

    #[test]
    fn live_phrases_are_spaced_and_joined() {
        let t0 = Instant::now();
        let mut lp = LivePaste::default();
        assert_eq!(lp.take_due(t0), None);
        lp.add("Hello.");
        assert_eq!(lp.take_due(t0).as_deref(), Some("Hello."));
        lp.add("How are you?");
        assert_eq!(lp.take_due(t0 + Duration::from_millis(100)), None);
        lp.add("Fine.");
        assert_eq!(lp.take_due(t0 + PASTE_GAP).as_deref(), Some(" How are you? Fine."));
    }
}
