# FnKey — Agent Instructions

## What is this

FnKey is a macOS menu bar app (Rust) that records audio while a hotkey is held, transcribes it via Whisper, sanitizes the text with a small LLM, and pastes the result at the cursor. Single file: `src/main.rs`.

## Architecture

```
Hotkey held → mic records → release → WAV encoded → Whisper STT → LLM sanitizer → clipboard → ⌘V paste
```

- **Transcription**: Any OpenAI-compatible `/v1/audio/transcriptions` endpoint
- **Sanitization**: Any OpenAI-compatible `/v1/chat/completions` endpoint
- **Config**: `~/.config/fnkey/config.toml` (TOML, deserialized into `Config` struct)
- **No runtime dependencies** — single static binary, no frameworks beyond macOS system libs

## Text Sanitization Pipeline

The sanitizer is the key differentiator. Raw Whisper output is messy — filler words, repeated words, misheard technical terms. A small LLM (0.6B–3B params) cleans it up in real time (<200ms).

### Setup for contributors

After cloning this repo, set up local inference for development:

1. **Whisper STT** — download and serve a Whisper model:
   ```bash
   # Option A: llama.cpp (any platform with Vulkan/CUDA/Metal)
   whisper-server -m ggml-large-v3-turbo.bin --port 8100

   # Option B: faster-whisper-server (CUDA)
   pip install faster-whisper-server
   faster-whisper-server --model large-v3-turbo --port 8100

   # Option C: MLX (macOS Apple Silicon)
   pip install mlx-whisper
   # (use via mlx-whisper Python API, or wrap in a server)
   ```

2. **Text sanitizer** — download and serve a small LLM:
   ```bash
   # Option A: llama.cpp
   llama-server -m Qwen3-0.6B-Q6_K.gguf --port 8108 -ngl 999 --jinja -c 4096

   # Option B: MLX
   pip install mlx-lm
   mlx_lm.server --model mlx-community/Qwen3-0.6B-4bit --port 8108
   ```

3. **Configure** `~/.config/fnkey/config.toml`:
   ```toml
   api_key = "not-needed"
   transcription_url = "http://localhost:8100/v1/audio/transcriptions"
   whisper_model = "whisper-large-v3-turbo"
   polish_url = "http://localhost:8108/v1/chat/completions"
   polish_model = "qwen3-0.6b"
   always_polish = true
   ```

### How the system prompt works

The `polish_prompt` config field overrides the built-in system prompt. For 0.6B models, the prompt must be:
- **Short** — the model has limited instruction-following at this scale
- **Explicit** — use a replacement dictionary for domain terms the STT engine misrecognizes
- **End with `/no_think`** — disables Qwen3 reasoning mode, keeping latency under 200ms

Example for a codebase that uses llama.cpp, Claude, and Tailscale:

```toml
polish_prompt = """Fix dictation. Remove filler words. Fix grammar and punctuation.
Replace misheard terms:
- clod dot MD/cloud dot MD → CLAUDE.md
- agents dot MD → agents.md
- lama dot CPP → llama.cpp
- quan three/qan three → Qwen3
- tail scale → Tailscale
Output ONLY the corrected text. /no_think"""
```

**When helping users customize their prompt**: identify the technical terms they use frequently, figure out how Whisper is likely to misrecognize them, and add those as `misheard → correct` pairs. This is the single most impactful thing you can do to improve dictation accuracy.

## Build & test

```bash
cargo build --release          # Build
./build-app.sh                 # Create .app bundle
cargo check                    # Type check only
```

After rebuilding, macOS invalidates permissions — user must re-add the app in System Settings (Input Monitoring, Accessibility).

## Key code locations

- `Config` struct and `load_config()` — config parsing and defaults
- `polish_text()` — LLM sanitization call
- `transcribe_and_paste()` — main pipeline: encode WAV → call Whisper → sanitize → paste
- `run_event_tap()` — hotkey detection via CGEventTap
- `enhance_audio()` — audio preprocessing (DC removal, high-pass filter, normalization)

## Config fields

| Field | Default | Purpose |
|-------|---------|---------|
| `api_key` | `""` | Bearer token for Whisper transcription (also used for sanitizer if `polish_api_key` is empty) |
| `polish_api_key` | `""` | Separate bearer token for sanitizer endpoint (empty = falls back to `api_key`) |
| `transcription_url` | Groq | Whisper endpoint |
| `polish_url` | Groq | Chat completions endpoint for sanitizer |
| `whisper_model` | `whisper-large-v3` | Model name sent to STT endpoint |
| `polish_model` | `llama-3.3-70b-versatile` | Model name sent to sanitizer endpoint |
| `hotkey` | `fn` | Trigger key (fn/option/control/shift/command) |
| `language` | `""` (auto) | ISO-639-1 hint for Whisper |
| `always_polish` | `true` | Sanitize every dictation by default |
| `polish_prompt` | `""` (built-in) | Custom system prompt for sanitizer |
