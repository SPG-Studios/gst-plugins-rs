// SPDX-CopyrightText: 2026 Amyspark <amy@centricular.com>
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;

/**
 * SECTION:element-imagersoverlay
 *
 * The imagersoverlay element overlays an image loaded from file onto a video stream.
 *
 * Changing the positioning or overlay width and height properties at runtime
 * is supported, but it might be prudent to to protect the property setting
 * code with GST_BASE_TRANSFORM_LOCK and GST_BASE_TRANSFORM_UNLOCK, as
 * g_object_set() is not atomic for multiple properties passed in one go.
 *
 * Changing the image at runtime is supported.
 *
 * ## Example launch line
 *
 * ```bash
 * gst-launch-1.0 -v videotestsrc ! gdkpixbufoverlay location=image.png ! autovideosink
 * ```
 *
 * Since: 0.16
 */
mod imp;

glib::wrapper! {
    pub struct Overlay(ObjectSubclass<imp::ImageRsOverlay>) @extends gst_video::VideoFilter, gst_base::BaseTransform, gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "imagersoverlay",
        gst::Rank::NONE,
        Overlay::static_type(),
    )
}
