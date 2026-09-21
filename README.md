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

| Platform | Asset | |
|---|---|---|
| Linux x86_64 | `dit-linux-x86_64` | always published |
| Linux aarch64 | `dit-linux-aarch64` | always published |
| macOS Apple Silicon | `dit-macos-aarch64` | always published |
| macOS Intel | `dit-macos-x86_64` | always published |
| Windows x86_64 | `dit-windows-x86_64.exe` | always published |
| Linux armv7 (32-bit ARM) | `dit-linux-armv7` | best-effort |
| Linux x86_64 — fully static | `dit-linux-x86_64-static` | best-effort |
| Linux aarch64 — fully static | `dit-linux-aarch64-static` | best-effort |
| Windows ARM64 | `dit-windows-aarch64.exe` | best-effort |

The five *always published* assets are gated in CI: a release is not created unless
all five exist and match their checksums. The *best-effort* ones are cross-compiled
and may be absent from a given release — `install.sh`, `install.ps1` and `dit update`
all fall back automatically (Windows ARM64 → the x86_64 build under emulation;
Linux → the glibc/static counterpart).

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
mkdir -p ~/.red/dit && chmod 700 ~/.red/dit
echo 'ELEVENLABS_API_KEY=sk_your_key_here' > ~/.red/dit/.env
chmod 600 ~/.red/dit/.env
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

`dit settings` reads and writes this file. CLI flags always override it. Every key can also come from a `DIT_*` environment variable (e.g. `DIT_DELIVERY=paste`); the order is *defaults < config.toml < environment < CLI flags*.

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
| `--paste-shift` | off | Linux: force `Ctrl+Shift+V` when auto focus detection is unavailable |
| `--type` | off | Linux: type via uinput instead of clipboard |
| `--delivery` | `auto` | Linux: `auto` (zellij-aware, see [Terminals and zellij](#terminals-and-zellij)), `paste`, or `type` |
| `--layout` | `auto` | Linux `--type` keyboard layout: `auto`, `us`, `abnt2` |
| `--env-file` | `~/.red/dit/.env` | Path to the API key file |
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

Settings persist to `~/.dit/config.toml`; the ElevenLabs key lives separately at
`~/.red/dit/.env`. Both are shared with the CLI. The tray's **Settings…** menu
item opens this window, and a saved key is used by the next recording.

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
> With the default `delivery = "auto"`, dit detects the focused app for every transcript and uses `Ctrl+Shift+V` in known terminals or `Ctrl+V` in other apps. Use `--paste-shift` to force the terminal chord when focus detection is unavailable, or `--type` for uinput typing (bypasses the clipboard almost entirely — avoiding GNOME/Wayland intermittently interpreting the clipboard as an image after a screenshot copy).
>
> `--type` is layout-aware: dit detects the active keyboard layout (XKB env → GNOME settings → setxkbmap → localectl → locale) and maps characters accordingly. `us` and `abnt2` (Brazilian) are supported — on ABNT2, `ç` is typed directly and dead-key accents ride the clipboard fallback. Pin it with `--layout us|abnt2` (or `layout = "abnt2"` in config.toml) if detection guesses wrong; `dit doctor` shows what was detected.

### Terminals and zellij

By default (`delivery = "auto"`) dit checks which window is focused before each delivery. When it is a known terminal — Alacritty, kitty, WezTerm, foot, Ghostty, GNOME Terminal, Ptyxis, GNOME Console or Konsole — and a [zellij](https://zellij.dev) client runs inside it, dit writes the transcript straight into that session's focused pane as a **bracketed paste**:

```text
zellij --session <name> action write ESC[200~   # only if the app enabled bracketed paste
zellij --session <name> action write <text>
zellij --session <name> action write ESC[201~
```

Shells, editors and agent TUIs (bash, fish, vim, nano, Claude Code, Codex, redcode) then see pasted text, so a newline never submits, and nothing depends on the terminal's paste shortcut or the clipboard. Escape and other control characters (everything but tabs and newlines) are stripped from the transcript first, so dictated text can never end the paste early. A detected terminal without a reachable zellij session falls back to `Ctrl+Shift+V`; other apps use `Ctrl+V`. If focus is unknown, dit keeps the configured fallback (`Ctrl+V`, `--paste-shift`, or `--type`). Modifier state and `V` are emitted in separate input frames so Wayland compositors cannot mistake the shortcut for a literal `v`.

How the session is found: dit walks the focused window's process tree for a `zellij` client and reads the session from its command line (`zellij attach NAME`, `zellij --session NAME`) or its `ZELLIJ_SESSION_NAME`; it talks to the server with that client's own binary and `ZELLIJ_SOCKET_DIR`. A client that doesn't name its session is matched by the window title, or by being the only live session. When the focused window's pid is unknown, the only live session (or the one the title names) is used.

Focused-window detection:

| Session | Provider |
|---|---|
| **GNOME (Wayland or X11)** | the bundled Shell extension `dit-focus@reddb.io` (below) |
| **Other X11 desktops** | `_NET_ACTIVE_WINDOW` / `WM_CLASS` / `_NET_WM_PID`, built in |
| **Other Wayland compositors** | not detected yet — delivery stays on the paste chord |

GNOME gives ordinary apps no way to ask which window is focused on Wayland, so dit ships a tiny extension that answers `io.reddb.dit.Focus.Get() → (app_id, wm_class, pid, title)` on the session bus when dit asks. It shows no UI and does no work between calls. The installer offers it on GNOME, or:

```bash
dit gnome-extension install     # writes ~/.local/share/gnome-shell/extensions/dit-focus@reddb.io and enables it
# log out and back in: GNOME Shell on Wayland only loads new extensions at login
dit gnome-extension status      # installed / enabled / answering
dit doctor                      # run inside zellij: shows the route dit would take
dit gnome-extension uninstall   # disable and remove it
```

Pin the old behaviour with `--delivery paste` (always the paste chord) or `--delivery type` (always typing), or `delivery = "paste"` in `config.toml`. `--type` together with the default `auto` keeps typing as the fallback outside zellij.

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
