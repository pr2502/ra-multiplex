# ra-multiplex &emsp; [![Latest Version]][crates.io]

[Latest Version]: https://img.shields.io/crates/v/ra-multiplex.svg
[crates.io]: https://crates.io/crates/ra-multiplex

Multiplex server for `rust-analyzer` allows multiple LSP clients (editor
windows) to share a single `rust-analyzer` instance per cargo workspace.


## How it works

The project two binaries, `ra-multiplex` which is a thin wrapper that acts like
`rust-analyzer` but only connects to a TCP socket at `127.0.0.1:27631` and
pipes stdin and stdout to it.

The second binary `ra-multiplex-server` will listen on `:27631` and spawn the
`rust-analyzer` server, but depending on the working directory the
`ra-multiplex` client was spawned from it might reuse an already spawned
`rust-analyzer` instance.

Because this is not really a supported mode of operation by either LSP or
`rust-analyzer` itself the `ra-multiplex-server` caches some messages and
modifies request/response ids in order to make most of LSP work.

The project is still work in progress so it'll probably block some LSP
functionality or break randomly or something, but it was tested and works well
enough with `coc-rust-analyzer` client in neovim, it should theoretically work
with other clients and editors too. If you have problems you're welcome to open
issues on this repository.


## How to use

Build the project with

```sh
$ cargo build --release
```

Run the `ra-multiplex-server`, make sure that `rust-analyzer` is in your
`PATH`, with optional logging.

```sh
$ which rust-analyzer
/home/user/.cargo/bin/rust-analyzer
$ RUST_LOG=info target/release/ra-multiplex-server
```

Configure your editor to use `ra-multiplex` as `rust-analyzer`, for example for
CoC in neovim edit `~/.config/nvim/coc-settings.json`, add:

```json
{
    "rust-analyzer.serverPath": "/path/to/ra-multiplex",
}
```


## Configuration

Configuration is stored in a TOML file in your system's default configuration
directory, for example `~/.config/ra-multiplex/config.toml`. If you're not sure
where that is on your system starting either `ra-multiplex` or
`ra-multiplex-server` without a config file present will print a warning with
the expected path.

Note that the configuration file is likely not necessary and `ra-multiplex`
should be usable with all defaults.

Example configuration file:

```toml
# this is an example configuration file for ra-multiplex
#
# all configuration options here are set to their default value they'll have if
# they're not present in the file or if the config file is missing completely.

# time in seconds after which a rust-analyzer server instance with no clients
# connected will get killed to save system memory.
#
# you can set this option to `false` for infinite timeout
instance_timeout = 300 # after 5 minutes

# time in seconds how long to wait between the gc task checks for disconnected
# clients and possibly starts a timeout task. the value must be at least 1.
gc_interval = 10 # every 10 seconds

# ip address on which ra-multiplex-server listens
#
# the default "127.0.0.1" only allows connections from localhost which is
# preferred since the protocol doesn't worry about security.
# ra-multiplex-server expects the filesystem structure and contents to be the
# same on its machine as on ra-multiplex's machine. if you want to run the
# server on a different computer it's theoretically possible but at least for
# now you're on your own.
listen = "127.0.0.1" # localhost

# tcp port number on which ra-multiplex-server listens
#
# ports below 1024 will typically require root privileges and should be
# avoided, this only needs to change if another application happens to collide
# with ra-multiplex.
port = 27631 # random unprivileged port

# default log filters
#
# RUST_LOG env variable overrides this option, both use the same syntax which
# is documented in the `env_logger` documentation here:
# <https://docs.rs/env_logger/0.9.0/env_logger/index.html#enabling-logging>
log_filters = "info"
```
