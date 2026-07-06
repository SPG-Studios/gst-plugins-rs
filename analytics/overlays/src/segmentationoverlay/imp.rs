// Copyright (C) 2026 Jeremy Whiting <jeremy.whiting@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_analytics::{AnalyticsMetaRefExt, AnalyticsRelationMeta};

use gst_base::prelude::BaseTransformExt;
use gst_base::subclass::prelude::*;
use gst_video::prelude::VideoFrameExt;
use gst_video::subclass::prelude::*;

use crate::coordination::{ClaimedRegion, add_claimed_regions};
use crate::geometry::Rect;
use crate::lifecycle::{OverlayLifecycle, lifecycle_event_kind};

use super::masks::{
    AnalyticsSegmentationMtd, DEFAULT_HINT_MAXIMUM_SEGMENT_TYPE, DEFAULT_RENDER_ENABLED,
    OVERLAY_OWNER, SegmentationMtdExt, Settings, State, cached_mask_filter, color_for_segment,
    related_classification, render_mask_canvas, update_selected_type_cache,
};

use std::sync::{LazyLock, Mutex};

#[derive(Default)]
pub struct SegmentationOverlay {
    settings: Mutex<Settings>,
    state: Mutex<State>,
}

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "segoverlay",
        gst::DebugColorFlags::empty(),
        Some("Segmentation overlay"),
    )
});

static BLEND_CAPS: LazyLock<gst::Caps> =
    LazyLock::new(|| gst_video::VideoCapsBuilder::new().build());

fn can_handle_caps(in_caps: &gst::Caps) -> bool {
    in_caps.is_subset(&BLEND_CAPS)
}

fn decide_attach_mode(
    upstream_has_meta: bool,
    caps_has_meta: bool,
    alloc_has_meta: bool,
    can_handle: bool,
) -> Result<bool, ()> {
    let attach = if upstream_has_meta {
        true
    } else if caps_has_meta {
        if alloc_has_meta { true } else { !can_handle }
    } else {
        false
    };

    if !attach && !can_handle {
        Err(())
    } else {
        Ok(attach)
    }
}

impl SegmentationOverlay {
    fn reset_runtime_state(&self) {
        let mut state = self.state.lock().unwrap();
        state.reset();
    }

    fn negotiate_attach_mode(&self, in_caps: &gst::Caps) -> Result<(), gst::LoggableError> {
        let srcpad = self.obj().upcast_ref::<gst::Element>().static_pad("src");
        let Some(srcpad) = srcpad else {
            self.state.lock().unwrap().set_attach_composition(false);
            return Ok(());
        };

        let upstream_has_meta = in_caps
            .features(0)
            .map(|f| f.contains(gst_video::CAPS_FEATURE_META_GST_VIDEO_OVERLAY_COMPOSITION))
            .unwrap_or(false);

        let mut caps = in_caps.clone();
        let mut caps_has_meta = false;
        let mut alloc_has_meta = false;

        if !upstream_has_meta {
            let mut caps_clone = caps.clone();
            if let Some(features) = caps_clone.make_mut().features_mut(0) {
                let is_sysmem = features.is_empty()
                    || features
                        .iter()
                        .all(|feature| feature == gst::CAPS_FEATURE_MEMORY_SYSTEM_MEMORY);

                features.add(gst_video::CAPS_FEATURE_META_GST_VIDEO_OVERLAY_COMPOSITION);
                let peercaps = srcpad.peer_query_caps(Some(&caps_clone));
                caps_has_meta = !peercaps.is_empty();
                if caps_has_meta {
                    caps = caps_clone;
                } else if !is_sysmem {
                    return Err(gst::loggable_error!(
                        CAT,
                        "Provided input caps with features but downstream does not support meta::GstVideoOverlayComposition"
                    ));
                }
            }
        }

        if upstream_has_meta || caps_has_meta {
            let mut query = gst::query::Allocation::new(Some(&caps), false);
            if srcpad.peer_query(&mut query) {
                alloc_has_meta = query
                    .find_allocation_meta::<gst_video::VideoOverlayCompositionMeta>()
                    .is_some();
            } else if srcpad.pad_flags().contains(gst::PadFlags::FLUSHING) {
                srcpad.mark_reconfigure();
                return Err(gst::loggable_error!(
                    CAT,
                    "ALLOCATION query failed while pad is flushing"
                ));
            }
        }

        let can_handle = can_handle_caps(in_caps);
        let attach = match decide_attach_mode(
            upstream_has_meta,
            caps_has_meta,
            alloc_has_meta,
            can_handle,
        ) {
            Ok(attach) => attach,
            Err(()) => {
                srcpad.mark_reconfigure();
                return Err(gst::loggable_error!(
                    CAT,
                    "Unsupported caps for blending: {}",
                    in_caps
                ));
            }
        };

        gst::debug!(
            CAT,
            imp = self,
            "attach mode negotiated: upstream_has_meta={}, caps_has_meta={}, alloc_has_meta={}, can_handle={}, attach={}",
            upstream_has_meta,
            caps_has_meta,
            alloc_has_meta,
            can_handle,
            attach,
        );

        self.state.lock().unwrap().set_attach_composition(attach);
        Ok(())
    }
}

