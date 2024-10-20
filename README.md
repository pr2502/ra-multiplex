# ra-multiplex &emsp; [![Latest Version]][crates.io]

[Latest Version]: https://img.shields.io/crates/v/ra-multiplex.svg
[crates.io]: https://crates.io/crates/ra-multiplex

Multiplex server for `rust-analyzer`, allows multiple LSP clients (editor
windows) to share a single `rust-analyzer` instance per cargo workspace.


## How it works

`ra-multiplex` acts like `rust-analyzer` but only connects to a TCP socket at
`127.0.0.1:27631` and pipes stdin and stdout through it.

Depending on the `workspaceFolders` provided by your editor during
initialization it can reuse an already spawned `rust-analyzer` instance.
 
Because neither LSP nor `rust-analyzer` itself support multiple clients
per server `ra-multiplex` intercepts the handshake process and modifies IDs
of requests and responses to track which response belongs to which client.
Because not all messages can be tracked this way it drops some, notably it
drops any requests from the server, this appears to not be a problem with
`coc-rust-analyzer` in neovim but YMMV.

If you have any problems you're welcome to open issues on this repository.


## How to use
Install ra-multiplex using cargo:

```sh
cargo install ra-multiplex
```

Install ra-multiplex using homebrew:

```sh
brew install simnalamburt/x/ra-multiplex
brew services start simnalamburt/x/ra-multiplex # to enable service
```

Run `ra-multiplex` in server mode, make sure that `rust-analyzer` is in your
`PATH`:

```sh
$ which rust-analyzer
/home/user/.cargo/bin/rust-analyzer
$ target/release/ra-multiplex --help
share one rust-analyzer server instance between multiple LSP clients to save resources

Usage: ra-multiplex [COMMAND]

Commands:
  client  Connect to a ra-mux server [default]
  server  Start a ra-mux server
  status  Print server status
  reload  Reload workspace
  help    Print this message or the help of the given subcommand(s)

Options:
  -h, --help     Print help
  -V, --version  Print version
```

`ra-multiplex server` can run as a systemd user service, see the example `ra-mux.service`.

Configure your editor to use `ra-multiplex` as `rust-analyzer`, for example for
CoC in neovim edit `~/.config/nvim/coc-settings.json`, add:

```json
{
    "rust-analyzer.serverPath": "/path/to/ra-multiplex"
}
```

If your editor can connect to a language server via TCP you don't need to use
the `ra-multiplex` client and connect directly to the server but you need to
provide the same information as the proxy command would. See the
[example config for neovim](examples/neovim/init.lua) for details.


## Configuration

Configuration is stored in a TOML file in your system's default configuration
directory, for example `~/.config/ra-multiplex/config.toml`. If you're not sure
where that is on your system starting `ra-multiplex` without a config file
present will print a notice with the expected path.

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
# or unix socket path on *nix operating systems
#
# the default "127.0.0.1" only allows connections from localhost which is
# preferred since the protocol doesn't worry about security.
# ra-multiplex server expects the filesystem structure and contents to be the
# same on its machine as on ra-multiplex's machine. if you want to run the
# server on a different computer it's theoretically possible but at least for
# now you're on your own.
#
# ports below 1024 will typically require root privileges and should be
# avoided, the default was picked at random, this only needs to change if
# another application happens to collide with ra-multiplex.
listen = ["127.0.0.1", 27631] # localhost & some random unprivileged port
# listen = "/var/run/ra-mux/ra-mux.sock" # unix socket

# ip address and port to which ra-multiplex will connect to
# or unix socket path on *nix operating systems
#
# this should usually just match the value of `listen`
connect = ["127.0.0.1", 27631] # same as `listen`
# connect = "/var/run/ra-mux/ra-mux.sock" # same as `listen`

# default log filters
#
# RUST_LOG env variable overrides this option, both use the same syntax which
# is documented in the `env_logger` documentation here:
# <https://docs.rs/env_logger/0.9.0/env_logger/index.html#enabling-logging>
log_filters = "info"

# environemnt variable names passed from `ra-multiplex client` to the server
#
# by default no variables are passed. and all servers are spawned in
# the same environment as the `ra-multiplex server` is. when a name like
# "LD_LIBRARY_PATH" is specifified the proxy reads the variable value from its
# environment and passes it to the server which then passes it on to the server
# executable.
#
# if "PATH" is specified here then the PATH from the client environment is
# going to be used for looking up a relative `--server-path`.
pass_environment = []
```


## Other LSP servers

By default `ra-multiplex` uses a `rust-analyzer` binary found in its `$PATH`
as the server. This can be overridden using the `--server-path` cli option with
the client subcommand or `RA_MUX_SERVER` environment variable. You can usually
configure one of these in your editor configuration. If both are specified the
cli option overrides the environment variable.

For example with `coc-clangd` in CoC for neovim add to
`~/.config/nvim/coc-settings.json`:

```json
{
    "clangd.path": "/home/user/.cargo/bin/ra-multiplex",
    "clangd.arguments": ["client", "--server-path", "/usr/bin/clangd"]
}
```

Or to set a custom path for `rust-analyzer` with `coc-rust-analyzer` add to
`~/.config/nvim/coc-settings.json`:

```json
{
    "rust-analyzer.server.path": "/home/user/.cargo/bin/ra-multiplex",
    "rust-analyzer.server.extraEnv": { "RA_MUX_SERVER": "/custom/path/rust-analyzer" }
}
```

If your editor configuration or plugin doesn't allow to add either you can
instead create a wrapper shell script and set it as the server path directly.
For example if `coc-clangd` didn't allow to pass additional arguments you'd
need a script like `/usr/local/bin/clangd-proxy`:

```sh
#!/bin/sh
RA_MUX_SERVER=/usr/bin/clangd exec /home/user/.cargo/bin/ra-multiplex client --server-path /usr/bin/clangd $@
```

And configure the editor to use the wrapper script in
`~/.config/nvim/coc-settings.json`:

```json
{
    "clangd.path": "/usr/local/bin/clangd-proxy"
}
```
