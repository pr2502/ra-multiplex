# ra-multiplex &emsp; [![Latest Version]][crates.io]

[Latest Version]: https://img.shields.io/crates/v/ra-multiplex.svg
[crates.io]: https://crates.io/crates/ra-multiplex

Multiplex server for `rust-analyzer`, allows multiple LSP clients (editor
windows) to share a single `rust-analyzer` instance per cargo workspace.


## How it works

The project has two binaries, `ra-multiplex` which is a thin wrapper that acts
like `rust-analyzer` but only connects to a TCP socket at `127.0.0.1:27631` and
pipes stdin and stdout through it.

The second binary `ra-multiplex-server` will listen on `:27631` and spawn the
`rust-analyzer` server, depending on the working directory the `ra-multiplex`
client was spawned from it can reuse an already spawned `rust-analyzer`
instance. It detects workspace root as the furthermost ancestor directory
containing a `Cargo.toml` file. If the automatic workspace detection fails or
if you're not using `rust-analyzer` as a server you can create a marker file
`.ra-multiplex-workspace-root` in the directory you want to use as a workspace
root, the first ancestor directory containing this file will be used.

Because neither LSP nor `rust-analyzer` itself support multiple clients per
server `ra-multiplex-server` caches the handshake messages and modifies IDs of
requests & responses to track which response belongs to which client. Because
not all messages can be tracked this way it drops some, notably it drops any
requests from the server, this appears to not be a problem with
`coc-rust-analyzer` in neovim but YMMV.

If you have any problems you're welcome to open issues on this repository.


## How to use

Build the project with

```sh
$ cargo build --release
```

Run the `ra-multiplex-server`, make sure that `rust-analyzer` is in your
`PATH`:

```sh
$ which rust-analyzer
/home/user/.cargo/bin/rust-analyzer
$ target/release/ra-multiplex-server
```

Configure your editor to use `ra-multiplex` as `rust-analyzer`, for example for
CoC in neovim edit `~/.config/nvim/coc-settings.json`, add:

```json
{
    "rust-analyzer.serverPath": "/path/to/ra-multiplex"
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

# ip address and port on which ra-multiplex-server listens
#
# the default "127.0.0.1" only allows connections from localhost which is
# preferred since the protocol doesn't worry about security.
# ra-multiplex-server expects the filesystem structure and contents to be the
# same on its machine as on ra-multiplex's machine. if you want to run the
# server on a different computer it's theoretically possible but at least for
# now you're on your own.
#
# ports below 1024 will typically require root privileges and should be
# avoided, the default was picked at random, this only needs to change if
# another application happens to collide with ra-multiplex.
listen = ["127.0.0.1", 27631] # localhost & some random unprivileged port

# ip address and port to which ra-multiplex will connect to
#
# this should usually just match the value of `listen`
connect = ["127.0.0.1", 27631] # same as `listen`

# default log filters
#
# RUST_LOG env variable overrides this option, both use the same syntax which
# is documented in the `env_logger` documentation here:
# <https://docs.rs/env_logger/0.9.0/env_logger/index.html#enabling-logging>
log_filters = "info"

# whether `ra-multiplex` client should process any arguments
#
# by default this is set to `false`, in this case `ra-multiplex` can be used as
# a drop-in replacement for `rust-analyzer` but no other server. for more
# information see the "Other LSP servers" section of the README.
arg_parsing = false
```


## Other LSP servers

Using other servers requires a bit more setup. First enable the `arg_parsing`
option in `~/.config/ra-multiplex/config.toml`:

```toml
arg_parsing = true
```

Then for each server you want to use (including `rust-analyzer`) create a
wrapper script that passes the right arguments like in the following example.
We're using the standard cargo install directories so update the examples for
your needs.

Wrapper script `rust-analyzer-proxy`:

```sh
#!/bin/sh
exec ~/.cargo/bin/ra-multiplex --server ~/.cargo/bin/rust-analyzer -- $@
```

Wrapper script `clangd-proxy`:

```sh
#!/bin/sh
exec ~/.cargo/bin/ra-multiplex --server /usr/bin/clangd -- --log=error $@
```

Configure LSP client to use the wrapper script, for CoC
`~/.config/nvim/coc-settings.json`:

```json
{
    "rust-analyzer.serverPath": "/home/user/.cargo/bin/rust-analyzer-proxy",
    "clangd.path": "/home/user/.cargo/bin/clangd-proxy"
}
```