impl OverlayLifecycle for SegmentationOverlay {
    fn reset_runtime_state(&self) {
        SegmentationOverlay::reset_runtime_state(self);
    }
}

#[glib::object_subclass]
impl ObjectSubclass for SegmentationOverlay {
    // Distinct from the C element's "GstSegmentationOverlay" so both plugins can
    // be loaded in the same process (the factory name stays "segoverlay").
    const NAME: &'static str = "GstRsSegmentationOverlay";
    type Type = super::SegmentationOverlay;
    type ParentType = gst_video::VideoFilter;
}

impl ObjectImpl for SegmentationOverlay {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecBoolean::builder("render-enabled")
                    .nick("Render enabled")
                    .blurb("When false, element runs in passthrough mode")
                    .default_value(DEFAULT_RENDER_ENABLED)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecUInt::builder("hint-maximum-segment-type")
                    .nick("Hint maximum segment type")
                    .blurb("Hint for expected maximum segment type value")
                    .minimum(1)
                    .maximum(u32::MAX)
                    .default_value(DEFAULT_HINT_MAXIMUM_SEGMENT_TYPE)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecString::builder("selected-types")
                    .nick("Selected types")
                    .blurb("Semicolon-separated type names to render")
                    .default_value(None)
                    .mutable_playing()
                    .build(),
                crate::coordination::priority_param_spec(),
                crate::coordination::publish_claimed_regions_param_spec(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "render-enabled" => {
                let render_enabled = value.get().expect("type checked upstream");
                let mut settings = self.settings.lock().unwrap();
                settings.render_enabled = render_enabled;

                let passthrough = !render_enabled;
                self.obj()
                    .upcast_ref::<gst_base::BaseTransform>()
                    .set_passthrough(passthrough);

                gst::info!(
                    CAT,
                    imp = self,
                    "render-enabled set to {}, passthrough={}",
                    render_enabled,
                    passthrough
                );
            }
            "hint-maximum-segment-type" => {
                let mut settings = self.settings.lock().unwrap();
                settings.hint_maximum_segment_type = value.get().expect("type checked upstream");
            }
            "selected-types" => {
                let mut settings = self.settings.lock().unwrap();
                settings.selected_types = value.get().expect("type checked upstream");

                let mut state = self.state.lock().unwrap();
                update_selected_type_cache(&mut state, settings.selected_types.as_deref());
            }
            "priority" => {
                let mut settings = self.settings.lock().unwrap();
                settings.priority = value.get().expect("type checked upstream");
            }
            "publish-claimed-regions" => {
                let mut settings = self.settings.lock().unwrap();
                settings.publish_claimed_regions = value.get().expect("type checked upstream");
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "render-enabled" => {
                let settings = self.settings.lock().unwrap();
                settings.render_enabled.to_value()
            }
            "hint-maximum-segment-type" => {
                let settings = self.settings.lock().unwrap();
                settings.hint_maximum_segment_type.to_value()
            }
            "selected-types" => {
                let settings = self.settings.lock().unwrap();
                settings.selected_types.to_value()
            }
            "priority" => {
                let settings = self.settings.lock().unwrap();
                settings.priority.to_value()
            }
            "publish-claimed-regions" => {
                let settings = self.settings.lock().unwrap();
                settings.publish_claimed_regions.to_value()
            }
            _ => unimplemented!(),
        }
    }

