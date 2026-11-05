<h1 align="center">stt-input</h1>

<p align="center">
  <img src="assets/icon.png" width="128" alt="stt-input icon" />
</p>

<p align="center">
  <a href="README.md">English</a>
  |
  <a href="README-zh.md">简体中文</a>
</p>

<p align="center">
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-GPL--3.0--or--later-blue" alt="GPL-3.0-or-later License" /></a>
</p>

A voice input method based on the MAI STT service

Hold space and speak — confirmed text streams into the focused app as you talk, release finalizes the tail The client protocol is reimplemented from the MIT-licensed VS Code sources, no VS Code installation required

## How it works

- Hold right Ctrl: captures 16 kHz mono audio from the default microphone
- Streams audio over WebSocket to the MAI transcription endpoint
- Authenticates with a GitHub OAuth token, same as VS Code voice mode
- While speaking: each confirmed fragment is streamed to the cursor via Ctrl+V as the service finalizes it
- Release space: the remaining final transcript is pasted the same way
- A small black level bar near the top of the screen animates with your voice while recording

## Requirements

- Windows 10/11
- Rust toolchain to build
- A GitHub account
- A microphone
- Outbound HTTPS, optional `HTTPS_PROXY` is honored

## Build

```powershell
cargo build --release
target\release\stt-input.exe
```

## Usage

Hold right Ctrl and speak, release to paste

| Flag | Effect |
| --- | --- |
| none | Run the dictation loop in the background, no console window |
| `-v`, `--verbose` | Write logs to `stt-input.log` next to the exe (overwritten on each start) |
| `--install-autostart` | Launch at login (HKCU Run key) |
| `--uninstall-autostart` | Remove the autostart entry |
| `--print-auth-mode` | Show the active credential source |
| `--dump-wav` | Record 3 s to `tmp/cap.wav` for mic diagnosis |
| `--paste-with-shift` | Paste with Ctrl+Shift+V (for terminals) |

## Authentication

Tokens resolve in this order:

1. `STT_GITHUB_TOKEN` environment variable
2. `token.json` next to the exe (portable cache)
3. `gh auth token` from the GitHub CLI credential store
4. GitHub device flow — a code is printed, open <https://github.com/login/device> and enter it

Tokens from gh or device flow are cached to `token.json` next to the exe, so the next start skips login Delete that file to force a fresh login The file travels with the folder, nothing is written to `~` or AppData

## Project layout

```
main.rs            entry point
src/auth.rs        GitHub token: env → token.json → gh → device flow
src/stt.rs         MAI transcription WebSocket client (proxy + TLS)
src/capture.rs     microphone capture, 16 kHz mono s16le, 100 ms chunks
src/paste.rs       clipboard injection, ported from Hex
src/hotkey.rs      right Ctrl hotkey polling
src/overlay.rs     voice level bar window
src/autostart.rs   registry autostart
```

## Limitations and known issues

- Windows only: paste, hotkey, overlay and autostart call Win32 APIs
- Space is a common typing key: keystrokes while typing trigger dictation
- The default recording device is used as-is, Voicemeeter users must route the mic into the default recording endpoint (verify with `--dump-wav`)
- Terminal apps often need `--paste-with-shift`
- `token.json` is stored in plain text next to the exe, a deliberate portability trade-off
- The MAI endpoint belongs to Microsoft: personal-use reuse of the VS Code dictation quota may be rate limited or restricted at any time

## Credits

- [Hex](https://github.com/anomalyco/hex) (MIT) — the Windows paste engine is ported from its `WindowsPaster`
- [VS Code](https://github.com/microsoft/vscode) (MIT) — the dictation client protocol is reimplemented from its sources

## License

[GPL-3.0-or-later](LICENSE)
