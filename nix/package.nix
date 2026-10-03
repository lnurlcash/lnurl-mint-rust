{
  lib,
  rustPlatform,
  cmake,
  boost,
  src, # the flake root, passed in from flake.nix (self)
}:

let
  cargoToml = lib.importTOML "${src}/Cargo.toml";
in
rustPlatform.buildRustPackage {
  pname = "lnurl-mint";
  inherit (cargoToml.package) version;
  inherit src;

  # every crate pinned by Cargo.lock, fetched as fixed-output derivations:
  # the build itself runs without network
  cargoLock.lockFile = "${src}/Cargo.lock";

  # lnurlcash-kernel's build script compiles Bitcoin Core's kernel with CMake.
  # Only that build script drives CMake: nixpkgs' CMake hook must not take
  # over this package's own configure phase.
  nativeBuildInputs = [ cmake ];
  dontUseCmakeConfigure = true;

  # Core needs Boost's headers; the build script would otherwise look in
  # /usr/include and then download them, which the sandbox forbids
  env.LNURLCASHKERNEL_BOOST_DIR = "${boost.dev}/lib/cmake/Boost-${boost.version}";

  # the checkPhase runs the whole cargo test suite: LUD-25/26 vectors against
  # Bitcoin Core's interpreter, the store, and the protocol over HTTP. The
  # regtest end-to-end test needs bitcoind and is CI's (scripts/).
  doCheck = true;

  meta = {
    description = cargoToml.package.description;
    homepage = "https://github.com/dni/lnurl-mint-rust";
    license = lib.licenses.mit;
    mainProgram = "lnurl-mint";
    platforms = [
      "x86_64-linux"
      "aarch64-linux"
    ];
  };
}
