# List recipes
default:
    @just --list

build:
    cargo build

# Run against your real Signal account (links on first start)
run *ARGS:
    cargo run -- {{ARGS}}

# Run with sample data; never touches Signal or your message store
demo:
    cargo run -- --demo

test:
    cargo test

clippy:
    cargo clippy --all-targets -- -D warnings

fmt:
    cargo fmt
