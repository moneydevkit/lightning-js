{
  description = "Lightning JS";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    crane.url = "github:ipetkov/crane";
    flake-utils.url = "github:numtide/flake-utils";

    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      nixpkgs,
      crane,
      flake-utils,
      rust-overlay,
      ...
    }:
    flake-utils.lib.eachDefaultSystem (
      localSystem:
      let
        pkgs = import nixpkgs {
          system = localSystem;
          overlays = [ (import rust-overlay) ];
        };

        inherit (pkgs) lib stdenv;

        # Load Rust toolchain configuration from rust-toolchain.toml.
        # This ensures consistency between local development (rustup) and Nix builds.
        # See: https://rust-lang.github.io/rustup/overrides.html#the-toolchain-file
        rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;

        # Initialize Crane with our toolchain.
        # Crane is a Nix library for building Rust projects with good caching.
        # See: https://crane.dev/
        craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;

        # Filter source to only include Rust-relevant files.
        # This improves caching by excluding unrelated files from the build hash.
        src = craneLib.cleanCargoSource ./.;

        # Map Nix system to NAPI-RS node addon naming convention.
        # The naming follows: {name}.{platform}-{arch}[-libc].node
        nativeNodeName =
          {
            "x86_64-linux" = "lightning-js.linux-x64-gnu.node";
            "aarch64-linux" = "lightning-js.linux-arm64-gnu.node";
            "x86_64-darwin" = "lightning-js.darwin-x64.node";
            "aarch64-darwin" = "lightning-js.darwin-arm64.node";
          }
          .${localSystem} or (throw "Unsupported system: ${localSystem}");

        # Generate install phase command for copying the built library
        # Uses find to locate the library since Crane may place it in various locations
        mkInstallPhase = nodeName: ''
          mkdir -p $out/lib
          # Find and copy the shared library with NAPI-RS naming convention
          # Cargo builds a cdylib (.so on Linux, .dylib on macOS)
          # Node.js expects native addons to have .node extension
          found=""
          for ext in so dylib; do
            lib=$(find target -name "liblightning_js.$ext" -type f | head -1)
            if [ -n "$lib" ]; then
              cp "$lib" "$out/lib/${nodeName}"
              found=1
              break
            fi
          done
          if [ -z "$found" ]; then
            echo "ERROR: Could not find liblightning_js.so or liblightning_js.dylib"
            exit 1
          fi
        '';

        # ============================================================
        # Native build configuration (shared between packages and checks)
        # ============================================================
        nativeCommonArgs = {
          inherit src;
          pname = "lightning-js";
          strictDeps = true;
          nativeBuildInputs = [ pkgs.pkg-config ] ++ lib.optionals stdenv.isLinux [ pkgs.mold ];
          buildInputs = [ pkgs.openssl ];
        };

        # Shared dependency derivation for native release builds and checks
        nativeCargoArtifacts = craneLib.buildDepsOnly nativeCommonArgs;

        # Separate debug deps (different profile = different artifacts)
        debugCargoArtifacts = craneLib.buildDepsOnly (nativeCommonArgs // { CARGO_PROFILE = "dev"; });

        # ============================================================
        # Native build helper (release or debug)
        # ============================================================
        mkNativePackage =
          {
            isDebug ? false,
          }:
          craneLib.buildPackage (
            nativeCommonArgs
            // {
              cargoArtifacts = if isDebug then debugCargoArtifacts else nativeCargoArtifacts;

              # Set profile for debug builds (Crane defaults to release)
              CARGO_PROFILE = lib.optionalString isDebug "dev";

              installPhaseCommand = mkInstallPhase nativeNodeName;
            }
          );

        # ============================================================
        # Cross-compilation helper for Linux targets (glibc and musl)
        # ============================================================
        mkCrossPackage =
          {
            crossSystem, # Nix cross-compilation system (e.g., "aarch64-linux" or attrset)
            target, # Rust target triple (e.g., "aarch64-unknown-linux-gnu")
            nodeName, # Output filename for the .node addon
            extraRustFlags ? "", # Additional RUSTFLAGS for compilation
            useStaticLibs ? false, # Use pkgsStatic for musl builds
            isMuslNative ? false, # Build musl on native system (no cross pkgs)
          }:
          let
            # For musl native builds, use regular pkgs; for cross builds, configure cross-compilation
            crossPkgs =
              if isMuslNative then
                pkgs
              else
                import nixpkgs {
                  inherit localSystem crossSystem;
                  overlays = [ (import rust-overlay) ];
                };

            # Create Crane lib for cross target
            crossCraneLib = (crane.mkLib crossPkgs).overrideToolchain (
              p: p.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml
            );

            # Base RUSTFLAGS: always use mold linker for Linux
            baseRustFlags = "-C link-arg=-fuse-ld=mold";
            rustFlags = if extraRustFlags != "" then "${baseRustFlags} ${extraRustFlags}" else baseRustFlags;

            crossSrc = crossCraneLib.cleanCargoSource ./.;

            commonArgs = {
              src = crossSrc;
              pname = "lightning-js-${target}";
              strictDeps = true;
              doCheck = false;
              CARGO_BUILD_TARGET = target;
              RUSTFLAGS = rustFlags;
              nativeBuildInputs = [
                crossPkgs.pkg-config
                pkgs.mold
              ];
              buildInputs = if useStaticLibs then [ pkgs.pkgsStatic.openssl ] else [ crossPkgs.openssl ];
            };

            # Build dependencies separately for better caching
            cargoArtifacts = crossCraneLib.buildDepsOnly commonArgs;
          in
          crossCraneLib.buildPackage (
            commonArgs
            // {
              inherit cargoArtifacts;
              installPhaseCommand = mkInstallPhase nodeName;
            }
          );

        # ============================================================
        # Android cross-compilation helper
        # ============================================================
        mkAndroidPackage =
          {
            target, # Rust target triple (e.g., "aarch64-linux-android")
            nodeName, # Output filename for the .node addon
          }:
          let
            # Android NDK requires unfree license acceptance
            androidPkgs = import nixpkgs {
              system = localSystem;
              config.allowUnfree = true;
              config.android_sdk.accept_license = true;
              overlays = [ (import rust-overlay) ];
            };

            androidNdk = androidPkgs.androidenv.androidPkgs.ndk-bundle;

            androidCraneLib = (crane.mkLib androidPkgs).overrideToolchain (
              p: p.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml
            );

            # NDK toolchain paths - API 28+ for getentropy() support (required by aws-lc-sys)
            ndkToolchain = "${androidNdk}/libexec/android-sdk/ndk-bundle/toolchains/llvm/prebuilt/linux-x86_64";
            apiLevel = "28";

            # Map target triple to NDK binary prefix
            ndkPrefix =
              {
                "aarch64-linux-android" = "aarch64-linux-android";
              }
              .${target};

            # Map target triple to Cargo environment variable format (underscores, uppercase for linker)
            cargoEnvTarget =
              {
                "aarch64-linux-android" = {
                  lower = "aarch64_linux_android";
                  upper = "AARCH64_LINUX_ANDROID";
                };
              }
              .${target};

            commonArgs = {
              inherit src;
              pname = "lightning-js-${target}";
              strictDeps = true;
              doCheck = false;
              CARGO_BUILD_TARGET = target;
              ANDROID_NDK_HOME = "${androidNdk}/libexec/android-sdk/ndk-bundle";
              "CC_${cargoEnvTarget.lower}" = "${ndkToolchain}/bin/${ndkPrefix}${apiLevel}-clang";
              "CXX_${cargoEnvTarget.lower}" = "${ndkToolchain}/bin/${ndkPrefix}${apiLevel}-clang++";
              "AR_${cargoEnvTarget.lower}" = "${ndkToolchain}/bin/llvm-ar";
              "CARGO_TARGET_${cargoEnvTarget.upper}_LINKER" = "${ndkToolchain}/bin/${ndkPrefix}${apiLevel}-clang";
              nativeBuildInputs = [ pkgs.pkg-config ];
            };

            # Build dependencies separately for better caching
            cargoArtifacts = androidCraneLib.buildDepsOnly commonArgs;
          in
          androidCraneLib.buildPackage (
            commonArgs
            // {
              inherit cargoArtifacts;
              installPhaseCommand = mkInstallPhase nodeName;
            }
          );

        # ============================================================
        # Checks (run via `nix flake check`)
        # Reuses nativeCargoArtifacts for faster CI when package is also built
        # ============================================================
        checks = {
          clippy = craneLib.cargoClippy (
            nativeCommonArgs
            // {
              cargoArtifacts = nativeCargoArtifacts;
              cargoClippyExtraArgs = "--all-targets -- --deny warnings";
            }
          );

          fmt = craneLib.cargoFmt { inherit src; };

          # Also verify the package builds
          build = nativePackage;
        };

        # Instantiate native packages
        nativePackage = mkNativePackage { };
        debugPackage = mkNativePackage { isDebug = true; };
      in
      {
        inherit checks;

        packages = {
          default = nativePackage;
          debug = debugPackage;
        }
        # Cross-compilation packages are only available on Linux hosts
        // lib.optionalAttrs stdenv.isLinux {
          # Linux ARM64 with glibc (standard Linux distros)
          aarch64_unknown_linux_gnu = mkCrossPackage {
            crossSystem = "aarch64-linux";
            target = "aarch64-unknown-linux-gnu";
            nodeName = "lightning-js.linux-arm64-gnu.node";
          };

          # Linux x86_64 with musl (dynamically linked to musl.so)
          # Note: Uses -crt-static because cdylib output is incompatible with static musl
          x86_64_unknown_linux_musl = mkCrossPackage {
            crossSystem = localSystem;
            target = "x86_64-unknown-linux-musl";
            nodeName = "lightning-js.linux-x64-musl.node";
            extraRustFlags = "-C target-feature=-crt-static";
            useStaticLibs = true;
            isMuslNative = true;
          };

          # Linux ARM64 with musl (dynamically linked to musl.so for cdylib compatibility)
          aarch64_unknown_linux_musl = mkCrossPackage {
            crossSystem = {
              config = "aarch64-unknown-linux-musl";
            };
            target = "aarch64-unknown-linux-musl";
            nodeName = "lightning-js.linux-arm64-musl.node";
            extraRustFlags = "-C target-feature=-crt-static";
          };

          # Android ARM64 (modern Android devices)
          aarch64_linux_android = mkAndroidPackage {
            target = "aarch64-linux-android";
            nodeName = "lightning-js.android-arm64.node";
          };
        };

        devShells.default = pkgs.mkShell {
          name = "lightning-js-dev";

          packages = with pkgs; [
            nodejs_22 # JavaScript runtime
            yarn # Package manager
            rustToolchain # Rust compiler and tools (from rust-toolchain.toml)
            pkg-config # Finds system libraries
            openssl # TLS library
            mold # Fast linker (optional, improves build times)
            gcc # C compiler for build scripts
            just # Command runner (see justfile)
          ];

          PKG_CONFIG_PATH = "${pkgs.openssl.dev}/lib/pkgconfig";
          RUST_BACKTRACE = "1";

          shellHook = ''
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

        formatter = pkgs.nixfmt;
      }
    );
}
