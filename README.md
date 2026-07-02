<p align="center">
  <h1 align="center">🎙️ dit</h1>
  <p align="center"><strong>Push-to-toggle voice dictation for your whole desktop.</strong></p>
  <p align="center">Hit a key. Talk. The words land in whatever app is focused. Hit it again to stop.</p>
</p>

<p align="center">
  <a href="https://github.com/reddb-io/dit/actions/workflows/ci.yml"><img src="https://img.shields.io/github/actions/workflow/status/reddb-io/dit/ci.yml?style=flat-square&label=CI" alt="CI"></a>
  <a href="https://github.com/reddb-io/dit/releases"><img src="https://img.shields.io/github/v/release/reddb-io/dit?style=flat-square" alt="Release"></a>
  <a href="./LICENSE"><img src="https://img.shields.io/badge/license-MIT-blue?style=flat-square" alt="License"></a>
  <img src="https://img.shields.io/badge/platforms-Linux%20%C2%B7%20macOS%20%C2%B7%20Windows-informational?style=flat-square" alt="Platforms">
  <img src="https://img.shields.io/badge/built%20with-Rust-orange?style=flat-square&logo=rust" alt="Rust">
</p>

---

`dit` is a cross-platform voice dictation tool: press a key, speak, and the transcript is pasted into whatever app is focused — no switching windows, no copy-paste.

It supports two transcription engines:

