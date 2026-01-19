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

        # Build inputs shared across native builds and checks.
        # These are libraries linked into the final binary.
        commonBuildInputs =
          with pkgs;
          [ openssl ]
          ++ lib.optionals stdenv.isDarwin [
            # macOS requires these frameworks for network/TLS operations
            darwin.apple_sdk.frameworks.Security
            darwin.apple_sdk.frameworks.SystemConfiguration
          ];

        # ============================================================
        # Native build for the current system
        # ============================================================
        nativePackage = pkgs.callPackage (
          # Using callPackage for proper "splicing" - Nix automatically
          # provides the correct versions of dependencies for the target platform.
          # See: https://crane.dev/examples/cross-rust-overlay.html
          {
            lib,
            openssl,
            pkg-config,
            stdenv,
            darwin,
          }:
          craneLib.buildPackage {
            inherit src;
            pname = "lightning-js";

            # Enforce strict separation between build-time and runtime deps
            strictDeps = true;

            # pkg-config runs at build time to locate openssl
            # mold is a fast linker for ELF binaries (Linux targets only, not macOS Mach-O)
            nativeBuildInputs = [ pkg-config ] ++ lib.optionals stdenv.isLinux [ pkgs.mold ];

            # Libraries linked into the final binary
            buildInputs = [
              openssl
            ]
            ++ lib.optionals stdenv.isDarwin [
              darwin.apple_sdk.frameworks.Security
              darwin.apple_sdk.frameworks.SystemConfiguration
            ];

            # macOS minimum deployment target (matches CI)
            MACOSX_DEPLOYMENT_TARGET = lib.optionalString stdenv.isDarwin "10.13";

            # Custom install phase for NAPI-RS native addon.
            # Cargo builds a cdylib (.so on Linux, .dylib on macOS).
            # Node.js expects native addons to have .node extension.
            # The naming convention follows NAPI-RS: {name}.{platform}-{arch}.node
            installPhaseCommand = ''
              mkdir -p $out/lib

              # Copy the shared library with NAPI-RS naming convention
              if [ -f target/release/liblightning_js.so ]; then
                cp target/release/liblightning_js.so $out/lib/${nativeNodeName}
              elif [ -f target/release/liblightning_js.dylib ]; then
                cp target/release/liblightning_js.dylib $out/lib/${nativeNodeName}
              fi
            '';
          }
        ) { };

        # ============================================================
        # Debug build (faster compilation, no optimizations)
        # ============================================================
        debugPackage = pkgs.callPackage (
          {
            lib,
            openssl,
            pkg-config,
            stdenv,
            darwin,
          }:
          craneLib.buildPackage {
            inherit src;
            pname = "lightning-js";
            strictDeps = true;

            # Build in debug mode (no --release flag)
            cargoExtraArgs = "";
            CARGO_PROFILE = "dev";

            nativeBuildInputs = [ pkg-config ] ++ lib.optionals stdenv.isLinux [ pkgs.mold ];
            buildInputs = [
              openssl
            ]
            ++ lib.optionals stdenv.isDarwin [
              darwin.apple_sdk.frameworks.Security
              darwin.apple_sdk.frameworks.SystemConfiguration
            ];

            MACOSX_DEPLOYMENT_TARGET = lib.optionalString stdenv.isDarwin "10.13";

            # Debug builds output to target/debug/
            installPhaseCommand = ''
              mkdir -p $out/lib

              if [ -f target/debug/liblightning_js.so ]; then
                cp target/debug/liblightning_js.so $out/lib/${nativeNodeName}
              elif [ -f target/debug/liblightning_js.dylib ]; then
                cp target/debug/liblightning_js.dylib $out/lib/${nativeNodeName}
              fi
            '';
          }
        ) { };

        # ============================================================
        # Cross-compilation helper for glibc targets
        # ============================================================
        mkCrossPackage =
          {
            crossSystem, # Nix cross-compilation system (e.g., "aarch64-linux")
            target, # Rust target triple (e.g., "aarch64-unknown-linux-gnu")
            nodeName, # Output filename for the .node addon
            extraFlags ? "", # Additional RUSTFLAGS for compilation
          }:
          let
            # Import nixpkgs configured for cross-compilation.
            # localSystem = build machine, crossSystem = target machine.
            crossPkgs = import nixpkgs {
              inherit localSystem crossSystem;
              overlays = [ (import rust-overlay) ];
            };

            # Create Crane lib for cross target.
            # Using a function ensures proper toolchain splicing for cross-compilation.
            crossCraneLib = (crane.mkLib crossPkgs).overrideToolchain (
              p: p.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml
            );
          in
          crossPkgs.callPackage (
            {
              openssl,
              pkg-config,
            }:
            crossCraneLib.buildPackage {
              src = crossCraneLib.cleanCargoSource ./.;
              pname = "lightning-js";
              strictDeps = true;

              # Skip tests - can't run cross-compiled binaries on build host
              doCheck = false;

              # Tell Cargo which target to build for
              CARGO_BUILD_TARGET = target;

              # Use RUSTFLAGS (priority #2) to override config.toml's target-specific flags.
              # For musl targets, we need -crt-static to allow cdylib output.
              RUSTFLAGS = "-C link-arg=-fuse-ld=mold" + (if extraFlags != "" then " ${extraFlags}" else "");

              # pkg-config runs at build time to locate openssl
              # mold is required because .cargo/config.toml sets -fuse-ld=mold for Linux targets
              nativeBuildInputs = [
                pkg-config
                pkgs.mold
              ];
              buildInputs = [ openssl ];

              # Find and copy the cross-compiled shared library.
              # The library is in target/{target}/release/ for cross builds.
              installPhaseCommand = ''
                mkdir -p $out/lib

                # Use find to locate the .so file in the target-specific directory
                find target -name "liblightning_js.so" -exec cp {} $out/lib/${nodeName} \; 2>/dev/null || true
              '';
            }
          ) { };

        # ============================================================
        # Cross-compilation helper for musl targets
        # Note: Uses dynamic musl (-crt-static) because cdylib (shared library)
        # output is incompatible with static musl (+crt-static).
        # ============================================================
        mkMuslPackage =
          {
            target, # Rust target triple (e.g., "x86_64-unknown-linux-musl")
            nodeName, # Output filename for the .node addon
          }:
          let
            muslCraneLib = (crane.mkLib pkgs).overrideToolchain (
              p: p.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml
            );
          in
          muslCraneLib.buildPackage {
            inherit src;
            pname = "lightning-js";
            strictDeps = true;

            # Skip tests for cross-compiled targets
            doCheck = false;

            # Build for musl target
            CARGO_BUILD_TARGET = target;

            # Use RUSTFLAGS (priority #2) to override config.toml's target-specific flags.
            # -crt-static: Required for cdylib output (musl defaults to +crt-static which
            # is incompatible with shared libraries). The addon will depend on musl.so.
            # -fuse-ld=mold: Fast linker (matches config.toml for consistency).
            RUSTFLAGS = "-C target-feature=-crt-static -C link-arg=-fuse-ld=mold";

            # pkg-config runs at build time to locate openssl
            # mold is required because .cargo/config.toml sets -fuse-ld=mold for Linux targets
            nativeBuildInputs = with pkgs; [
              pkg-config
              mold
            ];

            # Use static versions of libraries for musl builds
            buildInputs = with pkgs.pkgsStatic; [ openssl ];

            installPhaseCommand = ''
              mkdir -p $out/lib

              # Find and copy the musl-linked shared library
              find target -name "liblightning_js.so" -exec cp {} $out/lib/${nodeName} \; 2>/dev/null || true
            '';
          };

        # ============================================================
        # Cross-compilation helper for Android targets
        # ============================================================
        mkAndroidPackage =
          {
            target, # Rust target triple (e.g., "aarch64-linux-android")
            nodeName, # Output filename for the .node addon
            androidAbi, # Android ABI (e.g., "arm64-v8a")
          }:
          let
            # Android NDK requires unfree license acceptance
            androidPkgs = import nixpkgs {
              system = localSystem;
              config.allowUnfree = true;
              config.android_sdk.accept_license = true;
              overlays = [ (import rust-overlay) ];
            };

            # Android NDK from nixpkgs
            androidNdk = androidPkgs.androidenv.androidPkgs.ndk-bundle;

            androidCraneLib = (crane.mkLib androidPkgs).overrideToolchain (
              p: p.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml
            );

            # NDK toolchain paths
            # Using API 28+ for getentropy() support required by aws-lc-sys
            ndkToolchain = "${androidNdk}/libexec/android-sdk/ndk-bundle/toolchains/llvm/prebuilt/linux-x86_64";
            apiLevel = "28";
          in
          androidCraneLib.buildPackage {
            inherit src;
            pname = "lightning-js";
            strictDeps = true;

            # Skip tests - can't run Android binaries on Linux
            doCheck = false;

            # Tell Cargo which target to build for
            CARGO_BUILD_TARGET = target;

            # Android NDK environment variables
            ANDROID_NDK_HOME = "${androidNdk}/libexec/android-sdk/ndk-bundle";
            CC_aarch64_linux_android = "${ndkToolchain}/bin/aarch64-linux-android${apiLevel}-clang";
            CXX_aarch64_linux_android = "${ndkToolchain}/bin/aarch64-linux-android${apiLevel}-clang++";
            AR_aarch64_linux_android = "${ndkToolchain}/bin/llvm-ar";
            CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER = "${ndkToolchain}/bin/aarch64-linux-android${apiLevel}-clang";

            CC_armv7_linux_androideabi = "${ndkToolchain}/bin/armv7a-linux-androideabi${apiLevel}-clang";
            CXX_armv7_linux_androideabi = "${ndkToolchain}/bin/armv7a-linux-androideabi${apiLevel}-clang++";
            AR_armv7_linux_androideabi = "${ndkToolchain}/bin/llvm-ar";
            CARGO_TARGET_ARMV7_LINUX_ANDROIDEABI_LINKER = "${ndkToolchain}/bin/armv7a-linux-androideabi${apiLevel}-clang";

            nativeBuildInputs = with pkgs; [ pkg-config ];

            installPhaseCommand = ''
              mkdir -p $out/lib

              # Find and copy the Android shared library
              find target -name "liblightning_js.so" -exec cp {} $out/lib/${nodeName} \; 2>/dev/null || true
            '';
          };

        # ============================================================
        # Checks (run via `nix flake check`)
        # ============================================================

        # Pre-build dependencies only (cached separately from source changes).
        # This speeds up incremental builds significantly.
        cargoArtifacts = craneLib.buildDepsOnly {
          inherit src;
          pname = "lightning-js";
          # mold links ELF (Linux targets only)
          nativeBuildInputs = with pkgs; [ pkg-config ] ++ lib.optionals stdenv.isLinux [ mold ];
          buildInputs = commonBuildInputs;
        };

        # Run clippy linter with warnings as errors
        cargoClippy = craneLib.cargoClippy {
          inherit src cargoArtifacts;
          pname = "lightning-js";
          cargoClippyExtraArgs = "--all-targets -- --deny warnings";
          nativeBuildInputs = with pkgs; [ pkg-config ] ++ lib.optionals stdenv.isLinux [ mold ];
          buildInputs = commonBuildInputs;
        };

        # Check code formatting with rustfmt
        cargoFmt = craneLib.cargoFmt { inherit src; };
      in
      {
        checks = {
          inherit nativePackage cargoClippy cargoFmt;
        };

        packages = {
          default = nativePackage;
          debug = debugPackage; # Fast build without optimizations
        }
        # Cross-compilation packages are only available on Linux hosts
        // pkgs.lib.optionalAttrs pkgs.stdenv.isLinux {
          # Linux ARM64 with glibc (for standard Linux distros)
          aarch64_unknown_linux_gnu = mkCrossPackage {
            crossSystem = "aarch64-linux";
            target = "aarch64-unknown-linux-gnu";
            nodeName = "lightning-js.linux-arm64-gnu.node";
          };

          # Linux x86_64 with musl (dynamically linked to musl.so)
          x86_64_unknown_linux_musl = mkMuslPackage {
            target = "x86_64-unknown-linux-musl";
            nodeName = "lightning-js.linux-x64-musl.node";
          };

          # Linux ARM64 with musl (static, portable binary)
          # Linux ARM64 with musl (dynamically linked to musl.so for cdylib compatibility)
          aarch64_unknown_linux_musl = mkCrossPackage {
            crossSystem = {
              config = "aarch64-unknown-linux-musl";
            };
            target = "aarch64-unknown-linux-musl";
            nodeName = "lightning-js.linux-arm64-musl.node";
            # -crt-static: Required for cdylib output (musl defaults to +crt-static)
            extraFlags = "-C target-feature=-crt-static";
          };

          # Android ARM64 (most modern Android devices)
          aarch64_linux_android = mkAndroidPackage {
            target = "aarch64-linux-android";
            nodeName = "lightning-js.android-arm64.node";
            androidAbi = "arm64-v8a";
          };

          # Android ARMv7 (older 32-bit devices)
          armv7_linux_androideabi = mkAndroidPackage {
            target = "armv7-linux-androideabi";
            nodeName = "lightning-js.android-arm-eabi.node";
            androidAbi = "armeabi-v7a";
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

          # Tell pkg-config where to find OpenSSL headers and libraries
          PKG_CONFIG_PATH = "${pkgs.openssl.dev}/lib/pkgconfig";

          # Enable Rust backtraces for debugging
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

        # Nix code formatter (run via `nix fmt`)
        formatter = pkgs.nixfmt-rfc-style;
      }
    );
}
