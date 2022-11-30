#! /usr/bin/bash

set -eu

crates=(
    "tutorial"
    "version-helper"

    "audio/audiofx"
    "audio/claxon"
    "audio/csound"
    "audio/lewton"
    "audio/spotify"

    "generic/file"
    "generic/sodium"
    "generic/threadshare"

    "mux/flavors"
    "mux/fmp4"
    "mux/mp4"

    "net/aws"
    "net/hlssink3"
    "net/ndi"
    "net/onvif"
    "net/raptorq"
    "net/reqwest"
    "net/rtp"
    "net/webrtchttp"
    "net/webrtc"
    "net/webrtc/protocol"
    "net/webrtc/signalling"

    "text/ahead"
    "text/json"
    "text/regex"
    "text/wrap"

    "utils/fallbackswitch"
    "utils/togglerecord"
    "utils/tracers"
    "utils/uriplaylistbin"

    "video/cdg"
    "video/closedcaption"
    "video/dav1d"
    "video/ffv1"
    "video/gif"
    "video/gtk4"
    "video/hsv"
    "video/png"
    "video/rav1e"
    "video/videofx"
    "video/webp"
)

features_matrix=(
    "--all-features"
    ""
    "--no-default-features"
)

for features in "${features_matrix[@]}"; do
    for crate in "${crates[@]}"; do
        LocalFeatures=$features;

        echo "Building $crate with features: $LocalFeatures"

        cargo build --color=always --manifest-path "$crate/Cargo.toml" --all-targets $LocalFeatures
        G_DEBUG=fatal_warnings cargo test --no-fail-fast --color=always --manifest-path "$crate/Cargo.toml" --all-targets $LocalFeatures
    done
done
