require'lspconfig'.rust_analyzer.setup {
  cmd = vim.lsp.rpc.connect("127.0.0.1", 27631),
  init_options = {
    lspMux = {
      server = "rust-analyzer",
      version = "1",
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
