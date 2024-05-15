# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).


## [Unreleased]

### Added
- configuration option `pass_environment` which specifies a list of env var names to be passed from ra-multiplex client proxy to the spawned language server (rust-analyzer)


## [v0.2.4] - 2024-05-15

### Fixed
- track open files by clients to avoid sending duplicate `textDocument/didOpen` and `textDocument/didClose` notifications
- fix `reload` subcommand leaking client connections
- properly percent-decodes URIs from LSP, so whitespace and other URI unsafe characters in project path don't cause issues


## [v0.2.3] - 2024-03-17

### Added
- `status` subcommand to show server state
- `reload` subcommand to send `rust-analyzer/reloadWorkspace` command in case automatic reload fails

### Changed
- merge server and client binaries, now using subcommands with a single binary
- replace custom handshake messages with a field in native LSP initialize messages [#49](https://github.com/pr2502/ra-multiplex/pull/49)

### Fixed
- handle `workspace/configuration` server request to fix rust-analyzer configuration with some LSP clients
- handle `workDoneProgress/create` server request to fix progress indicators in editors


## [v0.2.2] - 2023-05-02

### Changed
- allows disabling automatic workspace detection with `workspace_detection` configuration option


## [v0.2.1] - 2023-02-01

### Fixed
- send expected response to editor on `shutdown` request, prevents hang when quitting helix


## [v0.2.0] - 2022-07-01

### Added
- adds `RA_MUX_SERVER` env var as an alias to `--ra-mux-server` CLI option

### Changed
- removes `arg_parsing` option from configuration, client argument parsing is now unconditional
- renames `--server` CLI option to `--ra-mux-server`

### Fixed
- client recognizes `--version` CLI flag and doesn't cause error with vscode anymore


## [v0.1.8] - 2022-06-02

### Fixed
- skip `shutdown` request so disconnecting clients don't turn off the instance for other clients


## [v0.1.7] - 2022-02-05

### Added
- initial support for language servers other than rust-analyzer
- optional CLI parsing for client, enabled with `arg_parsing` configuration option
- enable `.ra-multiplex-workspace-root` file to mark workspace root for workspaces without Cargo.toml


## [v0.1.6] - 2022-02-03

### Fixed
- disconnect clients immediately when instance closes
- cache `initialize` request ID instead of guessing it


## [v0.1.5] - 2022-02-01

### Changed
- replace `listen` and `port` configuration options with separate `listen` and `connect` options for server and client/proxy



## [v0.1.4] - 2022-02-01

### Changed
- remove requirement for nightly


## [v0.1.3] - 2022-01-30

### Added
- configuration file support

### Fixed
- crash if client cwd is not valid utf-8 explicitly instead of using replacement characters


## [v0.1.2] - 2022-01-30

### Fixed
- dangling input channels no longer keep instance alive

## [v0.1.1] - 2022-01-29
