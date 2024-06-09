{
  description = "Multiplex server for rust-analyzer, allows multiple LSP clients (editor windows) to share a single rust-analyzer instance per cargo workspace.";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-23.05";
    utils.url = "github:numtide/flake-utils";
    rust-overlay.url = "github:oxalica/rust-overlay";
    crane.url = "github:ipetkov/crane";
  };

  outputs = { self, nixpkgs, utils, rust-overlay, crane, ... }:
    utils.lib.eachDefaultSystem (system:
      let
        overlays = [
            (import rust-overlay)

        ];
        pkgs = import nixpkgs { inherit system overlays; };
        craneLib = crane.lib.${system};

        rustVersion = pkgs.rust-bin.stable."1.72.0".default;

        rustPlatform = pkgs.makeRustPlatform {
          cargo = rustVersion;
          rustc = rustVersion;
        };
        ra-multiplex = craneLib.buildPackage {
          src = self;
        };

      in {
        packages = {
          inherit ra-multiplex;
          default = ra-multiplex;
        };
        devShell = pkgs.mkShell {

          buildInputs =
            [ (rustVersion.override { extensions = [ "rust-src" "rust-analyzer" ]; }) ];
        };
      });
}
