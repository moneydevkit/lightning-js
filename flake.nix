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

        # Pinned by rust-toolchain.toml so the dev shell runs the exact
        # toolchain CI lints with; see the comment there. The sha256 pins the
        # channel's component set and must be bumped together with the
        # channel (build once with lib.fakeSha256 to learn the new one).
        rustToolchain = fenixPkgs.fromToolchainFile {
          file = ./rust-toolchain.toml;
          sha256 = "sha256-OATSZm98Es5kIFuqaba+UvkQtFsVgJEBMmS+t6od5/U=";
        };

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

          # Nix's fortify hardening breaks tikv-jemalloc-sys debug builds: the
          # wrapper injects _FORTIFY_SOURCE, cargo passes -O0, glibc emits a
          # #warning, and jemalloc's -Werror configure probes all fail
          # ("cannot determine return type of strerror_r").
          hardeningDisable = [ "fortify" ];

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
