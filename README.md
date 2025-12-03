# fnkey.ai

Hold Fn key, speak, paste transcribed text.

## Install

1. Set your Groq API key:
   ```bash
   echo 'export GROQ_API_KEY="your-key"' >> ~/.zshrc
   source ~/.zshrc
   ```

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

## Notes

- Uses Groq Whisper API (whisper-large-v3-turbo)
- Falls back to Option key if Fn not detected after 5s
- Floating red dot appears during recording
