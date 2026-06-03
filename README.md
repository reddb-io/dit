<p align="center">
  <h1 align="center">🎙️ dictator</h1>
  <p align="center"><strong>Push-to-toggle voice dictation for your whole desktop.</strong></p>
  <p align="center">Hit a key. Talk. The words land in whatever app is focused. Hit it again to stop.</p>
</p>

<p align="center">
  <a href="https://github.com/reddb-io/dictator/actions/workflows/ci.yml"><img src="https://img.shields.io/github/actions/workflow/status/reddb-io/dictator/ci.yml?style=flat-square&label=CI" alt="CI"></a>
  <a href="https://github.com/reddb-io/dictator/releases"><img src="https://img.shields.io/github/v/release/reddb-io/dictator?style=flat-square" alt="Release"></a>
  <a href="./LICENSE"><img src="https://img.shields.io/badge/license-MIT-blue?style=flat-square" alt="License"></a>
  <img src="https://img.shields.io/badge/platforms-Linux%20%C2%B7%20macOS%20%C2%B7%20Windows-informational?style=flat-square" alt="Platforms">
  <img src="https://img.shields.io/badge/built%20with-Rust-orange?style=flat-square&logo=rust" alt="Rust">
</p>

---

`dictator` streams your microphone to [ElevenLabs **Scribe v2 Realtime**](https://elevenlabs.io/docs/api-reference/speech-to-text)
and pastes each finalized sentence into the focused window the instant it's ready — no app
to switch to, no transcript window to copy out of. It's a single static binary, written in
Rust, that runs the same way on **Linux, macOS and Windows**.

It started as [`whisperflow.py`](https://gist.github.com/filipeforattini/a8c3c91c093245566db924c4d8c75ac7) —
a Linux/Wayland-only Python script. This is the portable, dependency-light rewrite.

```
   ┌─ press F9 ─────────────────────────────────────────────── press F9 ─┐
   ▼                                                                      ▼
 mic ──► resample 16 kHz ──► WebSocket ──► Scribe v2 Realtime
                                                  │
                       committed_transcript ◄─────┘
                                  │
                  clipboard  ──►  Ctrl/⌘+V  ──►  ✶ focused app
```

---

## Install

### Download a release

Grab the binary for your platform from the [**Releases**](https://github.com/reddb-io/dictator/releases) page:

| Platform | Asset |
|---|---|
| Linux x86_64 | `dictator-linux-x86_64` |
| Linux aarch64 | `dictator-linux-aarch64` |
| macOS Apple Silicon | `dictator-macos-aarch64` |
| macOS Intel | `dictator-macos-x86_64` |
| Windows x86_64 | `dictator-windows-x86_64.exe` |

```bash
curl -fsSL https://github.com/reddb-io/dictator/releases/latest/download/dictator-linux-x86_64 -o dictator
chmod +x dictator && sudo mv dictator /usr/local/bin/
```

Every asset ships a `.sha256` sidecar — verify with `shasum -a 256 -c dictator-<asset>.sha256`.

### Build from source

```bash
cargo install --path .          # or: cargo build --release
```

<details>
<summary><strong>Linux build dependencies</strong></summary>

```bash
sudo apt-get install -y \
  libasound2-dev libxdo-dev libxi-dev libxtst-dev \
  libxcb1-dev libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev \
  libdbus-1-dev pkg-config
```

macOS and Windows need no extra system packages.
</details>

---

## Configure

Put your ElevenLabs API key in `~/.dictator.env`:

```bash
echo 'ELEVENLABS_API_KEY=sk_your_key_here' > ~/.dictator.env
```

(or export `ELEVENLABS_API_KEY`, or pass `--env-file <path>`).

---

## Use

```bash
dictator                    # F9 toggle, Portuguese
dictator --language en      # English
dictator --hotkey F8        # any of F1..F12
dictator --device "Fifine"  # prefer an input device by name substring
dictator --list-devices     # list inputs and exit
```

Press **F9** → speak → press **F9** again. `Ctrl+C` quits. Crank up logs with `RUST_LOG=dictator=debug`.

| Flag | Default | Description |
|---|---|---|
| `--language` | `pt` | Scribe language code (`pt`, `en`, `es`, …) |
| `--model` | `scribe_v2_realtime` | Scribe realtime model id |
| `--hotkey` | `F9` | Toggle key (`F1`..`F12`) |
| `--device` | *system default* | Input device name substring |
| `--env-file` | `~/.dictator.env` | Path to the key file |
| `--list-devices` | — | Print input devices and exit |

---

## How it works

`dictator` is faithful to the original script's streaming contract:

- **`partial_transcript`** events are **ignored** — they're an unstable preview, and typing
  them character-by-character would scramble the output.
- **`committed_transcript`** events are stable per-segment text, committed by the server's
  Voice Activity Detection on each pause. Every one is **pasted immediately**.
- Identical consecutive segments are **de-duplicated** so nothing lands twice.
- On stop, an empty `commit: true` frame **flushes the last open segment**, then the
  clipboard is **restored** to whatever you had before.

| Concern | Crate | Replaces (`whisperflow.py`) |
|---|---|---|
| Global hotkey | [`rdev`](https://crates.io/crates/rdev) | `evdev` + `input` group |
| Audio capture | [`cpal`](https://crates.io/crates/cpal) | `parec` (PulseAudio) |
| WebSocket | [`tokio-tungstenite`](https://crates.io/crates/tokio-tungstenite) | `websockets` |
| Clipboard | [`arboard`](https://crates.io/crates/arboard) | `wl-copy` / `wl-paste` |
| Paste keystroke | [`enigo`](https://crates.io/crates/enigo) | `ydotool key ctrl+v` |
| Notifications | [`notify-rust`](https://crates.io/crates/notify-rust) | `notify-send` |

---

## Platform notes

> [!IMPORTANT]
> **Linux / Wayland** restricts global key-grabbing and synthetic input for security.
> Capture and paste may be limited depending on your compositor — running under **X11 /
> XWayland** is the reliable path. (The original script worked around this with `ydotool`
> and the `input` group.) On **X11**, everything works out of the box.

> [!NOTE]
> **macOS** — grant **Accessibility** permission (System Settings → Privacy & Security →
> Accessibility) so `dictator` can read the hotkey and send the paste keystroke. The paste
> modifier is automatically **⌘** instead of Ctrl.
>
> **Windows** — works out of the box.

---

## Releases & CI

Tag-driven, built on [Blacksmith](https://blacksmith.sh) runners — Linux x86_64/aarch64 compile
**natively** (no QEMU/cross), macOS ships universal coverage and Windows an `.exe`. Pushing a
`v*.*.*` tag builds every target, attaches the binaries plus `.sha256` sidecars, and renders the
changelog from conventional commits via [`git-cliff`](https://git-cliff.org).

```bash
git tag v0.1.0 && git push origin v0.1.0   # → builds, smoke-tests, publishes the GitHub Release
```

---

## License

MIT © [RedDB.io](https://github.com/reddb-io)
