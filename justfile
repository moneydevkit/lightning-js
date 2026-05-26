default:
    @just --list

system := env("NIX_SYSTEM")

# Run all checks (fmt, clippy, build)
check:
    nix flake check

# Format code
fmt:
    cargo fmt
    nixfmt flake.nix

# Check formatting
fmt-check:
    nix build .#checks.{{system}}.fmt

# Run clippy
clippy:
    nix build .#checks.{{system}}.clippy

# Verify the package builds
build-check:
    nix build .#checks.{{system}}.build

# Auto-fix lint issues
fix:
    cargo clippy --all-targets --fix --allow-dirty --allow-staged

# Build native release binary
build:
    nix build

# Build debug binary (faster, for testing)
build-debug:
    nix build .#debug

# Clean build artifacts
clean:
    cargo clean
    rm -f result
