
## What's in 0.2.4


### Bug Fixes

- **clippy**: resolve 5 pre-existing -D warnings errors blocking CI
- **models**: add #[derive(Debug)] to ModelEntry


### Features

- **scribe**: pass retention config to SessionLog::open
- **output**: prune old session logs at session open
- **config**: add session_max_age_days and session_max_count retention knobs
- **main**: dispatch Command::Models to models::run
- **config**: add Models subcommand and ModelsAction enum
- add `dit models {list,download,path,rm}` subcommands
- portable multi-arch Linux builds + native `dit update` self-upgrade ([#2](https://github.com/reddb-io/dit/pull/2))


### Style

- cargo fmt