    fn constructed(&self) {
        self.parent_constructed();
        self.obj()
            .upcast_ref::<gst_base::BaseTransform>()
            .set_passthrough(!DEFAULT_RENDER_ENABLED);
    }
}

impl GstObjectImpl for SegmentationOverlay {}

impl ElementImpl for SegmentationOverlay {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Segmentation Overlay",
                "Filter/Editor/Video",
                "Overlay a visual representation of segmentation metadata on the video",
                "Jeremy Whiting <jeremy.whiting@collabora.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let caps = BLEND_CAPS.clone();

            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            vec![src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl BaseTransformImpl for SegmentationOverlay {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::AlwaysInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = false;

    fn start(&self) -> Result<(), gst::ErrorMessage> {
        self.lifecycle_start()
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        self.lifecycle_stop()
    }

    fn sink_event(&self, event: gst::Event) -> bool {
        match lifecycle_event_kind(&event) {
            Some(_) => {
                self.reset_runtime_state();
                self.parent_sink_event(event)
            }
            None => self.parent_sink_event(event),
        }
    }
}

impl VideoFilterImpl for SegmentationOverlay {
    fn set_info(
        &self,
        in_caps: &gst::Caps,
        _in_info: &gst_video::VideoInfo,
        _out_caps: &gst::Caps,
        _out_info: &gst_video::VideoInfo,
    ) -> Result<(), gst::LoggableError> {
        self.negotiate_attach_mode(in_caps)
    }

    fn transform_frame_ip(
        &self,
        frame: &mut gst_video::VideoFrameRef<&mut gst::BufferRef>,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let settings = self.settings.lock().unwrap().clone();
        let mut state = self.state.lock().unwrap();

        update_selected_type_cache(&mut state, settings.selected_types.as_deref());

        let selected_types = state.selected_type_quarks();

        // Destination rects of the masks we draw, claimed below so downstream
        // overlays steer their labels around them.
        let mut claimed_rects: Vec<Rect> = Vec::new();
        if let Some(meta) = frame.buffer().meta::<AnalyticsRelationMeta>() {
            let mut composition = frame
                .buffer()
                .iter_meta::<gst_video::VideoOverlayCompositionMeta>()
                .next()
                .map(|overlay_meta| overlay_meta.overlay().to_owned());

            for seg_mtd in meta.iter::<AnalyticsSegmentationMtd>() {
                let Some((mask, mut ofx, mut ofy, mut canvas_w, mut canvas_h)) = seg_mtd.mask()
                else {
                    continue;
                };

                if canvas_w == 0 || canvas_h == 0 {
                    continue;
                }

                let frame_w = frame.width() as i32;
                let frame_h = frame.height() as i32;
                ofx = ofx.clamp(0, frame_w);
                ofy = ofy.clamp(0, frame_h);
                canvas_w = canvas_w.min((frame_w - ofx).max(0) as u32);
                canvas_h = canvas_h.min((frame_h - ofy).max(0) as u32);

                if canvas_w == 0 || canvas_h == 0 {
                    continue;
                }

                let cls_mtd = related_classification(&meta, &seg_mtd);
                let mask_filter =
                    cached_mask_filter(&mut state, cls_mtd.as_ref(), selected_types.as_deref());
                let Some(canvas) = render_mask_canvas(
                    mask,
                    canvas_w,
                    canvas_h,
                    mask_filter.as_deref(),
                    |segment_value| {
                        color_for_segment(
                            &mut state,
                            segment_value,
                            settings.hint_maximum_segment_type,
                        )
                    },
                ) else {
                    continue;
                };

                claimed_rects.push(Rect::from_xywh(ofx, ofy, canvas_w as i32, canvas_h as i32));

                let rect = gst_video::VideoOverlayRectangle::new_raw(
                    &canvas,
                    ofx,
                    ofy,
                    canvas_w,
                    canvas_h,
                    gst_video::VideoOverlayFormatFlags::PREMULTIPLIED_ALPHA,
                );

                if let Some(composition_ref) = composition.as_mut() {
                    if let Some(composition_mut) = composition_ref.get_mut() {
                        composition_mut.add_rectangle(&rect);
                    }
                } else {
                    composition = gst_video::VideoOverlayComposition::new(Some(&rect)).ok();
                }
            }

            state.set_composition(composition);
        }

        let composition = state.composition();
        let attach = state.attach_composition();
        drop(state);

        if let Some(composition) = composition {
            if attach {
                // SAFETY: The frame is writable and uniquely borrowed here.
                let buffer = unsafe { gst::BufferRef::from_mut_ptr((*frame.as_mut_ptr()).buffer) };
                gst_video::VideoOverlayCompositionMeta::add(buffer, &composition);
            } else {
                composition
                    .blend(frame)
                    .map_err(|_| gst::FlowError::Error)?;
            }
        }

        // Publish the mask regions we drew as soft Avoid claims, so a downstream
        // overlay's label placement prefers not to draw on top of them. Masks
        // are large and semi-transparent, hence Avoid rather than Occlude.
        if !claimed_rects.is_empty() && settings.publish_claimed_regions {
            // SAFETY: the frame is writable and uniquely borrowed here.
            let buffer = unsafe { gst::BufferRef::from_mut_ptr((*frame.as_mut_ptr()).buffer) };
            let regions: Vec<ClaimedRegion> = claimed_rects
                .iter()
                .map(|rect| ClaimedRegion::avoid(*rect, OVERLAY_OWNER, settings.priority))
                .collect();
            add_claimed_regions(buffer, &regions);
        }

        Ok(gst::FlowSuccess::Ok)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn can_handle_caps_accepts_video_raw_caps() {
        gst::init().unwrap();
        let caps = gst_video::VideoCapsBuilder::new()
            .format(gst_video::VideoFormat::I420)
            .build();
        assert!(can_handle_caps(&caps));
    }

    #[test]
    fn can_handle_caps_rejects_non_video_caps() {
        gst::init().unwrap();
        let caps = gst::Caps::builder("audio/x-raw").build();
        assert!(!can_handle_caps(&caps));
    }

    #[test]
    fn reset_runtime_state_clears_cached_overlay_state() {
        gst::init().unwrap();
        let overlay = SegmentationOverlay::default();

        {
            let mut state = overlay.state.lock().unwrap();
            state.test_set_segment_color(1, 0x00ff_0000);
            state.test_set_next_color_index(1);

            let mut overlay_buf = gst::Buffer::from_mut_slice(vec![0_u8; 4]);
            gst_video::VideoMeta::add(
                overlay_buf.get_mut().unwrap(),
                gst_video::VideoFrameFlags::empty(),
                gst_video::VideoFormat::Bgra,
                1,
                1,
            )
            .unwrap();

            let rect = gst_video::VideoOverlayRectangle::new_raw(
                &overlay_buf,
                0,
                0,
                1,
                1,
                gst_video::VideoOverlayFormatFlags::PREMULTIPLIED_ALPHA,
            );
            state.set_composition(gst_video::VideoOverlayComposition::new(Some(&rect)).ok());
            state.set_attach_composition(true);
        }

        overlay.reset_runtime_state();

        let state = overlay.state.lock().unwrap();
        assert!(state.test_segment_colors_is_empty());
        assert_eq!(state.test_next_color_index(), 0);
        assert!(state.composition().is_none());
        assert!(!state.attach_composition());
    }

    #[test]
    fn decide_attach_mode_upstream_meta_always_attaches() {
        let attach = decide_attach_mode(true, false, false, true).unwrap();
        assert!(attach);
    }

    #[test]
    fn decide_attach_mode_caps_and_alloc_meta_attaches() {
        let attach = decide_attach_mode(false, true, true, true).unwrap();
        assert!(attach);
    }

    #[test]
    fn decide_attach_mode_caps_meta_without_alloc_prefers_blend_if_possible() {
        let attach = decide_attach_mode(false, true, false, true).unwrap();
        assert!(!attach);
    }

    #[test]
    fn decide_attach_mode_caps_meta_without_alloc_forces_attach_when_cannot_blend() {
        let attach = decide_attach_mode(false, true, false, false).unwrap();
        assert!(attach);
    }

    #[test]
    fn decide_attach_mode_without_meta_and_without_blend_support_fails() {
        let result = decide_attach_mode(false, false, false, false);
        assert!(result.is_err());
    }

    #[test]
    fn decide_attach_mode_without_meta_but_with_blend_support_blends() {
        let attach = decide_attach_mode(false, false, false, true).unwrap();
        assert!(!attach);
    }
}
