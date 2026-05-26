default:
    @just --list

# Default to empty so non-Nix recipes (e.g. `just ci`) work without NIX_SYSTEM set.
# Nix-dependent recipes below will fail loudly if invoked without it.
system := env("NIX_SYSTEM", "")

# Run all checks (fmt, clippy, build)
check:
    nix flake check

# Run CI checks without Nix
ci:
    NIX_HARDENING_ENABLE="" cargo fmt -- --check
    NIX_HARDENING_ENABLE="" cargo clippy --all-targets -- --deny warnings
    NIX_HARDENING_ENABLE="" cargo test

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

# Clean build artifacts
clean:
    cargo clean
    rm -f result
