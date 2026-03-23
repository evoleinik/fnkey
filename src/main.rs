//! fnkey.ai - Hold Fn key, speak, paste transcribed text
//!
//! Streams audio to Deepgram via WebSocket for real-time transcription.
//! Falls back to Groq Whisper batch API if Deepgram key not configured.
//!
//! Config files (~/.config/fnkey/):
//!   deepgram_key  - Deepgram API key (streaming, preferred)
//!   api_key       - Groq API key (batch fallback + polish)

use std::collections::HashMap;
use std::env;
use std::ffi::c_void;
use std::io::Cursor;
use std::io::Write as IoWrite;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
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
const DEEPGRAM_SAMPLE_RATE: u32 = 16000;

/// Messages sent from audio callback / event tap to the WebSocket thread
enum WsCommand {
    /// Raw PCM audio chunk (already resampled to 16kHz, i16 LE bytes)
    Audio(Vec<u8>),
    /// Stop streaming, finalize, paste result
    Stop,
}

/// Result from Deepgram streaming thread
enum DgResult {
    /// Transcription succeeded (may be empty string)
    Ok(String),
    /// Connection or streaming error
    Err(String),
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
}

static mut STATUS_ITEM: *mut Object = std::ptr::null_mut();
static mut AUDIO_STREAM: Option<Stream> = None;
static AUTO_RETURN: AtomicBool = AtomicBool::new(false);
static mut AUTO_RETURN_ITEM: *mut Object = std::ptr::null_mut();

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

    // Load auto-return preference
    if read_config_file("auto_return").map_or(false, |v| v == "1") {
        AUTO_RETURN.store(true, Ordering::SeqCst);
    }

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
) {
    thread::spawn(move || {
        let mut url = format!(
            "wss://api.deepgram.com/v1/listen?\
             encoding=linear16&sample_rate={}&channels=1&\
             interim_results=true&endpointing=300&\
             punctuate=true&smart_format=true&model=nova-3&\
             language=multi",
            DEEPGRAM_SAMPLE_RATE
        );
        for kw in &keywords {
            url.push_str(&format!("&keyterm={}", urlencoding::encode(kw)));
        }
        let request = tungstenite::http::Request::builder()
            .uri(&url)
            .header("Authorization", format!("Token {}", key))
            .header("Host", "api.deepgram.com")
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

        // Set WebSocket to non-blocking so we can interleave send/recv
        if let tungstenite::stream::MaybeTlsStream::NativeTls(ref s) = ws.get_ref() {
            let _ = s.get_ref().set_nonblocking(true);
        } else if let tungstenite::stream::MaybeTlsStream::Plain(ref s) = ws.get_ref() {
            let _ = s.set_nonblocking(true);
        }

        let mut transcript = String::new();
        let mut raw_msgs: Vec<String> = Vec::new();
        let mut running = true;
        let mut ws_error: Option<String> = None;
        let started = std::time::Instant::now();
        let mut chunks_sent: u32 = 0;
        let mut bytes_sent: usize = 0;
        let mut msgs_received: u32 = 0;
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
                        let msg = format!("Deepgram send error after {}ms, {} chunks/{}KB sent: {}",
                            started.elapsed().as_millis(), chunks_sent, bytes_sent / 1024, e);
                        log_error(&msg);
                        ws_error = Some(msg);
                        running = false;
                    } else {
                        chunks_sent += 1;
                        bytes_sent += len;
                    }
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
                                msgs_received += 1;
                                accumulate_transcript(&text, &mut transcript, &mut raw_msgs);
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
                    ws_error = Some(format!("Deepgram: channel disconnected after {}ms, {} chunks/{}KB sent",
                        started.elapsed().as_millis(), chunks_sent, bytes_sent / 1024));
                    running = false;
                }
            }

            // 2. Try to read transcript from WebSocket (non-blocking)
            match ws.read() {
                Ok(Message::Text(text)) => {
                    msgs_received += 1;
                    accumulate_transcript(&text, &mut transcript, &mut raw_msgs);
                }
                Ok(Message::Close(frame)) => {
                    if !got_stop {
                        // Server closed before we sent Stop — unexpected
                        let reason = frame.as_ref()
                            .map(|f| format!("code={}, reason='{}'", f.code, f.reason))
                            .unwrap_or_else(|| "no close frame".to_string());
                        let msg = format!("Deepgram server closed early after {}ms, {} chunks/{}KB sent, {} msgs recv'd: {}",
                            started.elapsed().as_millis(), chunks_sent, bytes_sent / 1024, msgs_received, reason);
                        log_error(&msg);
                        ws_error = Some(msg);
                    }
                    running = false;
                }
                Err(tungstenite::Error::Io(ref e))
                    if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(tungstenite::Error::ConnectionClosed) => {
                    if !got_stop {
                        let msg = format!("Deepgram connection dropped after {}ms, {} chunks/{}KB sent, {} msgs recv'd",
                            started.elapsed().as_millis(), chunks_sent, bytes_sent / 1024, msgs_received);
                        log_error(&msg);
                        ws_error = Some(msg);
                    }
                    running = false;
                }
                Err(e) => {
                    let msg = format!("Deepgram WebSocket error after {}ms, {} chunks/{}KB sent, {} msgs recv'd: {}",
                        started.elapsed().as_millis(), chunks_sent, bytes_sent / 1024, msgs_received, e);
                    log_error(&msg);
                    ws_error = Some(msg);
                    running = false;
                }
                _ => {}
            }

            // Small sleep to avoid busy-spinning
            thread::sleep(Duration::from_millis(5));
        }

        // Send result back — if we got a transcript, use it even if WS errored
        let text = transcript.trim().to_string();
        if !text.is_empty() {
            let _ = result_tx.send(DgResult::Ok(text));
        } else if let Some(err) = ws_error {
            let _ = result_tx.send(DgResult::Err(err));
        } else {
            let mut msg = format!(
                "Deepgram: empty transcript after {}ms, {} chunks/{}KB sent, {} msgs recv'd",
                started.elapsed().as_millis(), chunks_sent, bytes_sent / 1024, msgs_received
            );
            // Log raw Deepgram responses so we can see what it actually sent
            for (i, raw) in raw_msgs.iter().enumerate() {
                msg.push_str(&format!("\n  msg[{}]: {}", i, raw));
            }
            log_error(&msg);
            let _ = result_tx.send(DgResult::Err(msg.lines().next().unwrap_or(&msg).to_string()));
        }
    });
}

