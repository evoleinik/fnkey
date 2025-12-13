# fnkey.ai

Hold Fn key, speak, paste transcribed text.

## Install

1. Set your Groq API key:
   ```bash
   mkdir -p ~/.config/fnkey
   echo 'your-groq-api-key' > ~/.config/fnkey/api_key
   ```
   Get a key at [console.groq.com](https://console.groq.com)

2. Build and install:
   ```bash
   ./build-app.sh
   cp -r FnKey.app /Applications/
   ```

3. Launch:
   ```bash
   open /Applications/FnKey.app
   ```

4. Grant permissions in **System Settings → Privacy & Security**:

   | Permission | Purpose | How to Grant |
   |------------|---------|--------------|
   | **Input Monitoring** | Detect Fn key press | Add FnKey.app via + button |
   | **Microphone** | Record voice | Prompted on first use, or add manually |
   | **Accessibility** | Auto-paste text | Add FnKey.app via + button |

   Note: After rebuilding the app, you may need to remove and re-add it in these settings.

## Usage

- Hold **Fn** key and speak
- Release to transcribe and paste
- Click menu bar icon (○) → Quit to exit

The icon changes: ○ (idle) → ● (recording)

## Build from source

```bash
cargo build --release
./build-app.sh
```

## Features

- **Whisper large-v3** - Full model for best accuracy
- **Audio enhancement** - DC offset removal, high-pass filter, peak normalization
- **Config file** - API key stored in `~/.config/fnkey/api_key`
- **Auto sample rate** - Uses device's native sample rate

## TODO

Features from Ito not yet implemented:

- **Vocabulary hints** - Send prompt with proper nouns/technical terms to improve accuracy
- **No-speech detection** - Use `verbose_json` response format and check `no_speech_prob` to skip silent recordings
- **Custom dictionary** - User-configurable word list for domain-specific terms

## Notes

- Falls back to Option key if Fn not detected after 5s
- Floating red dot appears during recording

## Known Limitations

**Slight recording delay**: There's a brief moment when you start speaking before audio capture begins. This is a deliberate tradeoff — eliminating this delay would require the microphone to be always active, showing the yellow indicator constantly. The current design prioritizes privacy: the microphone only activates when you press the Fn key.
