
## What's in 0.2.4


### Bug Fixes

- **settings**: adapt to eframe 0.35 App::ui trait and always expose Settings subcommand
- **models**: add #[derive(Debug)] to ModelEntry


### Build & CI

- add Linux GUI system deps and --features gui build step


### Chores

- update Cargo.lock for symphonia and arboard dependencies
- update Cargo.lock for eframe 0.35 gui dependency


### Features

- **main**: wire up dit transcribe subcommand
- **transcribe**: add cmd_transcribe module with decode, resample, and output routing
- **engine**: implement ScribeEngine::transcribe_batch via WebSocket
- **engine**: update transcribe_batch signature to accept Config for batch use
- **config**: add Transcribe subcommand and expose config helpers as pub(crate)
- **deps**: add symphonia (audio decode) and arboard (clipboard) for transcribe command
- **tray**: add Settings… menu item that spawns settings GUI as subprocess
- **settings**: add eframe/egui settings window with General and Account tabs
- **config**: add Settings subcommand and expose internals for settings GUI
- **config**: add gui feature flag with optional eframe dependency
- **scribe**: pass retention config to SessionLog::open
- **output**: prune old session logs at session open
- **config**: add session_max_age_days and session_max_count retention knobs
- **main**: dispatch Command::Models to models::run
- **config**: add Models subcommand and ModelsAction enum
- add `dit models {list,download,path,rm}` subcommands
- portable multi-arch Linux builds + native `dit update` self-upgrade ([#2](https://github.com/reddb-io/dit/pull/2))


