{
  description = ''
    Language server proxy which enables sharing a single language server,
    like rust-analyzer, between multiple LSP clients (editor windows).
  '';

  inputs = {
    nixpkgs.url      = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    utils.url        = "github:numtide/flake-utils";

    rust-overlay.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs = { self, nixpkgs, rust-overlay, utils }:
  utils.lib.eachDefaultSystem (system:
    let
      pkgs = import nixpkgs {
        inherit system;
        overlays = [(import rust-overlay)];
      };

      rustToolchain = pkgs.rust-bin.stable."1.80.1".minimal;
      rustPlatform = pkgs.makeRustPlatform {
        cargo = rustToolchain;
        rustc = rustToolchain;
      };

      rustDev = rustToolchain.override {
        extensions = ["rust-src" "rust-analyzer" "rust-docs" "clippy"];
      };
      # We want to use some unstable options in `rustfmt.toml` and
      # unfortunately the only way to do that is use nightly rustfmt.
      rustFmt = pkgs.rust-bin.nightly."2024-07-07".minimal.override {
        extensions = ["rustfmt"];
      };
    in {
      packages.default = rustPlatform.buildRustPackage {
        pname = "ra-multiplex";
        version = "0.2.5";
        src = self;
        cargoLock.lockFile = ./Cargo.lock;
      };
      devShells.default = pkgs.mkShell {
        nativeBuildInputs = [
          rustDev
          rustFmt
        ];
      };
    });
}