fn accumulate_transcript(json_text: &str, transcript: &mut String, raw_msgs: &mut Vec<String>) {
    raw_msgs.push(json_text.to_string());
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
        }
    }
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
        spawn_deepgram_thread(key, rx, kw, result_tx);

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
        thread::spawn(move || {
            let dg_text = if let Some(rx) = result_rx {
                match rx.recv_timeout(Duration::from_secs(5)) {
                    Ok(DgResult::Ok(text)) if !text.is_empty() => Some(text),
                    Ok(DgResult::Ok(_)) => {
                        log_error("Deepgram: empty transcript");
                        None
                    }
                    Ok(DgResult::Err(e)) => {
                        log_error(&format!("Deepgram failed: {}", e));
                        None
                    }
                    Err(_) => {
                        log_error("Deepgram: timeout waiting for result");
                        None
                    }
                }
            } else {
                None
            };

            if let Some(text) = dg_text {
                // Deepgram succeeded
                if let Ok(mut clipboard) = Clipboard::new() {
                    if clipboard.set_text(&text).is_ok() {
                        paste_and_maybe_return();
                    }
                }
            } else if let Some(ref key) = groq_key {
                if !shadow_audio.is_empty() {
                    // Fallback to Groq
                    show_notification("Deepgram failed, using Groq fallback");
                    log_error("Falling back to Groq Whisper");
                    if let Some(text) = transcribe_groq(shadow_audio, sample_rate, key, &keywords) {
                        if let Ok(mut clipboard) = Clipboard::new() {
                            if clipboard.set_text(&text).is_ok() {
                                paste_and_maybe_return();
                            }
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
        });
    } else {
        // Groq-only mode (no Deepgram key configured)
        if shadow_audio.is_empty() {
            return;
        }
        thread::spawn(move || {
            if let Some(ref key) = groq_key {
                if let Some(text) = transcribe_groq(shadow_audio, sample_rate, key, &keywords) {
                    if let Ok(mut clipboard) = Clipboard::new() {
                        if clipboard.set_text(&text).is_ok() {
                            paste_and_maybe_return();
                        }
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

extern "C" fn toggle_auto_return(_this: &Object, _cmd: Sel, _sender: id) {
    let new_val = !AUTO_RETURN.load(Ordering::SeqCst);
    AUTO_RETURN.store(new_val, Ordering::SeqCst);
    unsafe {
        if !AUTO_RETURN_ITEM.is_null() {
            let state: i64 = if new_val { 1 } else { 0 };
            let _: () = msg_send![AUTO_RETURN_ITEM as id, setState: state];
        }
    }
    if let Some(home) = env::var_os("HOME") {
        let path = std::path::Path::new(&home).join(".config").join("fnkey").join("auto_return");
        if new_val {
            let _ = std::fs::write(&path, "1");
        } else {
            let _ = std::fs::remove_file(&path);
        }
    }
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
            sel!(editKeywords:),
            edit_keywords as extern "C" fn(&Object, Sel, id),
        );
    }
    decl.register();
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

    // Auto Return toggle
    let handler_class = Class::get("FnKeyMenuHandler").unwrap();
    let handler: id = msg_send![handler_class, new];
    let _: () = msg_send![handler, retain];
    let auto_return_title = NSString::alloc(nil).init_str("Press Return after paste");
    let empty_key = NSString::alloc(nil).init_str("");
    let auto_return_item: id = msg_send![class!(NSMenuItem), alloc];
    let auto_return_item: id = msg_send![auto_return_item, initWithTitle: auto_return_title action: sel!(toggleAutoReturn:) keyEquivalent: empty_key];
    let _: () = msg_send![auto_return_item, setTarget: handler];
    if AUTO_RETURN.load(Ordering::SeqCst) {
        let _: () = msg_send![auto_return_item, setState: 1_i64];
    }
    AUTO_RETURN_ITEM = auto_return_item as *mut Object;
    let _: () = msg_send![menu, addItem: auto_return_item];

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
        vec![CGEventType::FlagsChanged],
        move |_, _, event| {
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

fn paste_and_maybe_return() {
    paste_with_cgevent();
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
