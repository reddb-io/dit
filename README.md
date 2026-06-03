# dictator

Cross-platform, push-to-toggle **voice dictation**. Press a hotkey, speak, and each
stable transcript segment is pasted into whatever window is focused. Press the hotkey
again to stop.

It streams microphone audio to [ElevenLabs **Scribe v2 Realtime**](https://elevenlabs.io/docs/api-reference/speech-to-text)
over a WebSocket and injects the committed segments via the clipboard. A Rust,
multi-platform reimplementation of the original Linux/Wayland-only `whisperflow.py`
Python script — now running on **Linux, macOS and Windows**.

## How it works

```
hotkey (rdev) ──toggle──► session
                            │
   mic (cpal) ─► resample 16 kHz ─► WebSocket ─► ElevenLabs Scribe v2
                                                        │
                       committed_transcript ◄───────────┘
                            │
                  clipboard (arboard) + paste keystroke (enigo)
```

* `partial_transcript` events are **ignored** (unstable preview).
* `committed_transcript` events are stable per-segment text, committed by the
  server's Voice Activity Detection on pauses — each is pasted immediately.
* Identical consecutive segments are de-duplicated.
* On stop, an empty `commit: true` frame flushes the final open segment, and the
  clipboard is restored to its previous contents.

## Install

### From a release

Download the archive for your platform from the
[Releases](https://github.com/filipeforattini/dictator/releases) page and put the
`dictator` binary on your `PATH`.

### From source

```bash
cargo install --path .
# or
cargo build --release   # ./target/release/dictator
```

#### Linux build dependencies

```bash
sudo apt-get install -y \
  libasound2-dev libxdo-dev libxi-dev libxtst-dev \
  libxcb1-dev libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev \
  libdbus-1-dev pkg-config
```

(macOS and Windows need no extra system packages.)

## Configure

Put your ElevenLabs API key in `~/.dictator.env`:

```bash
echo 'ELEVENLABS_API_KEY=sk_your_key_here' > ~/.dictator.env
```

(or export `ELEVENLABS_API_KEY` in your environment, or pass `--env-file <path>`).

## Use

```bash
dictator                       # default: F9 hotkey, pt language
dictator --language en         # English
dictator --hotkey F8           # different toggle key (F1..F12)
dictator --device "Fifine"     # prefer an input device by name substring
dictator --list-devices        # show input devices and exit
```

Press **F9** to start, speak, press **F9** again to stop. `Ctrl+C` quits.

Tune logging with `RUST_LOG`, e.g. `RUST_LOG=dictator=debug dictator`.

## Platform notes

* **Linux / X11** — works out of the box. Global key capture uses X11; the
  paste keystroke and clipboard use X11/XCB.
* **Linux / Wayland** — Wayland restricts global key grabbing and synthetic input
  for security. Capture/paste may be limited depending on the compositor; running
  under XWayland or an X11 session is the reliable path. (The original Python
  script sidestepped this with `evdot`/`ydotool` and the `input` group.)
* **macOS** — grant **Accessibility** permission (System Settings → Privacy &
  Security → Accessibility) so the app can read the hotkey and send the paste
  keystroke. The paste modifier is automatically **⌘** instead of Ctrl.
* **Windows** — works out of the box.

## License

MIT © Filipe Forattini
