// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

pub mod depay;
pub mod pay;
mod payload_header;
mod picture_segment;
mod profile_level;

pub(crate) const MEDIA_TYPE_JXSC: &str = "image/x-jxsc";
pub(crate) const MEDIA_TYPE_JXSV: &str = "video/x-jxsv";

const RFC9134_SAMPLING_VALUES: [&str; 13] = [
    "YCbCr-4:4:4",
    "YCbCr-4:2:2",
    "YCbCr-4:2:0",
    "CLYCbCr-4:4:4",
    "CLYCbCr-4:2:2",
    "CLYCbCr-4:2:0",
    "ICtCp-4:4:4",
    "ICtCp-4:2:2",
    "ICtCp-4:2:0",
    "RGB",
    "XYZ",
    "KEY",
    "UNSPECIFIED",
];

/// Pad-template structure for JPEG XS media (`image/x-jxsc` / `video/x-jxsv`).
pub(crate) fn media_structure(name: &str) -> gst::Structure {
    gst::Structure::builder(name)
        .field("alignment", "frame")
        .field("interlace-mode", "progressive")
        .field("width", gst::IntRange::new(1i32, 32_767i32))
        .field("height", gst::IntRange::new(1i32, 32_767i32))
        .field("sampling", gst::List::new(RFC9134_SAMPLING_VALUES))
        .field("depth", gst::List::new([8i32, 10, 12, 16]))
        .field(
            "framerate",
            gst::FractionRange::new(gst::Fraction::new(0, 1), gst::Fraction::new(i32::MAX, 1)),
        )
        .build()
}

pub(crate) fn media_pad_caps() -> gst::Caps {
    gst::Caps::builder_full()
        .structure(media_structure(MEDIA_TYPE_JXSC))
        .structure(media_structure(MEDIA_TYPE_JXSV))
        .build()
}

#[cfg(test)]
mod tests;
