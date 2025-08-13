{
  lib,
  rustPlatform,
  versionSuffix ? "",
  ...
}:

let
  manifest = lib.importTOML ./Cargo.toml;
in
rustPlatform.buildRustPackage {
  pname = manifest.package.name;
  version = "${manifest.package.version}${versionSuffix}";
  src = ./.;
  cargoLock.lockFile = ./Cargo.lock;
}
