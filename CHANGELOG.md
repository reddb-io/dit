
## What's in 0.5.0


### Features

- manage ElevenLabs API key in Red config ([#60](https://github.com/reddb-io/dit/pull/60))



**Full Changelog**: https://github.com/reddb-io/dit/compare/v0.4.1...v0.5.0


## What's in 0.4.1


### Bug Fixes

- **linux**: adapt paste chord to focused app ([#56](https://github.com/reddb-io/dit/pull/56))
- ship settings in release builds ([#57](https://github.com/reddb-io/dit/pull/57))



**Full Changelog**: https://github.com/reddb-io/dit/compare/v0.4.0...v0.4.1


## What's in 0.4.0


### Build & CI

- **release**: bump the minor version for features while on 0.x ([#54](https://github.com/reddb-io/dit/pull/54))
- keep trunk builds Linux-only ([#51](https://github.com/reddb-io/dit/pull/51))


### Features

- **delivery**: route dictation into zellij as a bracketed paste ([#53](https://github.com/reddb-io/dit/pull/53))



**Full Changelog**: https://github.com/reddb-io/dit/compare/v0.3.2...v0.4.0


## What's in 0.3.2


### Bug Fixes

- **release**: stop shipping an empty changelog on the tag-push route ([#49](https://github.com/reddb-io/dit/pull/49))
- **ci**: give release-plz the ALSA dev package that git_only now needs ([#47](https://github.com/reddb-io/dit/pull/47))
- **ci**: wake release-plz back up with git_only, and make idling loud ([#46](https://github.com/reddb-io/dit/pull/46))



**Full Changelog**: https://github.com/reddb-io/dit/compare/v0.3.1...v0.3.2


## What's in 0.3.1


### Bug Fixes

- **release**: repair the Windows install path and widen arch coverage


### CI

- run macOS/Windows legs post-merge only, PR caches restore-only


## What's in 0.3.0


### Bug Fixes

- **local**: make Whisper inference actually produce text
- repair local-engine integration and harden dictation paths


### Chores

- add AGENTS.md and CLAUDE.md project instructions


### Documentation

- rewrite README for v0.3.0 feature set


