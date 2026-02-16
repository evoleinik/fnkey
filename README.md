# fnkey.ai

Hold a hotkey, speak, paste transcribed text. Works with any OpenAI-compatible speech-to-text API.

## Install

1. Download from [Releases](https://github.com/evoleinik/fnkey/releases):
   - **Apple Silicon** (M1/M2/M3): `FnKey-arm64.zip`
   - **Intel**: `FnKey-x64.zip`

2. Unzip and move to Applications:
   ```bash
   unzip FnKey-arm64.zip
   mv FnKey.app /Applications/
   ```

3. Grant macOS permissions (see [Permissions](#permissions) below)

4. Launch:
   ```bash
   open /Applications/FnKey.app
   ```

5. Click the **○** menu bar icon → **Settings...** to configure your API endpoint

## Configuration

FnKey is configured via `~/.config/fnkey/config.toml`. A template is created automatically on first launch. Click **Settings...** in the menu bar to open it.

```toml
# API keys (optional — some local servers don't need one)
api_key = "gsk_..."           # Used for Whisper transcription
polish_api_key = ""           # Used for sanitizer (empty = use api_key)

# API endpoints (default: Groq — any OpenAI-compatible API works)
transcription_url = "https://api.groq.com/openai/v1/audio/transcriptions"
polish_url = "https://api.groq.com/openai/v1/chat/completions"

# Models (sent as-is in the API request — use whatever your server expects)
whisper_model = "whisper-large-v3"
polish_model = "llama-3.3-70b-versatile"

# Hotkey: fn | option | control | shift | command
hotkey = "fn"

# Language hint (ISO-639-1 code: "en", "sk", "de", "fr", etc. Empty = auto-detect)
language = ""

# Always run text sanitization on every dictation (default: true)
# When true, hold the polish modifier to get RAW Whisper output instead
always_polish = true

# Custom system prompt for text sanitization (empty = use built-in)
polish_prompt = ""
```

### Custom API endpoints

FnKey works with any OpenAI-compatible API. Examples:

```toml
# OpenAI for both
api_key = "sk-..."
transcription_url = "https://api.openai.com/v1/audio/transcriptions"
polish_url = "https://api.openai.com/v1/chat/completions"
whisper_model = "whisper-1"
polish_model = "gpt-4o-mini"

# Mixed: Groq for Whisper, OpenAI for sanitizer
api_key = "gsk_..."
polish_api_key = "sk-..."
transcription_url = "https://api.groq.com/openai/v1/audio/transcriptions"
polish_url = "https://api.openai.com/v1/chat/completions"
whisper_model = "whisper-large-v3"
polish_model = "gpt-4o-mini"

# Local / self-hosted (vLLM, faster-whisper-server, etc.)
api_key = "not-needed"
transcription_url = "http://localhost:8000/v1/audio/transcriptions"
whisper_model = "my-model-name"
```

Both plain-text and JSON transcription responses are handled automatically.

### Hotkey options

| Hotkey | Config value | Polish modifier |
|--------|-------------|-----------------|
| Fn | `"fn"` (default) | Ctrl |
| Option/Alt | `"option"` | Ctrl |
| Control | `"control"` | Shift |
| Shift | `"shift"` | Ctrl |
| Command | `"command"` | Ctrl |

When `hotkey = "control"`, the polish modifier switches to Shift to avoid conflict.

### Backward compatibility

FnKey checks for configuration in this order:
1. `~/.config/fnkey/config.toml`
2. `~/.config/fnkey/api_key` (legacy — plain text API key)
3. `GROQ_API_KEY` environment variable

## Usage

- Hold **hotkey** → speak → release → cleaned text pasted at cursor
- Hold **hotkey + polish modifier** → speak → release → raw Whisper output (bypasses sanitization)
- Click menu bar icon **○** → **Settings...** to edit config, **Quit** to exit

The icon changes: ○ (idle) → ● (recording)

When `always_polish = false`, the behavior is inverted: hotkey gives raw output, hotkey + modifier gives polished output.

## Text Sanitization

FnKey includes an LLM-powered text sanitization step that runs after Whisper transcription. It fixes the common artifacts of speech-to-text: filler words, repeated words, broken grammar, and misheard terms.

### How it works

```
Voice → [Whisper STT] → raw text → [LLM sanitizer] → clean text → clipboard → paste
```

The sanitizer is a lightweight LLM (as small as 0.6B parameters) that receives the raw Whisper output and a system prompt, then returns cleaned text. It uses any OpenAI-compatible chat completions endpoint.

### Running locally

For real-time dictation, the sanitizer must be fast. A small model (0.6B–3B) running locally can sanitize a sentence in under 200ms. Two recommended setups:

#### llama.cpp (Linux/macOS, GPU or CPU)

Download a small model like [Qwen3-0.6B](https://huggingface.co/Qwen/Qwen3-0.6B-GGUF) and serve it:

```bash
llama-server \
  -m Qwen3-0.6B-Q6_K.gguf \
  --port 8108 \
  --host 0.0.0.0 \
  -ngl 999 \
  --jinja \
  -c 4096

# On macOS with Metal:
llama-server -m Qwen3-0.6B-Q6_K.gguf --port 8108 -ngl 999 --jinja -c 4096
```

Then configure FnKey:
```toml
polish_url = "http://localhost:8108/v1/chat/completions"
polish_model = "Qwen3-0.6B-Q6_K.gguf"
api_key = "not-needed"
```

#### MLX (macOS Apple Silicon)

```bash
pip install mlx-lm
mlx_lm.server --model mlx-community/Qwen3-0.6B-4bit --port 8108
```

Then configure FnKey the same way.

#### Whisper locally

For the transcription side, run Whisper via [faster-whisper-server](https://github.com/fedirz/faster-whisper-server), [vLLM](https://docs.vllm.ai/), or llama.cpp's built-in whisper support:

```bash
# faster-whisper-server (CUDA)
pip install faster-whisper-server
faster-whisper-server --model large-v3-turbo --port 8100

# vLLM (CUDA)
vllm serve openai/whisper-large-v3-turbo --port 8100

# llama.cpp whisper
whisper-server -m ggml-large-v3-turbo.bin --port 8100
```

### Custom system prompt

The built-in prompt handles general dictation cleanup. For domain-specific accuracy, set `polish_prompt` in your config with a replacement dictionary for terms your STT engine commonly misrecognizes:

```toml
polish_prompt = """Fix dictation. Remove filler words. Fix grammar and punctuation.
Replace misheard terms:
- clod dot MD/cloud dot MD → CLAUDE.md
- agents dot MD → agents.md
- lama dot CPP/llama dot CPP → llama.cpp
- quan three/qan three → Qwen3
- M L X → MLX
- tailscale/tail scale → Tailscale
Output ONLY the corrected text. /no_think"""
```

The `/no_think` suffix disables reasoning on Qwen3 models, keeping response time under 200ms.

**Adapt this to your codebase.** If you dictate about Kubernetes, add `cooper netties → Kubernetes`. If you work on a project called "Nexus", add `nexus/next us → Nexus`. The replacement dictionary is the key to making 0.6B models accurate for your domain.

### Recommended models

| Model | Size | Speed | Notes |
|-------|------|-------|-------|
| Qwen3-0.6B | 600MB | ~270 t/s | Best speed, needs explicit replacement dictionary |
| Qwen2.5-1.5B | 1.5GB | ~150 t/s | Better understanding, less dictionary needed |
| Qwen3-1.7B | 1.7GB | ~120 t/s | Good balance of speed and quality |

For the sanitizer, smaller is better — the task is simple pattern matching and cleanup, not reasoning. Use `/no_think` with Qwen3 models to disable chain-of-thought and keep latency low.

## Permissions

FnKey requires three macOS permissions. All are configured in **System Settings → Privacy & Security**.

| Permission | Why | How to grant |
|------------|-----|--------------|
| **Input Monitoring** | Detect hotkey press/release | System Settings → Input Monitoring → click **+** → select FnKey.app |
| **Microphone** | Record voice while hotkey is held | Prompted automatically on first recording, or add manually |
| **Accessibility** | Simulate ⌘V to paste transcribed text | System Settings → Accessibility → click **+** → select FnKey.app |

### After rebuilding from source

When you rebuild and re-codesign the app, macOS **invalidates all previously granted permissions** because the binary signature changes. You must:

1. Open **System Settings → Privacy & Security**
2. For **Input Monitoring** and **Accessibility**: remove FnKey, then re-add `/Applications/FnKey.app`
3. Relaunch the app

The **Microphone** permission is usually re-prompted automatically.

### Troubleshooting permissions

| Symptom | Cause | Fix |
|---------|-------|-----|
| App launches but hotkey does nothing | Input Monitoring not granted | Add FnKey to Input Monitoring |
| Hotkey records but text doesn't paste | Accessibility not granted | Add FnKey to Accessibility |
| No microphone indicator when holding hotkey | Microphone not granted | Add FnKey to Microphone, or approve the prompt |
| Permissions are granted but app still doesn't work | Stale permission after rebuild | Remove and re-add FnKey in each permission category |

## Build from source

```bash
cargo build --release
```

To create an .app bundle:

```bash
./build-app.sh
cp -r FnKey.app /Applications/
```

To regenerate the app icon (requires Python + Pillow):

```bash
python3 -m venv .venv && source .venv/bin/activate && pip install Pillow
python3 gen-icon.py
```

Note: If cargo isn't found, run with login shell: `/bin/bash -l -c './build-app.sh'`

## Features

- **Text sanitization** — LLM-powered cleanup of filler words, repeated words, grammar, and misheard terms
- **Configurable hotkey** — Fn, Option, Control, Shift, or Command
- **Custom API endpoints** — any OpenAI-compatible transcription/chat API (Groq, OpenAI, vLLM, faster-whisper, etc.)
- **Custom system prompt** — domain-specific replacement dictionaries for accurate technical dictation
- **Audio enhancement** — DC offset removal, high-pass filter, peak normalization
- **TOML config** — `~/.config/fnkey/config.toml` with Settings menu item
- **Auto sample rate** — uses device's native sample rate
- **JSON response handling** — works with servers that return JSON instead of plain text

## Known Limitations

**Slight recording delay**: There's a brief moment when you start speaking before audio capture begins. This is a deliberate tradeoff — eliminating this delay would require the microphone to be always active, showing the yellow indicator constantly. The current design prioritizes privacy: the microphone only activates when you press the hotkey.
