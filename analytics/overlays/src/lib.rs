// Copyright (C) 2026 Jeremy Whiting <jeremy.whiting@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0
#![allow(clippy::non_send_fields_in_send_ty, unused_doc_comments)]

/**
 * plugin-overlays:
 *
 * Since: plugins-rs-0.16.0
 */
use gst::glib;

mod autobin;
mod color;
mod coordination;
mod geometry;
#[cfg(feature = "gl")]
mod glsupport;
pub mod hooks;
mod keypointsoverlay;
#[cfg(feature = "gl")]
mod keypointsoverlaygl;
mod lifecycle;
mod meta_transform;
mod keypointsoverlaybin;
mod objectdetectionoverlay;
mod objectdetectionoverlaybin;
#[cfg(feature = "gl")]
mod objectdetectionoverlaygl;
mod overlaycompositorbin;
mod segmentationoverlaybin;
mod overlay_intent;
mod overlaycompositor;
#[cfg(feature = "gl")]
mod overlaycompositorgl;
mod placement;
mod render;
mod segmentationoverlay;
#[cfg(feature = "gl")]
mod segmentationoverlaygl;

/// The drawing vocabulary hosts use to emit custom content from a [`hooks::DrawHook`].
pub use render::DrawCommand;

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    // Register the cross-element claimed-region meta so overlay elements can
    // coordinate placement (see the `coordination` module).
    coordination::register();
    // Register the deferred label-intent meta used by the compositor (deferred
    // label rendering).
    overlay_intent::register();

    objectdetectionoverlay::register(plugin)?;
    segmentationoverlay::register(plugin)?;
    keypointsoverlay::register(plugin)?;
    overlaycompositor::register(plugin)?;
    objectdetectionoverlaybin::register(plugin)?;
    keypointsoverlaybin::register(plugin)?;
    segmentationoverlaybin::register(plugin)?;
    overlaycompositorbin::register(plugin)?;
    #[cfg(feature = "gl")]
    objectdetectionoverlaygl::register(plugin)?;
    #[cfg(feature = "gl")]
    segmentationoverlaygl::register(plugin)?;
    #[cfg(feature = "gl")]
    keypointsoverlaygl::register(plugin)?;
    #[cfg(feature = "gl")]
    overlaycompositorgl::register(plugin)?;
    Ok(())
}

gst::plugin_define!(
    overlays,
    env!("CARGO_PKG_DESCRIPTION"),
    plugin_init,
    concat!(env!("CARGO_PKG_VERSION"), "-", env!("COMMIT_ID")),
    "MPL-2.0",
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_REPOSITORY"),
    env!("BUILD_REL_DATE")
);

#[cfg(test)]
mod tests {
    use super::*;
    use gst::prelude::*;
    use gst_base::prelude::BaseTransformExt;
    use gst_video::VideoCapsBuilder;
    use std::sync::Once;

    fn ensure_gstreamer_initialized() {
        static GST_INIT: Once = Once::new();
        GST_INIT.call_once(|| {
            gst::init().expect("Failed to initialize GStreamer for tests");
        });
    }

    #[test]
    fn object_detection_overlay_property_defaults_and_roundtrip() {
        ensure_gstreamer_initialized();
        let e = glib::Object::builder::<objectdetectionoverlay::ObjectDetectionOverlay>().build();

        assert!(!e.property::<bool>("render-enabled"));
        assert_eq!(
            e.property::<u32>("object-detection-outline-color"),
            0xFFFF_FFFF
        );
        assert!(e.property::<bool>("draw-labels"));
        assert!(e.property::<bool>("draw-tracking-labels"));
        assert_eq!(e.property::<u32>("labels-color"), 0xFFFF_FFFF);
        assert!(!e.property::<bool>("filled-box"));
        assert_eq!(e.property::<u64>("expire-overlay"), 1_000_000_000);
        assert!(e.property::<bool>("tracking-outline-colors"));
        assert!(!e.property::<bool>("suppress-builtin-rendering"));
        assert!(e.property::<bool>("publish-claimed-regions"));

        e.set_property("draw-labels", false);
        e.set_property("filled-box", true);
        e.set_property("expire-overlay", 2_000_000_000u64);
        e.set_property("suppress-builtin-rendering", true);
        e.set_property("publish-claimed-regions", false);

        assert!(!e.property::<bool>("draw-labels"));
        assert!(e.property::<bool>("filled-box"));
        assert_eq!(e.property::<u64>("expire-overlay"), 2_000_000_000);
        assert!(e.property::<bool>("suppress-builtin-rendering"));
        assert!(!e.property::<bool>("publish-claimed-regions"));
    }

    #[test]
    fn segmentation_overlay_property_defaults_and_roundtrip() {
        ensure_gstreamer_initialized();
        let e = glib::Object::builder::<segmentationoverlay::SegmentationOverlay>().build();

        assert!(!e.property::<bool>("render-enabled"));
        assert_eq!(e.property::<u32>("hint-maximum-segment-type"), 10);
        assert_eq!(e.property::<Option<String>>("selected-types"), None);
        assert!(e.property::<bool>("publish-claimed-regions"));

        e.set_property("hint-maximum-segment-type", 64u32);
        e.set_property("selected-types", Some("person;car".to_string()));
        e.set_property("publish-claimed-regions", false);

        assert_eq!(e.property::<u32>("hint-maximum-segment-type"), 64);
        assert_eq!(
            e.property::<Option<String>>("selected-types"),
            Some("person;car".to_string())
        );
        assert!(!e.property::<bool>("publish-claimed-regions"));
    }

