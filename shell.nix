let
  sources = import ./npins;
  pkgs = import sources.nixpkgs {
    overlays = [(import sources.rust-overlay)];
  };

  rustDev = pkgs.rust-bin.stable."1.88.0".minimal.override {
    extensions = ["rust-src" "rust-analyzer" "rust-docs" "clippy"];
  };
  # We want to use some unstable options in `rustfmt.toml` and
  # unfortunately the only way to do that is use nightly rustfmt.
  rustFmt = pkgs.rust-bin.nightly."2025-05-09".minimal.override {
    extensions = ["rustfmt"];
  };
in
pkgs.mkShell {
  nativeBuildInputs = [
    rustDev
    rustFmt
  ];
}
