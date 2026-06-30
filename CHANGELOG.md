
## What's in 0.2.4


### Build & CI

- lock toml as a direct dependency
- add toml dependency for the config store


### Features

- **config**: parse via ArgMatches so explicit flags win over the config file
- **config**: layered resolution with persistent ~/.dit/config.toml store
- portable multi-arch Linux builds + native `dit update` self-upgrade ([#2](https://github.com/reddb-io/dit/pull/2))


