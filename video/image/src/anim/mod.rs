// SPDX-CopyrightText: 2026 Amyspark <amy@centricular.com>
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;

/**
 * SECTION:element-animatedimagersdec
 *
 * Decodes animated image formats using pure Rust to raw video
 *
 * ## Example launch line
 *
 * ```bash
 * gst-launch-1.0 filesrc location=$PATH ! typefind ! animatedimagersdec ! videoconvert ! autovideosink
 * ```
 *
 * Since: 0.16
 */
mod imp;

glib::wrapper! {
    pub struct Decoder(ObjectSubclass<imp::Decoder>) @extends gst::Element, gst::Object;
}

fn typefind_register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    use gst::{Caps, TypeFind, TypeFindProbability};

    TypeFind::register(
        Some(plugin),
        "image/x-gst-apng",
        // Needs to be bumped before typefind
        gst::Rank::PRIMARY + 100,
        Some("png"),
        Some(&Caps::builder("image/x-gst-apng").build()),
        |typefind| {
            // I have no idea how long till the first iDAT,
            // so we'll need to swallow the whole typefind
            let len = typefind.length().unwrap_or(0);
            if len > 0
                && let Some(data) = typefind.peek(0, len.min(u32::MAX as u64) as u32)
            {
                let cursor = std::io::Cursor::new(data);
                let mut options = png::DecodeOptions::default();
                options.set_ignore_checksums(true);
                options.set_ignore_iccp_chunk(true);
                options.set_ignore_text_chunk(true);
                // read_header_info is not enough, it just parses basic info
                // we need to find the acTL chunk
                if let Ok(reader) = png::Decoder::new_with_options(cursor, options).read_info()
                    && reader.info().is_animated()
                {
                    typefind.suggest(
                        TypeFindProbability::Maximum,
                        &Caps::builder("image/x-gst-apng").build(),
                    );
                }
            }
        },
    )
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    typefind_register(plugin)?;
    gst::Element::register(
        Some(plugin),
        "animatedimagersdec",
        gst::Rank::SECONDARY - 1,
        Decoder::static_type(),
    )
}
