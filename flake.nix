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
        mkInstallPhase =
          { nodeName, targetDir }:
          ''
            mkdir -p $out/lib
            # Copy the shared library with NAPI-RS naming convention
            # Cargo builds a cdylib (.so on Linux, .dylib on macOS)
            # Node.js expects native addons to have .node extension
            if [ -f ${targetDir}/liblightning_js.so ]; then
              cp ${targetDir}/liblightning_js.so $out/lib/${nodeName}
            elif [ -f ${targetDir}/liblightning_js.dylib ]; then
              cp ${targetDir}/liblightning_js.dylib $out/lib/${nodeName}
            fi
          '';

        # Generate install phase for cross-compiled builds (searches target directory)
        mkCrossInstallPhase =
          nodeName:
          ''
            mkdir -p $out/lib
            find target -name "liblightning_js.so" -exec cp {} $out/lib/${nodeName} \; 2>/dev/null || true
          '';

        # ============================================================
        # Native build helper (release or debug)
        # ============================================================
        mkNativePackage =
          { isDebug ? false }:
          let
            targetDir = "target/${if isDebug then "debug" else "release"}";
          in
          pkgs.callPackage (
            # Using callPackage for proper "splicing" - Nix automatically
            # provides the correct versions of dependencies for the target platform.
            # See: https://crane.dev/examples/cross-rust-overlay.html
            {
              lib,
              openssl,
              pkg-config,
              stdenv,
            }:
            craneLib.buildPackage {
              inherit src;
              pname = "lightning-js";
              strictDeps = true;

              # Set profile for debug builds (Crane defaults to release)
              CARGO_PROFILE = lib.optionalString isDebug "dev";

              nativeBuildInputs =
                [ pkg-config ]
                ++ lib.optionals stdenv.isLinux [ pkgs.mold ];

              buildInputs =
                [ openssl ]
                ++ lib.optionals stdenv.isDarwin (
                  with pkgs.darwin.apple_sdk.frameworks; [ Security SystemConfiguration ]
                );

              # macOS minimum deployment target (matches CI)
              MACOSX_DEPLOYMENT_TARGET = lib.optionalString stdenv.isDarwin "11.0";

              installPhaseCommand = mkInstallPhase {
                nodeName = nativeNodeName;
                inherit targetDir;
              };
            }
          ) { };

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
            rustFlags =
              if extraRustFlags != "" then "${baseRustFlags} ${extraRustFlags}" else baseRustFlags;
          in
          crossPkgs.callPackage (
            { openssl, pkg-config }:
            crossCraneLib.buildPackage {
              src = crossCraneLib.cleanCargoSource ./.;
              pname = "lightning-js";
              strictDeps = true;

              # Skip tests - can't run cross-compiled binaries on build host
              doCheck = false;

              CARGO_BUILD_TARGET = target;
              RUSTFLAGS = rustFlags;

              nativeBuildInputs = [ pkg-config pkgs.mold ];
              buildInputs = if useStaticLibs then [ pkgs.pkgsStatic.openssl ] else [ openssl ];

              installPhaseCommand = mkCrossInstallPhase nodeName;
            }
          ) { };

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
                "armv7-linux-androideabi" = "armv7a-linux-androideabi";
              }
              .${target};

            # Map target triple to Cargo environment variable format (underscores, uppercase for linker)
            cargoEnvTarget =
              {
                "aarch64-linux-android" = {
                  lower = "aarch64_linux_android";
                  upper = "AARCH64_LINUX_ANDROID";
                };
                "armv7-linux-androideabi" = {
                  lower = "armv7_linux_androideabi";
                  upper = "ARMV7_LINUX_ANDROIDEABI";
                };
              }
              .${target};
          in
          androidCraneLib.buildPackage {
            inherit src;
            pname = "lightning-js";
            strictDeps = true;

            doCheck = false;

            CARGO_BUILD_TARGET = target;
            ANDROID_NDK_HOME = "${androidNdk}/libexec/android-sdk/ndk-bundle";

            # Set CC, CXX, AR, and linker for the specific target
            "CC_${cargoEnvTarget.lower}" = "${ndkToolchain}/bin/${ndkPrefix}${apiLevel}-clang";
            "CXX_${cargoEnvTarget.lower}" = "${ndkToolchain}/bin/${ndkPrefix}${apiLevel}-clang++";
            "AR_${cargoEnvTarget.lower}" = "${ndkToolchain}/bin/llvm-ar";
            "CARGO_TARGET_${cargoEnvTarget.upper}_LINKER" = "${ndkToolchain}/bin/${ndkPrefix}${apiLevel}-clang";

            nativeBuildInputs = [ pkgs.pkg-config ];

            installPhaseCommand = mkCrossInstallPhase nodeName;
          };

        # ============================================================
        # Checks (run via `nix flake check`)
        # ============================================================
        checks =
          let
            # Build inputs only used for checks (not for package builds, which use callPackage splicing)
            checkBuildInputs =
              [ pkgs.openssl ]
              ++ lib.optionals stdenv.isDarwin (
                with pkgs.darwin.apple_sdk.frameworks; [ Security SystemConfiguration ]
              );
            checkNativeBuildInputs =
              [ pkgs.pkg-config ]
              ++ lib.optionals stdenv.isLinux [ pkgs.mold ];

            # Pre-build dependencies only (cached separately from source changes)
            cargoArtifacts = craneLib.buildDepsOnly {
              inherit src;
              pname = "lightning-js";
              nativeBuildInputs = checkNativeBuildInputs;
              buildInputs = checkBuildInputs;
            };
          in
          {
            clippy = craneLib.cargoClippy {
              inherit src cargoArtifacts;
              pname = "lightning-js";
              cargoClippyExtraArgs = "--all-targets -- --deny warnings";
              nativeBuildInputs = checkNativeBuildInputs;
              buildInputs = checkBuildInputs;
            };

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

        packages =
          {
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
              crossSystem = { config = "aarch64-unknown-linux-musl"; };
              target = "aarch64-unknown-linux-musl";
              nodeName = "lightning-js.linux-arm64-musl.node";
              extraRustFlags = "-C target-feature=-crt-static";
            };

            # Android ARM64 (modern Android devices)
            aarch64_linux_android = mkAndroidPackage {
              target = "aarch64-linux-android";
              nodeName = "lightning-js.android-arm64.node";
            };

            # Android ARMv7 (older 32-bit devices)
            armv7_linux_androideabi = mkAndroidPackage {
              target = "armv7-linux-androideabi";
              nodeName = "lightning-js.android-arm-eabi.node";
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
