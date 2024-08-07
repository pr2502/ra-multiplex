require'lspconfig'.rust_analyzer.setup {
  cmd = vim.lsp.rpc.connect("127.0.0.1", 27631),
  -- When using unix domain sockets, use something like:
  --cmd = vim.lsp.rpc.connect("/path/to/ra-multiplex.sock"),
  settings = {
    ["rust-analyzer"] = {
      lspMux = {
        version = "1",
        method = "connect",
        server = "rust-analyzer",
      },
    },
  },
}