    #[test]
    fn keypoints_overlay_property_defaults_and_roundtrip() {
        ensure_gstreamer_initialized();
        let e = glib::Object::builder::<keypointsoverlay::KeypointsOverlay>().build();

        assert!(!e.property::<bool>("render-enabled"));
        assert_eq!(e.property::<u32>("keypoint-color"), 0xFFFF_0000);
        assert_eq!(e.property::<f64>("keypoint-radius"), 3.0);
        assert!(e.property::<bool>("draw-labels"));
        assert_eq!(e.property::<u32>("labels-color"), 0xFFFF_FFFF);
        assert!(!e.property::<bool>("draw-skeleton"));
        assert_eq!(e.property::<u32>("skeleton-color"), 0xFF00_FF00);
        assert_eq!(e.property::<f64>("skeleton-line-width"), 2.0);
        assert_eq!(e.property::<Option<String>>("semantic-tag"), None);
        assert!(!e.property::<bool>("suppress-builtin-rendering"));
        assert!(e.property::<bool>("publish-claimed-regions"));

        e.set_property("draw-skeleton", true);
        e.set_property("skeleton-line-width", 6.0f64);
        e.set_property("semantic-tag", Some("pose/".to_string()));
        e.set_property("suppress-builtin-rendering", true);
        e.set_property("publish-claimed-regions", false);

        assert!(e.property::<bool>("draw-skeleton"));
        assert_eq!(e.property::<f64>("skeleton-line-width"), 6.0);
        assert_eq!(
            e.property::<Option<String>>("semantic-tag"),
            Some("pose/".to_string())
        );
        assert!(e.property::<bool>("suppress-builtin-rendering"));
        assert!(!e.property::<bool>("publish-claimed-regions"));
    }

    #[test]
    fn object_detection_overlay_passthrough_toggles_with_render_enabled() {
        ensure_gstreamer_initialized();
        let e = glib::Object::builder::<objectdetectionoverlay::ObjectDetectionOverlay>().build();

        assert!(e.upcast_ref::<gst_base::BaseTransform>().is_passthrough());

        e.set_property("render-enabled", true);
        assert!(!e.upcast_ref::<gst_base::BaseTransform>().is_passthrough());

        e.set_property("render-enabled", false);
        assert!(e.upcast_ref::<gst_base::BaseTransform>().is_passthrough());
    }

    #[test]
    fn segmentation_overlay_passthrough_toggles_with_render_enabled() {
        ensure_gstreamer_initialized();
        let e = glib::Object::builder::<segmentationoverlay::SegmentationOverlay>().build();

        assert!(e.upcast_ref::<gst_base::BaseTransform>().is_passthrough());

        e.set_property("render-enabled", true);
        assert!(!e.upcast_ref::<gst_base::BaseTransform>().is_passthrough());

        e.set_property("render-enabled", false);
        assert!(e.upcast_ref::<gst_base::BaseTransform>().is_passthrough());
    }

    #[test]
    fn keypoints_overlay_passthrough_toggles_with_render_enabled() {
        ensure_gstreamer_initialized();
        let e = glib::Object::builder::<keypointsoverlay::KeypointsOverlay>().build();

        assert!(e.upcast_ref::<gst_base::BaseTransform>().is_passthrough());

        e.set_property("render-enabled", true);
        assert!(!e.upcast_ref::<gst_base::BaseTransform>().is_passthrough());

        e.set_property("render-enabled", false);
        assert!(e.upcast_ref::<gst_base::BaseTransform>().is_passthrough());
    }

    #[test]
    fn object_detection_overlay_has_video_raw_pad_templates() {
        ensure_gstreamer_initialized();
        let e = glib::Object::builder::<objectdetectionoverlay::ObjectDetectionOverlay>().build();

        let sink = e.pad_template("sink").expect("missing sink pad template");
        let src = e.pad_template("src").expect("missing src pad template");

        assert_eq!(sink.caps().to_string(), "video/x-raw");
        assert_eq!(src.caps().to_string(), "video/x-raw");
    }

    #[test]
    fn segmentation_overlay_has_video_raw_pad_templates() {
        ensure_gstreamer_initialized();
        let e = glib::Object::builder::<segmentationoverlay::SegmentationOverlay>().build();

        let sink = e.pad_template("sink").expect("missing sink pad template");
        let src = e.pad_template("src").expect("missing src pad template");

        let expected_caps = VideoCapsBuilder::new().build().to_string();

        assert_eq!(sink.caps().to_string(), expected_caps);
        assert_eq!(src.caps().to_string(), expected_caps);
    }

    #[test]
    fn keypoints_overlay_has_video_raw_pad_templates() {
        ensure_gstreamer_initialized();
        let e = glib::Object::builder::<keypointsoverlay::KeypointsOverlay>().build();

        let sink = e.pad_template("sink").expect("missing sink pad template");
        let src = e.pad_template("src").expect("missing src pad template");

        assert_eq!(sink.caps().to_string(), "video/x-raw");
        assert_eq!(src.caps().to_string(), "video/x-raw");
    }
}
