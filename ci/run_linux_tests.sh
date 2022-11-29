#! /usr/bin/sh

cargo build --locked --color=always --workspace --all-targets
G_DEBUG=fatal_warnings cargo test --locked --color=always --workspace --all-targets

cargo build --locked --color=always --workspace --all-targets --all-features
G_DEBUG=fatal_warnings cargo test --locked --color=always --workspace --all-targets --all-features

cargo build --locked --color=always --workspace --all-targets --no-default-features
G_DEBUG=fatal_warnings cargo test --locked --color=always --workspace --all-targets --no-default-features
