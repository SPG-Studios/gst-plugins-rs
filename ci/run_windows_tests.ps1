$env:ErrorActionPreference='Stop'

# List of all the crates we want to build
# We need to do this manually to avoid trying
# to build the ones which can't work on windows
[string[]] $crates = @(
    "tutorial",
    "version-helper",

    "audio/audiofx",
    "audio/claxon",
    # "audio/csound"
    "audio/lewton",
    "audio/spotify",

    "generic/file",
    "generic/sodium",
    "generic/threadshare",

    "mux/flavors",
    "mux/fmp4",
    "mux/mp4",

    "net/aws",
    "net/hlssink3",
    "net/ndi",
    "net/onvif",
    "net/raptorq",
    "net/reqwest",
    "net/rtp",
    "net/webrtchttp",
    "net/webrtc",
    "net/webrtc/protocol",
    "net/webrtc/signalling",

    "text/ahead",
    "text/json",
    "text/regex",
    "text/wrap",

    "utils/fallbackswitch",
    "utils/togglerecord",
    "utils/tracers",
    "utils/uriplaylistbin",

    "video/cdg",
    "video/closedcaption",
    "video/dav1d",
    "video/ffv1",
    "video/gif",
    "video/gtk4",
    "video/hsv",
    "video/png",
    "video/rav1e",
    "video/videofx"
    # "video/webp"
)

[string[]] $features_matrix = @(
    "--all-features"
    "",
    "--no-default-features",
)

function Run-Tests {
    param (
        $Features
    )

    foreach($crate in $crates) {
        $LocalFeatures = $Features

        # Don't append feature flags if the string is null/empty
        # Or when we want to build without default features
        if ($env:LocalFeatures -and ($env:LocalFeatures -ne '--no-default-features')) {
            if ($crate -eq 'video/gtk4') {
                # --all-features would enable glx, wayland and eglx11 which
                # can't be built on windows
                $env:LocalFeatures = ""
            }
        }

        Write-Host "Building $crate with features: $LocalFeatures"
        cargo build --color=always --manifest-path $crate/Cargo.toml --all-targets $LocalFeatures

        if (!$?) {
            Write-Host "Build failed"
            Exit 1
        }

        $env:G_DEBUG="fatal_warnings"
        cargo test --no-fail-fast --color=always --manifest-path $crate/Cargo.toml --all-targets $LocalFeatures

        if (!$?) {
            Write-Host "Tests failed"
            Exit 1
        }
    }
}

foreach($feature in $features_matrix) {
    Run-Tests -Features $feature
}
