{
  description = "ShadowPTY - Headless TUI testing MCP server in Rust";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    crane = {
      url = "github:ipetkov/crane";
    };
  };

  outputs = {
    self,
    nixpkgs,
    flake-utils,
    rust-overlay,
    crane,
  }:
    flake-utils.lib.eachDefaultSystem (system: let
      overlays = [(import rust-overlay)];
      pkgs = import nixpkgs {
        inherit system overlays;
      };

      rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
      craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;

      src = craneLib.cleanCargoSource (craneLib.path ./.);

      commonArgs = {
        inherit src;
        strictDeps = true;
        buildInputs = pkgs.lib.optionals pkgs.stdenv.isDarwin [
          pkgs.apple-sdk_14
        ];
      };

      cargoArtifacts = craneLib.buildDepsOnly commonArgs;

      shadowpty = craneLib.buildPackage (commonArgs
        // {
          inherit cargoArtifacts;
        });
    in {
      packages = {
        default = shadowpty;
        inherit shadowpty;
        termcp = shadowpty;
      };

      apps.default = flake-utils.lib.mkApp {
        drv = shadowpty;
      };

      devShells.default = craneLib.devShell {
        inherit cargoArtifacts;
        packages = with pkgs;
          [
            rustToolchain
            rust-analyzer
            clippy
            cargo
            asciinema
          ]
          ++ pkgs.lib.optionals pkgs.stdenv.isDarwin [
            pkgs.apple-sdk_14
          ];
      };
    });
}