- **ElevenLabs** (default, `--engine elevenlabs`) — streams your mic to [ElevenLabs Scribe v2 Realtime](https://elevenlabs.io/docs/api-reference/speech-to-text), real-time word-by-word delivery. (`--engine cloud` still works as an alias.)
- **Local** (`--engine local`) — records while you hold/toggle the key, then transcribes fully **offline** with a [Whisper](https://openai.com/research/whisper) model (pure-Rust via `candle`). No API key, no network, no cost per use.

It's a single static binary written in Rust, identical on **Linux, macOS and Windows**.

```
── ElevenLabs ─────────────────────────────────────────────────────────────
mic ──► resample 16 kHz ──► WebSocket ──► Scribe v2 Realtime
                                                 │
                      committed_transcript ◄──────┘
                                 │
                      typed as keystrokes  ──►  ✶ focused app

── Local (offline) ────────────────────────────────────────────────────────
[hold key] mic ──► resample 16 kHz ──► buffer
[release]  buffer ──► Whisper (candle, CPU) ──► transcript
                                                      │
                                           typed as keystrokes  ──►  ✶ focused app
```

---

## Install

### One-liner

**Linux / macOS:**
```bash
curl -fsSL https://raw.githubusercontent.com/reddb-io/dit/main/install.sh | bash
```

**Windows (PowerShell):**
```powershell
irm https://raw.githubusercontent.com/reddb-io/dit/main/install.ps1 | iex
```

The installer detects your OS/arch, picks the best build, downloads and verifies the `.sha256`, installs to `~/.local/bin` (Unix) or `%LOCALAPPDATA%\Programs\dit` (Windows), and walks you through the API key, runtime deps, and autostart service.

```bash
# fully non-interactive
curl -fsSL .../install.sh | bash -s -- --yes --api-key sk_... --with-service
```

### Staying up to date — `dit update`

```bash
dit update            # upgrade to the latest release (no-op if already current)
dit update --check    # just report whether a newer version exists
dit update --force    # re-download and reinstall the current version
dit update --version v0.3.0   # pin a specific release
```

### Manual download

Grab the binary for your platform from the [**Releases**](https://github.com/reddb-io/dit/releases) page:

| Platform | Asset |
|---|---|
| Linux x86_64 | `dit-linux-x86_64` |
| Linux aarch64 | `dit-linux-aarch64` |
| Linux armv7 (32-bit ARM) | `dit-linux-armv7` |
| Linux x86_64 — fully static | `dit-linux-x86_64-static` |
| Linux aarch64 — fully static | `dit-linux-aarch64-static` |
| macOS Apple Silicon | `dit-macos-aarch64` |
| macOS Intel | `dit-macos-x86_64` |
| Windows x86_64 | `dit-windows-x86_64.exe` |

Every asset ships a `.sha256` sidecar — verify with `shasum -a 256 -c dit-<asset>.sha256`.

> [!NOTE]
> **Distro-portable by design.** The default Linux binaries target glibc ≥ 2.28 (Ubuntu 18.04+, Debian 10+). For older or musl hosts, use the `*-static` variant (ALSA linked in, no system deps).

> [!NOTE]
> **Dependencies on Linux.** The headless dictation binary's only runtime dep is `libasound2`. The settings GUI (`dit settings`) additionally needs `libGL` and `libxkbcommon` — the installer offers to add them.

### Build from source

```bash
cargo build --release                        # ElevenLabs-only (lean)
cargo build --release --features local       # add local Whisper engine
cargo build --release --features gui         # add settings GUI
cargo build --release --features local,gui   # everything (release default)
```

<details>
<summary><strong>Linux build dependencies</strong></summary>

```bash
sudo apt-get install -y libasound2-dev pkg-config
# for --features gui:
sudo apt-get install -y libxkbcommon-dev libgl1-mesa-dev
```
</details>

---

## Configure

### ElevenLabs engine (API key)

```bash
echo 'ELEVENLABS_API_KEY=sk_your_key_here' > ~/.dit.env
```

Or use the settings GUI: `dit settings` → Account tab.

### Persistent config (`~/.dit/config.toml`)

All CLI flags can be persisted:

```toml
language = "pt"
engine = "elevenlabs"
mode = "toggle"
hotkey = "F9"
no_filler = false
```

`dit settings` reads and writes this file. CLI flags always override it.

---

## Use

```bash
dit                                   # ElevenLabs engine, F9 toggle, Portuguese
dit --engine local                    # offline Whisper engine
dit --engine local --mode hold        # hold key to record, release to transcribe
dit --language en                     # English
dit --language auto                   # auto-detect spoken language
dit --hotkey RightAlt                 # single modifier key as hotkey
dit --hotkey "RightCtrl+F9"           # key combo
dit --hotkey F8                       # any F1..F12
dit --device "Fifine"                 # prefer an input device by name substring
dit --no-filler                       # strip "uh"/"um" from output
dit --keyterm RedDB --keyterm Scribe  # bias toward names/jargon (ElevenLabs, repeatable)
dit --vad-silence 0.8                 # commit faster on shorter pauses (ElevenLabs)
dit --region eu                       # EU data residency (ElevenLabs)
dit --list-devices                    # list inputs and exit
dit doctor                            # diagnose mic/keyboard/session permissions
dit settings                          # open the settings GUI
dit update                            # update to the latest release
dit update --check                    # only report whether an update is available
```

While recording, the tray icon becomes a **VU meter**: red bars = silence, green = healthy speech, yellow/red = loud input. `Ctrl+C` quits.

### Recording modes

| Mode | Behaviour |
|---|---|
| `--mode toggle` (default) | Press once to start, press again to stop |
| `--mode hold` | Hold the key to record, release to transcribe |

### Hotkeys

Any key combo works, not just F-keys:

```bash
dit --hotkey F9              # classic
dit --hotkey RightAlt        # single modifier
dit --hotkey RightCtrl       # single modifier
dit --hotkey "RightAlt+F9"   # combo
```

> [!NOTE]
> `Fn` is not capturable on most platforms — use a regular key or modifier instead.

### Flag reference

| Flag | Default | Description |
|---|---|---|
| `--engine` | `elevenlabs` | `elevenlabs` (alias: `cloud`) or `local` |
| `--mode` | `toggle` | `toggle` or `hold` |
| `--language` | `pt` | Language code, or `auto` for auto-detection |
| `--model` | `scribe_v2_realtime` | Scribe model (ElevenLabs) or Whisper model name (local) |
| `--hotkey` | `F9` | Toggle/hold key — F1..F12, modifier keys, or combos |
| `--device` | *system default* | Input device name substring |
| `--no-filler` | off | Remove filler words (`no_verbatim`) |
| `--keyterm <TERM>` | — | Bias toward a term; repeatable (ElevenLabs only) |
| `--vad-silence <SECS>` | `1.5` | Silence before segment commits (ElevenLabs only) |
| `--region` | `global` | API region: `global`, `us`, `eu`, `in` (ElevenLabs only) |
| `--no-preview` | off | Disable live terminal preview |
| `--paste-shift` | off | Linux: paste with `Ctrl+Shift+V` (for terminals) |
| `--type` | off | Linux: type via uinput instead of clipboard |
| `--layout` | `auto` | Linux `--type` keyboard layout: `auto`, `us`, `abnt2` |
| `--env-file` | `~/.dit.env` | Path to the API key file |
| `--list-devices` | — | Print input devices and exit |

---

## Local engine & model management

The local engine runs Whisper inference fully on-device — no internet, no API key, no per-use cost.

```bash
# Manage models
dit models list                          # show available models and which are installed
dit models download whisper-tiny-local   # download the offline model (~42 MB)
dit models path                          # print the models directory (~/.dit/models/)
dit models rm whisper-tiny-local         # delete a downloaded model

# Dictate offline
dit --engine local                       # uses whisper-tiny-local by default
```

Each model is a quantised GGUF plus its tokenizer, downloaded from HuggingFace, verified by SHA-256, and stored in `~/.dit/models/`. The Models tab in `dit settings` also lets you manage them visually. Long recordings are transcribed in 30-second windows, so nothing is cut off.

Available models: `whisper-tiny-local` (multilingual tiny, q8_0 — alias: `tiny`). Larger quantised builds will be added as they become available upstream.

---

## Settings GUI

```bash
dit settings     # open the settings window
```

| Tab | Contents |
|---|---|
| **General** | Language, hotkey, recording mode, engine |
| **Audio** | Input device picker + live VU meter |
| **Models** | Download and manage local Whisper models |
| **Account** | ElevenLabs API key |
| **About** | Version info |

All settings persist to `~/.dit/config.toml` and are shared with the CLI. The tray's **Settings…** menu item also opens this window.

---

## File transcription

Transcribe existing audio files with either engine:

```bash
dit transcribe meeting.wav                       # ElevenLabs engine, stdout
dit transcribe --engine local interview.mp3      # local Whisper, no API key needed
dit transcribe lecture.flac --out lecture.txt    # write to file
dit transcribe --engine local *.wav              # batch, multiple files
dit transcribe --language en talk.wav            # pick the language (or `auto`)
```

Supported formats: `wav`, `mp3`, `flac`, `m4a`.

---

## Tray controls

The system tray provides runtime controls without restarting:

- **Switch input device** — submenu with all detected mics
- **Switch language** — change on the fly
- **Switch mode** — toggle ↔ hold, applied to the very next key press
- **Switch transcript style** — verbatim ↔ remove fillers
- **Switch engine** — ElevenLabs ↔ local (in builds with the local engine)
- **Settings…** — open the settings GUI
- **Open last transcript** — opens the most recent session log
- **Pause** — the hotkey can't *start* a recording; stopping always works

---

## Nothing gets lost

- **Live terminal preview** — unstable partials appear on a self-rewriting line; only finalized text is typed into the focused app.
- **Session logs** — every committed segment is appended to `~/.dit/sessions/session-<ts>.txt`. Logs are pruned automatically (last 30 days / 100 sessions).

---

## Run it always (autostart)

```bash
dit service install                     # autostart with defaults
dit service install --language en --no-filler   # bake in your flags
dit service status
dit service uninstall
```

| OS | What it installs |
|---|---|
| **Linux** | systemd `--user` service (`journalctl --user -u dit -f`) or XDG autostart |
| **macOS** | LaunchAgent in `~/Library/LaunchAgents` |
| **Windows** | Task Scheduler logon task |

> [!IMPORTANT]
> It installs a **user-session agent**, not a root/system daemon — `dit` must live inside your graphical session to access the keyboard, audio, and display.

---

## Platform notes

> [!IMPORTANT]
> **Linux** — dit uses a kernel-level input backend (evdev + uinput), works on both X11 and Wayland. One-time setup:
>
> ```bash
> sudo usermod -aG input $USER
> echo 'KERNEL=="uinput", GROUP="input", MODE="0660"' | sudo tee /etc/udev/rules.d/99-uinput.rules
> sudo udevadm control --reload && sudo udevadm trigger
> # log out and back in
> ```
>
> In terminals, use `--paste-shift` (`Ctrl+Shift+V`) or `--type` (uinput typing, bypasses clipboard entirely — avoids GNOME/Wayland intermittently interpreting the clipboard as an image after a screenshot copy).
>
> `--type` is layout-aware: dit detects the active keyboard layout (XKB env → GNOME settings → setxkbmap → localectl → locale) and maps characters accordingly. `us` and `abnt2` (Brazilian) are supported — on ABNT2, `ç` is typed directly and dead-key accents ride the clipboard fallback. Pin it with `--layout us|abnt2` (or `layout = "abnt2"` in config.toml) if detection guesses wrong; `dit doctor` shows what was detected.

> [!NOTE]
> **macOS** — grant **Accessibility** permission (System Settings → Privacy & Security → Accessibility).
>
> **Windows** — works out of the box.

---

## Releases & CI

[`release-plz`](https://release-plz.dev) reads [Conventional Commits](https://www.conventionalcommits.org) and opens a release PR that bumps the version (`feat` → minor, `fix` → patch). Merging creates the tag, which triggers the release build on [Blacksmith](https://blacksmith.sh) — all 8 targets, stripped, smoke-tested, published with `.sha256` sidecars and a changelog.

```
commits (feat:/fix:/…) ─► release-plz PR ─► merge ─► tag vX.Y.Z ─► binaries + GitHub Release
```

---

## License

MIT © [RedDB.io](https://github.com/reddb-io)
