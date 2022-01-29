# ra-multiplex

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
