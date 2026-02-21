// SPDX-CopyrightText: 2026 Amyspark <amy@centricular.com>
// SPDX-License-Identifier: MPL-2.0
#![allow(clippy::non_send_fields_in_send_ty, unused_doc_comments)]

/**
 * plugin-image:
 *
 * Since: plugins-rs-0.16
 */
use gst::glib;

#[cfg(feature = "animated_formats")]
mod anim;
mod decoder;
mod encoder;
#[cfg(feature = "v1_24")]
mod overlay;

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    decoder::register(plugin)?;
    encoder::register(plugin)?;
    #[cfg(feature = "animated_formats")]
    anim::register(plugin)?;
    #[cfg(feature = "v1_24")]
    overlay::register(plugin)?;
    Ok(())
}

gst::plugin_define!(
    imagers,
    env!("CARGO_PKG_DESCRIPTION"),
    plugin_init,
    concat!(env!("CARGO_PKG_VERSION"), "-", env!("COMMIT_ID")),
    "MPL-2.0",
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_REPOSITORY"),
    env!("BUILD_REL_DATE")
);
