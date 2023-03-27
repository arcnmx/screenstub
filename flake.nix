{
  description = "A VFIO KVM switch using DDC/CI";
  inputs = {
    flakelib.url = "github:flakelib/fl";
    nixpkgs = { };
    rust = {
      url = "github:arcnmx/nixexprs-rust";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };
  outputs = { self, flakelib, nixpkgs, rust, ... }@inputs: let
    nixlib = nixpkgs.lib;
  in flakelib {
    inherit inputs;
    systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
    devShells = {
      plain = {
        mkShell, writeShellScriptBin, hostPlatform
      , udev
      , libxcb ? xorg.libxcb, xorg ? { }
      , pkg-config, python3
      , libiconv
      , CoreGraphics ? darwin.apple_sdk.frameworks.CoreGraphics, darwin
      , enableRust ? true, cargo
      , rustTools ? [ ]
      , nativeBuildInputs ? [ ]
      }: mkShell {
        inherit rustTools;
        buildInputs = [ libxcb ]
          ++ nixlib.optional hostPlatform.isLinux udev
          ++ nixlib.optionals hostPlatform.isDarwin [ libiconv CoreGraphics ];
        nativeBuildInputs = [ pkg-config python3 ]
          ++ nixlib.optional enableRust cargo
          ++ nativeBuildInputs ++ [
            (writeShellScriptBin "generate" ''nix run .#generate "$@"'')
          ];
        RUST_LOG = "debug";
      };
      stable = { rust'stable, outputs'devShells'plain }: outputs'devShells'plain.override {
        inherit (rust'stable) mkShell;
        enableRust = false;
      };
      dev = { rust'unstable, outputs'devShells'plain }: outputs'devShells'plain.override {
        inherit (rust'unstable) mkShell;
        enableRust = false;
        rustTools = [ "rust-analyzer" ];
      };
      default = { outputs'devShells }: outputs'devShells.plain;
    };
    packages = {
      screenstub = {
        __functor = _: import ./derivation.nix;
        fl'config.args = {
          crate.fallback = self.lib.crate;
        };
      };
      default = { screenstub }: screenstub;
    };
    legacyPackages = { callPackageSet }: callPackageSet {
      source = { rust'builders }: rust'builders.wrapSource self.lib.crate.src;

      generate = { rust'builders, outputHashes }: rust'builders.generateFiles {
        paths = {
          "lock.nix" = outputHashes;
        };
      };
      outputHashes = { rust'builders }: rust'builders.cargoOutputHashes {
        inherit (self.lib) crate;
      };
    } { };
    checks = {
      ${if false then "rustfmt" else null} = { rust'builders, screenstub }: rust'builders.check-rustfmt-unstable {
        inherit (screenstub) src;
        config = ./.rustfmt.toml;
      };
    };
    lib = with nixlib; {
      crate = rust.lib.importCargo {
        path = ./Cargo.toml;
        inherit (import ./lock.nix) outputHashes;
      };
      inherit (self.lib.crate) version;
      releaseTag = "v${self.lib.version}";
    };
    config = rec {
      name = "screenstub";
    };
  };
}
