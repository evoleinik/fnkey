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
# API key (optional — some local servers don't need one)
api_key = "gsk_..."

# API endpoints (default: Groq — any OpenAI-compatible API works)
transcription_url = "https://api.groq.com/openai/v1/audio/transcriptions"
polish_url = "https://api.groq.com/openai/v1/chat/completions"

# Models (sent as-is in the API request — use whatever your server expects)
whisper_model = "whisper-large-v3"
polish_model = "llama-3.3-70b-versatile"

# Hotkey: fn | option | control | shift | command
hotkey = "fn"
```

### Custom API endpoints

FnKey works with any OpenAI-compatible API. Examples:

```toml
# OpenAI
api_key = "sk-..."
transcription_url = "https://api.openai.com/v1/audio/transcriptions"
polish_url = "https://api.openai.com/v1/chat/completions"
whisper_model = "whisper-1"
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

- Hold **hotkey** and speak → raw transcription pasted at cursor
- Hold **hotkey + polish modifier** and speak → polished transcription (removes filler words, fixes punctuation)
- Release to transcribe and paste
- Click menu bar icon **○** → **Settings...** to edit config, **Quit** to exit

The icon changes: ○ (idle) → ● (recording)

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

- **Configurable hotkey** — Fn, Option, Control, Shift, or Command
- **Custom API endpoints** — any OpenAI-compatible transcription/chat API (Groq, OpenAI, vLLM, faster-whisper, etc.)
- **Polish mode** — hold an extra modifier to clean up filler words and fix punctuation via LLM
- **Audio enhancement** — DC offset removal, high-pass filter, peak normalization
- **TOML config** — `~/.config/fnkey/config.toml` with Settings menu item
- **Auto sample rate** — uses device's native sample rate
- **JSON response handling** — works with servers that return JSON instead of plain text

## Known Limitations

**Slight recording delay**: There's a brief moment when you start speaking before audio capture begins. This is a deliberate tradeoff — eliminating this delay would require the microphone to be always active, showing the yellow indicator constantly. The current design prioritizes privacy: the microphone only activates when you press the hotkey.
