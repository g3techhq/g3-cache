set windows-shell := ["powershell.exe", "-NoLogo", "-NoProfile", "-Command"]

default:
    @just --list

setup:
    lefthook install

format:
    cargo fmt --all
    cargo fmt --manifest-path macros/Cargo.toml

format-check:
    cargo fmt --all -- --check
    cargo fmt --manifest-path macros/Cargo.toml -- --check

check:
    cargo check
    cargo check --features server
    cargo check --features mobile
    cargo check --features desktop
    cargo check --features web --target wasm32-unknown-unknown
    cargo check --manifest-path macros/Cargo.toml

lint:
    cargo clippy --all-targets --no-deps --features server
    cargo clippy --manifest-path macros/Cargo.toml --all-targets --no-deps

lint-strict:
    cargo clippy --all-targets --no-deps --features server -- -D warnings
    cargo clippy --no-deps --features mobile -- -D warnings
    cargo clippy --no-deps --features desktop -- -D warnings
    cargo clippy --no-deps --features web --target wasm32-unknown-unknown -- -D warnings
    cargo clippy --manifest-path macros/Cargo.toml --all-targets --no-deps -- -D warnings

test:
    cargo nextest run --features server
    cargo nextest run --lib
    cargo nextest run --manifest-path macros/Cargo.toml --no-tests pass
    cargo test --doc --features server
    cargo test --doc --manifest-path macros/Cargo.toml

spell:
    typos

security:
    cargo deny check

pre-push: format-check check lint-strict test testbed spell

quality: pre-push

package-check:
    cargo package --allow-dirty --manifest-path macros/Cargo.toml
    cargo package --allow-dirty --list

ci: quality security package-check

package:
    cargo package --manifest-path macros/Cargo.toml
    cargo package

# Builds the test bed against real Dioxus server functions, both sides.
testbed:
    cargo check --manifest-path testbed/Cargo.toml --no-default-features --features server
    cargo check --manifest-path testbed/Cargo.toml --target wasm32-unknown-unknown
