
## What's in 0.2.4


### Bug Fixes

- **linux_input**: mitigate Wayland image-paste race + add hybrid typing
- **settings**: adapt to eframe 0.35 App::ui trait and always expose Settings subcommand
- **models**: add #[derive(Debug)] to ModelEntry


### Build & CI

- add Linux GUI system deps and --features gui build step


### Chores

- update Cargo.lock for eframe 0.35 gui dependency


### Documentation

- **readme**: document --type and the GNOME/Wayland image-paste mitigation
- **cargo**: document the option A (wayland-data-control) trade-off


### Features

- **main**: pass cfg.type_hybrid when spawning the injector
- **inject**: plumb type_hybrid into the injector spawn
- **config**: add opt-in --type hybrid delivery flag
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


