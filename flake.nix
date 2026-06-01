{
  description = "Lightning JS";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    crane.url = "github:ipetkov/crane";
    flake-utils.url = "github:numtide/flake-utils";

    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      nixpkgs,
      crane,
      flake-utils,
      fenix,
      ...
    }:
    flake-utils.lib.eachDefaultSystem (
      localSystem:
      let
        pkgs = nixpkgs.legacyPackages.${localSystem};
        inherit (pkgs) lib stdenv;

        fenixPkgs = fenix.packages.${localSystem};

        # Crane needs cargo >= 1.91 (`cargo package --exclude-lockfile`).
        # Cargo.toml's `rust-version = "1.85"` remains the MSRV for downstream
        # consumers; this toolchain is only for the dev shell and CI checks.
        rustToolchain = fenixPkgs.combine [
          (fenixPkgs.stable.withComponents [
            "cargo"
            "clippy"
            "rust-src"
            "rustc"
            "rustfmt"
          ])
          fenixPkgs.stable.rust-analyzer
        ];

        craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;
        src = craneLib.cleanCargoSource ./.;

        commonArgs = {
          inherit src;
          pname = "lightning-js";
          strictDeps = true;
          nativeBuildInputs = [ pkgs.pkg-config ] ++ lib.optionals stdenv.isLinux [ pkgs.mold ];
          buildInputs = [ pkgs.openssl ];
        };

        cargoArtifacts = craneLib.buildDepsOnly commonArgs;
      in
      {
        checks = {
          clippy = craneLib.cargoClippy (
            commonArgs
            // {
              inherit cargoArtifacts;
              cargoClippyExtraArgs = "--all-targets -- --deny warnings";
            }
          );

          fmt = craneLib.cargoFmt { inherit src; };

          build = craneLib.buildPackage (commonArgs // { inherit cargoArtifacts; });
        };

        devShells.default = pkgs.mkShell {
          name = "lightning-js-dev";

          packages = with pkgs; [
            nodejs_22
            yarn
            rustToolchain
            pkg-config
            openssl
            jemalloc
            mold
            gcc
            just
          ];

          PKG_CONFIG_PATH = "${pkgs.openssl.dev}/lib/pkgconfig";
          RUST_BACKTRACE = "1";
          NIX_SYSTEM = localSystem;

          shellHook = ''
            git config core.hooksPath .githooks
            echo "=========================================="
            echo "  Lightning JS Development Shell"
            echo "=========================================="
            echo "Rust: $(rustc --version)"
            echo "Node: $(node --version)"
            echo ""
            echo "Run 'just' to see available build commands"
            echo "=========================================="
          '';
        };

        formatter = pkgs.nixfmt-rfc-style;
      }
    );
}
