{
  description = "Like raop-zeroconf-discover, but with systemd-resolved";
  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay.url = "github:oxalica/rust-overlay";
  };
  outputs = {
    nixpkgs,
    flake-utils,
    rust-overlay,
    ...
  }:
    flake-utils.lib.eachDefaultSystem (
      system: let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [rust-overlay.overlays.default];
          config.allowUnfree = true;
        };
        rust =
          (pkgs.rustChannelOf {
            date = "2023-05-09";
            channel = "nightly";
          })
          .default
          .override {
            extensions = ["rust-analyzer" "rust-src" "clippy"];
          };
      in rec {
        devShell = pkgs.mkShell {
          nativeBuildInputs = with pkgs; [
            alejandra
            rust
            pkg-config
            dbus.dev
            pipewire.dev
          ];
          LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
          # Set C flags for Rust's bindgen program. Unlike ordinary C
          # compilation, bindgen does not invoke $CC directly. Instead it
          # uses LLVM's libclang. To make sure all necessary flags are
          # included we need to look in a few places.
          # See https://web.archive.org/web/20220523141208/https://hoverbear.org/blog/rust-bindgen-in-nix/
          BINDGEN_EXTRA_CLANG_ARGS = let
            stdenv = pkgs.stdenv;
            lib = nixpkgs.lib;
          in "${builtins.readFile "${stdenv.cc}/nix-support/libc-crt1-cflags"} \
                ${builtins.readFile "${stdenv.cc}/nix-support/libc-cflags"} \
                ${builtins.readFile "${stdenv.cc}/nix-support/cc-cflags"} \
                ${builtins.readFile "${stdenv.cc}/nix-support/libcxx-cxxflags"} \
                -idirafter ${pkgs.libiconv}/include \
                ${lib.optionalString stdenv.cc.isClang "-idirafter ${stdenv.cc.cc}/lib/clang/${lib.getVersion stdenv.cc.cc}/include"} \
                ${lib.optionalString stdenv.cc.isGNU "-isystem ${stdenv.cc.cc}/include/c++/${lib.getVersion stdenv.cc.cc} -isystem ${stdenv.cc.cc}/include/c++/${lib.getVersion stdenv.cc.cc}/${stdenv.hostPlatform.config} -idirafter ${stdenv.cc.cc}/lib/gcc/${stdenv.hostPlatform.config}/${lib.getVersion stdenv.cc.cc}/include"} \
            ";
        };
      }
    );
}
