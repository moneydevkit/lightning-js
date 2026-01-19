# Lightning JS Build Commands
# Run `just` to see available commands
# Requires: Nix (https://nixos.org/download)

# Default: show available commands
default:
    @just --list

# Build native release binary
build:
    nix build

# Build debug binary (faster, for testing)
build-debug:
    nix build .#debug

# Build all Linux targets
build-linux: build build-linux-arm64 build-linux-musl build-linux-arm64-musl

# Build Linux ARM64 (glibc)
build-linux-arm64:
    nix build .#aarch64_unknown_linux_gnu

# Build Linux x64 (musl, static)
build-linux-musl:
    nix build .#x86_64_unknown_linux_musl

# Build Linux ARM64 (musl, static)
build-linux-arm64-musl:
    nix build .#aarch64_unknown_linux_musl

# Build Android ARM64
build-android:
    nix build .#aarch64_linux_android

# Build Android ARMv7
build-android-armv7:
    nix build .#armv7_linux_androideabi

# Build all Android targets
build-android-all: build-android build-android-arm

# Run lints (clippy + rustfmt)
check:
    nix flake check

# Clean build artifacts
clean:
    rm -rf node_modules
    cargo clean
    rm -f result
