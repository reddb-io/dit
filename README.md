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

`dit` streams your microphone to [ElevenLabs **Scribe v2 Realtime**](https://elevenlabs.io/docs/api-reference/speech-to-text)
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

The installer detects your OS/arch, picks the **best build for your host** (see below), downloads
the matching binary, verifies its `.sha256`, installs or updates it (`~/.local/bin` on Unix,
`%LOCALAPPDATA%\Programs\dit` on Windows) and puts it on your `PATH`. If `dit` is already installed,
it reads the local `dit --version`, compares it with the requested/latest release, skips a no-op
reinstall when already current, and updates older local binaries in place. On Linux it also restarts
an active `dit.service` so the running desktop agent picks up the new binary.

It then walks you through the rest interactively: **prompts for your ElevenLabs API key**, offers to
**install the runtime libraries** (detecting apt/dnf/pacman/zypper), and offers to **set up the
autostart service** — then smoke-tests that the binary runs.

```bash
# fully non-interactive, e.g. for provisioning
curl -fsSL .../install.sh | bash -s -- --yes --api-key sk_... --with-service
# other flags
curl -fsSL .../install.sh | bash -s -- --version v0.1.0
curl -fsSL .../install.sh | bash -s -- --install-dir /usr/local/bin --skip-deps --no-service
```

### Staying up to date — `dit update`

Once installed, dit upgrades itself. **`dit update`** is a first-class, self-contained command — no
`curl | bash`, no re-running the installer:

```bash
dit update            # upgrade to the latest release (no-op if already current)
dit update --check    # just report whether a newer version exists
dit update --force    # re-download and reinstall the current version
dit update --version v0.2.4   # pin a specific release
```

What it does, end to end:

- **Resolves** the latest release from the GitHub API (or the tag you pin).
- **Picks the right asset for *this* host** — correct arch, and the glibc-portable vs. fully-static
  `-static` variant inferred from how the running binary itself was built.
- **Downloads over HTTPS** (rustls, no system OpenSSL) and **verifies the published `SHA-256`** before
  touching anything — a mismatch aborts the update.
- **Atomically replaces the running executable** in place (safe on Windows too), then on Linux
  **restarts an active `dit.service`** so the desktop agent picks up the new binary.
- **Idempotent:** running it twice in a row just prints *"already the latest release — nothing to
  update."*

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

```bash
curl -fsSL https://github.com/reddb-io/dit/releases/latest/download/dit-linux-x86_64 -o dit
chmod +x dit && sudo mv dit /usr/local/bin/
```

Every asset ships a `.sha256` sidecar — verify with `shasum -a 256 -c dit-<asset>.sha256`.

> [!NOTE]
> **Distro-portable by design.** The default Linux `x86_64`/`aarch64`/`armv7` binaries are built
> with [`cargo-zigbuild`](https://github.com/rust-cross/cargo-zigbuild) against an **old glibc floor
> (2.28)**, so a *single* binary runs on every Ubuntu since 18.04 (20.04 / 22.04 / 24.04 / 26.04) and
> Debian 10+ — no more "version `GLIBC_2.39' not found" when you move between releases. If your host
> glibc is older than 2.28, or you're on a musl distro like Alpine, grab the `*-static` variant
> instead (ALSA is linked in, so it needs nothing on the system). The install script and `dit update`
> detect this and pick the right one for you automatically.

> [!NOTE]
> **Dependencies — there's essentially one, and the installer handles it.** The prebuilt Linux
> binary is self-contained: its *only* runtime dependency is the ALSA shared library `libasound2`
> (audio) — no `libxdo`, `wl-clipboard`, GTK or appindicator (input, clipboard and the tray are all
> pure-Rust). The install script checks for it and offers to install it via your package manager
> (`apt`/`dnf`/`pacman`/`zypper`); with `--yes` it just does it. The `*-static` build links ALSA in,
> so it needs **nothing at all** — use it (or `--static`) if you'd rather not touch system packages.
> `libasound2-dev` and `pkg-config` are only needed to *compile from source*, never to run a release.
> macOS and Windows need nothing extra.

### Build from source

```bash
cargo install --path .          # or: cargo build --release
```

<details>
<summary><strong>Linux build dependencies</strong></summary>

```bash
sudo apt-get install -y libasound2-dev pkg-config
```

(That's the only build dependency — the Linux input, clipboard and tray are all
pure-Rust, so no X11/GTK/xdo/appindicator dev packages are needed.)

macOS and Windows need no extra system packages.
</details>

---

## Configure

Put your ElevenLabs API key in `~/.dit.env`:

```bash
echo 'ELEVENLABS_API_KEY=sk_your_key_here' > ~/.dit.env
```

(or export `ELEVENLABS_API_KEY`, or pass `--env-file <path>`).

---

## Use

```bash
dit                              # F9 toggle, Portuguese
dit --language en                # English
dit --hotkey F8                  # any of F1..F12
dit --device "Fifine"            # prefer an input device by name substring
dit --no-filler                  # strip "uh"/"um" from the output
dit --keyterm RedDB --keyterm Scribe   # bias toward names/jargon (repeatable)
dit --vad-silence 0.8            # commit faster on shorter pauses
dit --region eu                  # EU data residency
dit --list-devices               # list inputs and exit
dit doctor                       # diagnose mic/keyboard/session permissions
dit update                       # update to the latest release (no-op if current)
dit update --check               # only report whether an update is available
```

Press **F9** → speak → press **F9** again. While recording, the tray icon becomes a high-contrast **VU meter**: dark red bars mean silence/no input, green bars mean healthy speech level, and yellow/red bars mean loud input. `Ctrl+C` quits. Crank up logs with `RUST_LOG=dit=debug`.

| Flag | Default | Description |
|---|---|---|
| `--language` | `pt` | Scribe language code (`pt`, `en`, `es`, …) |
| `--model` | `scribe_v2_realtime` | Scribe realtime model id |
| `--hotkey` | `F9` | Toggle key (`F1`..`F12`) |
| `--device` | *system default* | Input device name substring |
| `--no-filler` | off | Remove filler words (`no_verbatim`) |
| `--keyterm <TERM>` | — | Bias the model toward a term; repeatable |
| `--vad-silence <SECS>` | `1.5` | Silence before a segment commits — lower = snappier |
| `--region` | `global` | API region: `global`, `us`, `eu`, `in` |
| `--no-preview` | off | Disable the live terminal preview |
| `--env-file` | `~/.dit.env` | Path to the key file |
| `--list-devices` | — | Print input devices and exit |

`dit` is resilient to desktop hardware churn: on Linux it monitors `/dev/input`
for keyboards plugged in after startup, debounces duplicate hotkey events from
multi-event keyboards, ranks real capture devices ahead of noisy ALSA aliases,
and retries/fails over if a microphone stream disappears.

> [!TIP]
> For the sharpest transcripts: pass names and jargon with `--keyterm` (e.g. `--keyterm Kubernetes`),
> turn on `--no-filler` for clean prose, and lower `--vad-silence` (e.g. `0.8`) if you want each
> sentence to land sooner at the cost of slightly more fragmentation.

---

## Run it always (autostart)

`dit` is a long-running process — it has to be, since *something* must listen for the hotkey. To
have it start at login and stay ready, install it as a **user service**:

```bash
dit service install                     # autostart with defaults
dit service install --language en --no-filler   # …or bake in your flags
dit service status
dit service uninstall
```

| OS | What it installs |
|---|---|
| **Linux** | a systemd `--user` service (`journalctl --user -u dit -f` for logs), or an XDG autostart `.desktop` entry if there's no user systemd |
| **macOS** | a LaunchAgent in `~/Library/LaunchAgents` |
| **Windows** | a logon task via Task Scheduler |

> [!IMPORTANT]
> It installs a **user-session agent**, not a root/system daemon — and that's deliberate. A system
> service runs isolated from your login session (no display, no audio, no input access on Linux; in
> "session 0" with no desktop on Windows), so it physically *couldn't* read your keyboard or type
> into your apps. `dit` must live inside your graphical session.

---

## Nothing gets lost

Two surfaces keep your words safe without ever risking the focused app's text:

- **Live terminal preview** — the unstable `partial_transcript` "materializes" on a
  single, self-rewriting line in your terminal. You watch the sentence form in real time,
  but the app in focus **only ever receives committed (finalized) text**. No backspace-and-
  retype into a window we don't control, so there's no way to clobber what's already there.
- **Append-only transcript log** — every committed segment is written to
  `~/.dit/sessions/session-<ts>.txt`. If typing fails, the app loses focus, or the
  connection drops, the text is still on disk. A previewed tail that never got a final commit
  is recorded too (marked `# [uncommitted]`) — saved for recovery, **not** typed late.

```
… materializing this senten     ← live preview (dim, rewrites in place)
This sentence is now committed.  ← locked in, typed into the app + logged
```

## How it works

`dit` is faithful to the original script's streaming contract:

- **`partial_transcript`** events are **ignored** — they're an unstable preview, and typing
  them character-by-character would scramble the output.
- **`committed_transcript`** events are stable per-segment text, committed by the server's
  Voice Activity Detection on each pause. Every one is **typed into the focused app immediately**.
- Identical consecutive segments are **de-duplicated** so nothing lands twice.
- On stop, an empty `commit: true` frame **flushes the last open segment**.
- While audio is streaming, `dit` computes a lightweight RMS level locally and updates the tray icon
  about 5 times per second as a chunky 5-bar meter. Only the level is used for the icon; no audio is
  written to disk by default.

Text delivery is platform-specific. On Linux/Wayland, `dit` sets the clipboard and emits the paste
chord through `/dev/uinput` (`Ctrl+V`, or `Ctrl+Shift+V` with `--paste-shift` for terminals); this
makes delivery much more reliable than trying to synthesize every character individually. The text is
also appended to the session log, so a failed paste can still be recovered.

| Concern | Crate | Replaces (`whisperflow.py`) |
|---|---|---|
| Global hotkey | [`rdev`](https://crates.io/crates/rdev) | `evdev` + `input` group |
| Audio capture | [`cpal`](https://crates.io/crates/cpal) | `parec` (PulseAudio) |
| WebSocket | [`tokio-tungstenite`](https://crates.io/crates/tokio-tungstenite) | `websockets` |
| Text injection | [`enigo`](https://crates.io/crates/enigo) | `wl-copy` + `ydotool key ctrl+v` |
| Notifications | [`notify-rust`](https://crates.io/crates/notify-rust) | `notify-send` |

---

## Platform notes

> [!IMPORTANT]
> **Linux** — dit uses a kernel-level input backend (works the same on **X11 and Wayland**, since
> X11 global grabs and X11 input don't reach native Wayland apps): it reads the hotkey from
> `/dev/input` (evdev) and types by setting the clipboard and emitting the paste chord through
> `/dev/uinput`. No external libraries or tools — but it needs a one-time permission setup, which the
> installer offers to do:
>
> ```bash
> sudo usermod -aG input $USER          # read the keyboard
> echo 'KERNEL=="uinput", GROUP="input", MODE="0660"' | sudo tee /etc/udev/rules.d/99-uinput.rules
> sudo udevadm control --reload && sudo udevadm trigger   # write to uinput
> # then log out and back in
> ```
>
> In a terminal, paste is `Ctrl+Shift+V` — pass `--paste-shift` so dit uses that chord.

> [!NOTE]
> **macOS** — grant **Accessibility** permission (System Settings → Privacy & Security →
> Accessibility) so `dit` can read the hotkey and type into the focused app.
>
> **Windows** — works out of the box.

---

## Releases & CI

Versioning is **commit-driven**. [`release-plz`](https://release-plz.dev) reads the
[Conventional Commits](https://www.conventionalcommits.org) on `main` and opens a *release PR*
that bumps the version (`feat` → minor, `fix` → patch, `!`/`BREAKING CHANGE` → major) and updates
`CHANGELOG.md`. Merging that PR creates the version tag.

The tag triggers the release build on [Blacksmith](https://blacksmith.sh) runners. Linux
`x86_64`/`aarch64` compile **natively** then get their glibc floor lowered to 2.28 by
[`cargo-zigbuild`](https://github.com/rust-cross/cargo-zigbuild) (so one binary spans every modern
distro); `armv7` cross-compiles the same way. Two extra **fully-static musl** binaries
(`*-static`) build inside `messense/rust-musl-cross` containers with a statically-linked ALSA, as a
fallback for ancient or musl hosts. macOS ships universal coverage, Windows an `.exe`. Every target
is built `--locked`, stripped, smoke-tested, and published to a GitHub Release with `.sha256`
sidecars and a [`git-cliff`](https://git-cliff.org) changelog.

```
commits (feat:/fix:/…) ─► release-plz PR ─► merge ─► tag vX.Y.Z ─► binaries + GitHub Release
```

So you never tag by hand — just write conventional commits and merge the release PR. **No PAT
needed:** the tag release-plz creates triggers the build directly (you can also rebuild any tag
manually with `gh workflow run release.yml -f version=X.Y.Z`).

---

## License

MIT © [RedDB.io](https://github.com/reddb-io)
