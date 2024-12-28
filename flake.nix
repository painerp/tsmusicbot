{
  description = "Rust development template";

  inputs = {
    nixpkgs.url      = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = {
    self,
    nixpkgs,
    rust-overlay,
    flake-utils,
    ...
  }:
    flake-utils.lib.eachDefaultSystem
    (
      system: let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs {inherit system overlays;};
      in rec
      {
        devShells.default = pkgs.mkShell {
          buildInputs = with pkgs; [
            (rust-bin.stable.latest.default.override { extensions = [ "rust-src" ]; })
            pkg-config
            autoconf
            openssl
            libtool
            automake
          ];

          RUST_SRC_PATH = "${pkgs.rust-bin.stable.latest.default}/lib/rustlib/src/rust";
        };
      }
    );
}
