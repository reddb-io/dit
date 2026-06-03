
## What's in 0.1.0


### Bug Fixes

- **service**: scope exec_line to Linux (unused on macOS, broke -D warnings)
- **install**: don't let detect_platform's EXT check abort the script


### Build & CI

- drop the workflow_dispatch workaround — the tag triggers release.yml
- trigger release build without a PAT via workflow_dispatch


### Features

- minimal deps (Linux tray via ksni) + interactive installer
- **tray**: system tray icon with recording state + menu
- **service**: 'dit service install/uninstall/status' autostart user agent


