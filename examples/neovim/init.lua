require'lspconfig'.rust_analyzer.setup {
  cmd = vim.lsp.rpc.connect("127.0.0.1", 27631),
  init_options = {
    lspMux = {
      version = "1",
      method = "connect",
      server = "rust-analyzer",
    },
  },
  settings = {
    ['rust-analyzer'] = {
      checkOnSave = {
        command = "clippy",
      },
    }
  }
}
