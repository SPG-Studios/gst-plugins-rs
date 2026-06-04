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
use gst_analytics::{
    AnalyticsClassificationMtd, AnalyticsMetaRefExt, AnalyticsODMtd, AnalyticsRelationMeta,
    AnalyticsTrackingMtd, RelTypes,
};
use gst_video::prelude::VideoFrameExt;

use gst_base::prelude::{BaseTransformExt, BaseTransformExtManual};
use gst_base::subclass::prelude::*;
use gst_video::subclass::prelude::*;

use crate::geometry::{OccupiedRegionRegistry, Rect};
use crate::lifecycle::{LifecycleEventKind, OverlayLifecycle, lifecycle_event_kind};
use crate::render::{
    AnalyticsFrame, DrawCommand, LABEL_LAYOUT_GAP, LABEL_LAYOUT_HEIGHT, RenderContext,
    measure_label_text_width,
};

use std::sync::{LazyLock, Mutex};

const DEFAULT_RENDER_ENABLED: bool = false;
const DEFAULT_OBJECT_DETECTION_OUTLINE_COLOR: u32 = 0xFFFF_FFFF;
const DEFAULT_DRAW_LABELS: bool = true;
const DEFAULT_DRAW_TRACKING_LABELS: bool = true;
const DEFAULT_LABELS_COLOR: u32 = 0xFFFF_FFFF;
const DEFAULT_FILLED_BOX: bool = false;
const DEFAULT_EXPIRE_OVERLAY: u64 = 1_000_000_000;
const DEFAULT_TRACKING_OUTLINE_COLORS: bool = true;

#[derive(Debug, Clone, Copy)]
struct Settings {
    render_enabled: bool,
    object_detection_outline_color: u32,
    draw_labels: bool,
    draw_tracking_labels: bool,
    labels_color: u32,
    filled_box: bool,
    expire_overlay: u64,
    tracking_outline_colors: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            render_enabled: DEFAULT_RENDER_ENABLED,
            object_detection_outline_color: DEFAULT_OBJECT_DETECTION_OUTLINE_COLOR,
            draw_labels: DEFAULT_DRAW_LABELS,
            draw_tracking_labels: DEFAULT_DRAW_TRACKING_LABELS,
            labels_color: DEFAULT_LABELS_COLOR,
            filled_box: DEFAULT_FILLED_BOX,
            expire_overlay: DEFAULT_EXPIRE_OVERLAY,
            tracking_outline_colors: DEFAULT_TRACKING_OUTLINE_COLORS,
        }
    }
}

#[derive(Default)]
pub struct ObjectDetectionOverlay {
    render_context: Mutex<RenderContext>,
    settings: Mutex<Settings>,
    overlay_cache: Mutex<OverlayCache>,
    stream_state: Mutex<StreamState>,
    attach_composition: Mutex<bool>,
}

#[derive(Debug, Clone, Default)]
struct OverlayCache {
    analytics: AnalyticsFrame<'static>,
    commands: Vec<DrawCommand>,
    last_update_running_time: Option<gst::ClockTime>,
}

#[derive(Debug, Clone, Copy, Default)]
struct StreamState {
    flushing: bool,
    eos: bool,
}

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "odoverlay",
        gst::DebugColorFlags::empty(),
        Some("Object detection overlay skeleton"),
    )
});

static BLEND_CAPS: LazyLock<gst::Caps> =
    LazyLock::new(|| gst_video::VideoCapsBuilder::new().build());

fn label_text(mtd: &gst_analytics::AnalyticsMtdRef<'_, AnalyticsODMtd>) -> String {
    let label = mtd
        .obj_type()
        .map(|obj_type| obj_type.as_str().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    format!("{label} (c={:04.2})", mtd.confidence_level())
}

fn label_text_with_related(
    meta: &gst::MetaRef<'_, AnalyticsRelationMeta>,
    od_mtd: &gst_analytics::AnalyticsMtdRef<'_, AnalyticsODMtd>,
) -> String {
    let Some(cls_mtd) = meta
        .iter_direct_related::<AnalyticsClassificationMtd>(od_mtd.id(), RelTypes::RELATE_TO)
        .next()
    else {
        return label_text(od_mtd);
    };

    if cls_mtd.is_empty() {
        return label_text(od_mtd);
    }

    let cls_label = cls_mtd.quark(0).as_str().to_string();
    format!("{cls_label} (c={:04.2})", cls_mtd.level(0))
}

fn tracking_label_text(tracking_id: u64) -> String {
    format!("Track: {tracking_id}")
}

fn related_tracking_id(
    meta: &gst::MetaRef<'_, AnalyticsRelationMeta>,
    od_mtd: &gst_analytics::AnalyticsMtdRef<'_, AnalyticsODMtd>,
) -> Option<u64> {
    meta.iter_direct_related::<AnalyticsTrackingMtd>(od_mtd.id(), RelTypes::RELATE_TO)
        .next()
        .map(|tracking_mtd| tracking_mtd.info().0)
}

fn generate_track_color_hsv(track_id: u64) -> u32 {
    let mut h = 0.0_f32;
    let mut increment = 0.5_f32;
    const SATURATION: f32 = 0.85;
    const VALUE: f32 = 0.95;

    let mut id = track_id + 1;
    while id > 1 {
        if id & 1 == 1 {
            h += increment;
        }
        id >>= 1;
        increment *= 0.5;
    }

    while h >= 1.0 {
        h -= 1.0;
    }

    let hi = (h * 6.0) as i32;
    let f = h * 6.0 - hi as f32;
    let p = VALUE * (1.0 - SATURATION);
    let q = VALUE * (1.0 - f * SATURATION);
    let t = VALUE * (1.0 - (1.0 - f) * SATURATION);

    let (r, g, b) = match hi.rem_euclid(6) {
        0 => (VALUE, t, p),
        1 => (q, VALUE, p),
        2 => (p, VALUE, t),
        3 => (p, q, VALUE),
        4 => (t, p, VALUE),
        5 => (VALUE, p, q),
        _ => (0.0, 0.0, 0.0),
    };

    let r8 = (r * 255.0) as u32;
    let g8 = (g * 255.0) as u32;
    let b8 = (b * 255.0) as u32;

    (0xFF << 24) | (r8 << 16) | (g8 << 8) | b8
}

#[derive(Debug, Clone, Copy)]
struct FrameBounds {
    width: i32,
    height: i32,
}

#[derive(Debug, Clone, Copy)]
struct BBox {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

fn estimate_label_rect(anchor_x: i32, anchor_y: i32, text: &str) -> Rect {
    let text_width = measure_label_text_width(text);
    Rect::from_xywh(
        anchor_x,
        anchor_y.saturating_sub(LABEL_LAYOUT_HEIGHT),
        text_width,
        LABEL_LAYOUT_HEIGHT,
    )
}

fn reserve_label_anchor(
    registry: &mut OccupiedRegionRegistry,
    bbox: BBox,
    text: &str,
    preferred_x: i32,
    preferred_y: i32,
) -> Option<(i32, i32)> {
    let candidates = [
        (preferred_x, preferred_y),
        (
            bbox.x,
            bbox.y
                .saturating_sub(LABEL_LAYOUT_GAP)
                .max(LABEL_LAYOUT_HEIGHT),
        ),
        (
            bbox.x,
            bbox.y
                .saturating_add(bbox.h)
                .saturating_add(LABEL_LAYOUT_HEIGHT)
                .saturating_add(LABEL_LAYOUT_GAP),
        ),
        (
            bbox.x
                .saturating_add(bbox.w)
                .saturating_add(LABEL_LAYOUT_GAP),
            preferred_y,
        ),
    ];

    for (x, y) in candidates {
        if registry.reserve_label(estimate_label_rect(x, y, text)) {
            return Some((x, y));
        }
    }

    None
}

fn analytics_to_draw_commands(
    buffer: &gst::BufferRef,
    settings: Settings,
    bounds: FrameBounds,
) -> (AnalyticsFrame<'static>, Vec<DrawCommand>) {
    let Some(meta) = buffer.meta::<AnalyticsRelationMeta>() else {
        return (AnalyticsFrame::default(), Vec::new());
    };

    let mut commands = Vec::new();
    let mut object_count = 0;
    let mut occupied = OccupiedRegionRegistry::new(bounds.width, bounds.height);

    for od_mtd in meta.iter::<AnalyticsODMtd>() {
        let Ok(location) = od_mtd.location() else {
            continue;
        };

        let (bbox_x, bbox_y, bbox_w, bbox_h, bbox_rotation) = od_mtd
            .oriented_location()
            .map(|oriented| (oriented.x, oriented.y, oriented.w, oriented.h, oriented.r))
            .unwrap_or((location.x, location.y, location.w, location.h, 0.0));

        if bbox_w <= 0 || bbox_h <= 0 {
            continue;
        }

        let tracking_id = related_tracking_id(&meta, &od_mtd);
        let outline_color = if settings.tracking_outline_colors {
            tracking_id
                .map(|id| generate_track_color_hsv(id & 0x0FFF_FFFF))
                .unwrap_or(settings.object_detection_outline_color)
        } else {
            settings.object_detection_outline_color
        };

        object_count += 1;

        let bbox = BBox {
            x: bbox_x,
            y: bbox_y,
            w: bbox_w,
            h: bbox_h,
        };

        occupied.reserve_highlight(Rect::from_xywh(bbox.x, bbox.y, bbox.w, bbox.h));

        commands.push(DrawCommand::Rectangle {
            x: bbox_x as f32,
            y: bbox_y as f32,
            width: bbox_w as f32,
            height: bbox_h as f32,
            rotation: bbox_rotation,
            argb: outline_color,
            filled: settings.filled_box,
        });

        if settings.draw_labels {
            let label = label_text_with_related(&meta, &od_mtd);
            if let Some((label_x, label_y)) =
                reserve_label_anchor(&mut occupied, bbox, &label, location.x, location.y)
            {
                commands.push(DrawCommand::Text {
                    x: label_x as f32,
                    y: label_y as f32,
                    text: label,
                    argb: settings.labels_color,
                });
            }
        }

        if settings.draw_tracking_labels
            && let Some(tracking_id) = tracking_id
        {
            let tracking_text = tracking_label_text(tracking_id);
            if let Some((label_x, label_y)) = reserve_label_anchor(
                &mut occupied,
                bbox,
                &tracking_text,
                location.x,
                location.y.saturating_add(location.h),
            ) {
                commands.push(DrawCommand::Text {
                    x: label_x as f32,
                    y: label_y as f32,
                    text: tracking_text,
                    argb: settings.labels_color,
                });
            }
        }
    }

    (
        AnalyticsFrame {
            object_count,
            ..Default::default()
        },
        commands,
    )
}

fn should_reuse_cached_overlay(
    running_time: Option<gst::ClockTime>,
    last_update_running_time: Option<gst::ClockTime>,
    expire_overlay: u64,
) -> bool {
    let Some(last_update_running_time) = last_update_running_time else {
        return false;
    };

    // Match C semantics: MAX (GST_CLOCK_TIME_NONE) means never expire.
    if expire_overlay == u64::MAX {
        return true;
    }

    // Match C semantics: if running time is not available, keep last overlay.
    let Some(running_time) = running_time else {
        return true;
    };

    let elapsed = running_time
        .nseconds()
        .saturating_sub(last_update_running_time.nseconds());
    elapsed < expire_overlay
}

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

#[cfg(test)]
fn test_frame_bounds() -> FrameBounds {
    FrameBounds {
        width: 192,
        height: 192,
    }
}

#[glib::object_subclass]
impl ObjectSubclass for ObjectDetectionOverlay {
    const NAME: &'static str = "GstObjectDetectionOverlay";
    type Type = super::ObjectDetectionOverlay;
    type ParentType = gst_video::VideoFilter;
}

impl ObjectDetectionOverlay {
    fn reset_stream_state(&self) {
        *self.stream_state.lock().unwrap() = StreamState::default();
    }

    fn clear_overlay_cache(&self) {
        let mut cache = self.overlay_cache.lock().unwrap();
        cache.analytics = AnalyticsFrame::default();
        cache.commands.clear();
        cache.last_update_running_time = None;
    }

    fn buffer_running_time(&self, buffer: &gst::BufferRef) -> Option<gst::ClockTime> {
        let pts = buffer.pts()?;
        let segment = self
            .obj()
            .upcast_ref::<gst_base::BaseTransform>()
            .segment()
            .downcast::<gst::ClockTime>()
            .ok()?;
        segment.to_running_time(pts)
    }

    fn negotiate_attach_mode(&self, in_caps: &gst::Caps) -> Result<(), gst::LoggableError> {
        let srcpad = self.obj().upcast_ref::<gst::Element>().static_pad("src");
        let Some(srcpad) = srcpad else {
            *self.attach_composition.lock().unwrap() = false;
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

        *self.attach_composition.lock().unwrap() = attach;
        Ok(())
    }

    fn build_overlay_composition(
        &self,
        width: u32,
        height: u32,
        analytics: &AnalyticsFrame,
        commands: &[DrawCommand],
    ) -> Option<gst_video::VideoOverlayComposition> {
        if commands.is_empty() {
            return None;
        }

        let format = if cfg!(target_endian = "little") {
            gst_video::VideoFormat::Bgra
        } else {
            gst_video::VideoFormat::Argb
        };

        let mut buffer = gst::Buffer::with_size(width as usize * height as usize * 4).ok()?;
        gst_video::VideoMeta::add(
            buffer.get_mut().unwrap(),
            gst_video::VideoFrameFlags::empty(),
            format,
            width,
            height,
        )
        .ok()?;

        let info = gst_video::VideoInfo::builder(format, width, height)
            .build()
            .ok()?;

        {
            let mut frame =
                gst_video::VideoFrameRef::from_buffer_ref_writable(buffer.make_mut(), &info)
                    .ok()?;
            self.render_context
                .lock()
                .unwrap()
                .render(&mut frame, analytics, commands)
                .ok()?;
        }

        let rect = gst_video::VideoOverlayRectangle::new_raw(
            &buffer,
            0,
            0,
            width,
            height,
            gst_video::VideoOverlayFormatFlags::PREMULTIPLIED_ALPHA,
        );
        gst_video::VideoOverlayComposition::new(Some(&rect)).ok()
    }
}

impl OverlayLifecycle for ObjectDetectionOverlay {
    fn reset_runtime_state(&self) {
        self.reset_stream_state();
        self.clear_overlay_cache();
    }
}

impl ObjectImpl for ObjectDetectionOverlay {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecBoolean::builder("render-enabled")
                    .nick("Render enabled")
                    .blurb("When false, element runs in passthrough mode")
                    .default_value(DEFAULT_RENDER_ENABLED)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecUInt::builder("object-detection-outline-color")
                    .nick("Object detection outline color")
                    .blurb("Outline color for object detection boxes")
                    .minimum(0)
                    .maximum(u32::MAX)
                    .default_value(DEFAULT_OBJECT_DETECTION_OUTLINE_COLOR)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecBoolean::builder("draw-labels")
                    .nick("Draw labels")
                    .blurb("Draw class and confidence labels")
                    .default_value(DEFAULT_DRAW_LABELS)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecBoolean::builder("draw-tracking-labels")
                    .nick("Draw tracking labels")
                    .blurb("Draw tracking labels when tracking metadata exists")
                    .default_value(DEFAULT_DRAW_TRACKING_LABELS)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecUInt::builder("labels-color")
                    .nick("Labels color")
                    .blurb("Text labels color")
                    .minimum(0)
                    .maximum(u32::MAX)
                    .default_value(DEFAULT_LABELS_COLOR)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecBoolean::builder("filled-box")
                    .nick("Filled box")
                    .blurb("Fill object detection boxes instead of drawing outlines only")
                    .default_value(DEFAULT_FILLED_BOX)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecUInt64::builder("expire-overlay")
                    .nick("Expire overlay")
                    .blurb("Duration in nanoseconds to keep last overlay when metadata is missing")
                    .minimum(0)
                    .maximum(u64::MAX)
                    .default_value(DEFAULT_EXPIRE_OVERLAY)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecBoolean::builder("tracking-outline-colors")
                    .nick("Tracking outline colors")
                    .blurb("Use tracking-based dynamic outline colors")
                    .default_value(DEFAULT_TRACKING_OUTLINE_COLORS)
                    .mutable_playing()
                    .build(),
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
            "object-detection-outline-color" => {
                let mut settings = self.settings.lock().unwrap();
                settings.object_detection_outline_color =
                    value.get().expect("type checked upstream");
            }
            "draw-labels" => {
                let mut settings = self.settings.lock().unwrap();
                settings.draw_labels = value.get().expect("type checked upstream");
            }
            "draw-tracking-labels" => {
                let mut settings = self.settings.lock().unwrap();
                settings.draw_tracking_labels = value.get().expect("type checked upstream");
            }
            "labels-color" => {
                let mut settings = self.settings.lock().unwrap();
                settings.labels_color = value.get().expect("type checked upstream");
            }
            "filled-box" => {
                let mut settings = self.settings.lock().unwrap();
                settings.filled_box = value.get().expect("type checked upstream");
            }
            "expire-overlay" => {
                let mut settings = self.settings.lock().unwrap();
                settings.expire_overlay = value.get().expect("type checked upstream");
            }
            "tracking-outline-colors" => {
                let mut settings = self.settings.lock().unwrap();
                settings.tracking_outline_colors = value.get().expect("type checked upstream");
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
            "object-detection-outline-color" => {
                let settings = self.settings.lock().unwrap();
                settings.object_detection_outline_color.to_value()
            }
            "draw-labels" => {
                let settings = self.settings.lock().unwrap();
                settings.draw_labels.to_value()
            }
            "draw-tracking-labels" => {
                let settings = self.settings.lock().unwrap();
                settings.draw_tracking_labels.to_value()
            }
            "labels-color" => {
                let settings = self.settings.lock().unwrap();
                settings.labels_color.to_value()
            }
            "filled-box" => {
                let settings = self.settings.lock().unwrap();
                settings.filled_box.to_value()
            }
            "expire-overlay" => {
                let settings = self.settings.lock().unwrap();
                settings.expire_overlay.to_value()
            }
            "tracking-outline-colors" => {
                let settings = self.settings.lock().unwrap();
                settings.tracking_outline_colors.to_value()
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

impl GstObjectImpl for ObjectDetectionOverlay {}

impl ElementImpl for ObjectDetectionOverlay {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Object Detection Overlay (skeleton)",
                "Filter/Editor/Video",
                "Object detection overlay skeleton with passthrough toggle",
                "Jeremy Whiting <jeremy.whiting@collabora.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let caps = gst::Caps::builder("video/x-raw").build();

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

impl BaseTransformImpl for ObjectDetectionOverlay {
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
            Some(LifecycleEventKind::Eos) => {
                gst::info!(CAT, imp = self, "EOS");
                self.stream_state.lock().unwrap().eos = true;
                self.parent_sink_event(event)
            }
            Some(LifecycleEventKind::FlushStart) => {
                gst::info!(CAT, imp = self, "Flush start");
                self.stream_state.lock().unwrap().flushing = true;
                self.parent_sink_event(event)
            }
            Some(LifecycleEventKind::FlushStop) => {
                gst::info!(CAT, imp = self, "Flush stop");
                let mut stream_state = self.stream_state.lock().unwrap();
                stream_state.eos = false;
                stream_state.flushing = false;
                drop(stream_state);
                self.parent_sink_event(event)
            }
            None => self.parent_sink_event(event),
        }
    }
}

impl VideoFilterImpl for ObjectDetectionOverlay {
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
        let stream_state = *self.stream_state.lock().unwrap();
        if stream_state.eos || stream_state.flushing {
            return Err(gst::FlowError::Eos);
        }

        let settings = *self.settings.lock().unwrap();
        let buffer = frame.buffer();
        let running_time = self.buffer_running_time(buffer);
        let has_meta = buffer.meta::<AnalyticsRelationMeta>().is_some();

        let (analytics, commands) = if has_meta {
            let (analytics, commands) = analytics_to_draw_commands(
                buffer,
                settings,
                FrameBounds {
                    width: frame.width() as i32,
                    height: frame.height() as i32,
                },
            );
            let mut cache = self.overlay_cache.lock().unwrap();
            cache.analytics = analytics.clone();
            cache.commands = commands.clone();
            cache.last_update_running_time = running_time;
            (analytics, commands)
        } else {
            let mut cache = self.overlay_cache.lock().unwrap();
            if should_reuse_cached_overlay(
                running_time,
                cache.last_update_running_time,
                settings.expire_overlay,
            ) {
                (cache.analytics.clone(), cache.commands.clone())
            } else {
                cache.analytics = AnalyticsFrame::default();
                cache.commands.clear();
                cache.last_update_running_time = None;
                (AnalyticsFrame::default(), Vec::new())
            }
        };

        let attach = *self.attach_composition.lock().unwrap();

        if attach {
            if let Some(composition) =
                self.build_overlay_composition(frame.width(), frame.height(), &analytics, &commands)
            {
                // SAFETY: The frame is writable and uniquely borrowed here.
                let buffer = unsafe { gst::BufferRef::from_mut_ptr((*frame.as_mut_ptr()).buffer) };
                gst_video::VideoOverlayCompositionMeta::add(buffer, &composition);
            }
        } else if let Some(composition) =
            self.build_overlay_composition(frame.width(), frame.height(), &analytics, &commands)
        {
            composition
                .blend(frame)
                .map_err(|_| gst::FlowError::Error)?;
        }

        Ok(gst::FlowSuccess::Ok)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gst_analytics::{
        AnalyticsRelationMetaClassificationExt, AnalyticsRelationMetaODExt,
        AnalyticsRelationMetaTrackingExt,
    };

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

    #[test]
    fn stream_state_defaults_to_not_flushing_or_eos() {
        let state = StreamState::default();

        assert!(!state.flushing);
        assert!(!state.eos);
    }

    #[test]
    fn clear_overlay_cache_resets_cached_commands() {
        let overlay = ObjectDetectionOverlay::default();
        {
            let mut cache = overlay.overlay_cache.lock().unwrap();
            cache.analytics.object_count = 1;
            cache.commands.push(DrawCommand::NoOp);
            cache.last_update_running_time = Some(gst::ClockTime::from_seconds(1));
        }

        overlay.clear_overlay_cache();

        let cache = overlay.overlay_cache.lock().unwrap();
        assert_eq!(cache.analytics.object_count, 0);
        assert!(cache.commands.is_empty());
        assert_eq!(cache.last_update_running_time, None);
    }

    #[test]
    fn reset_stream_state_clears_flags() {
        let overlay = ObjectDetectionOverlay::default();
        {
            let mut state = overlay.stream_state.lock().unwrap();
            state.flushing = true;
            state.eos = true;
        }

        overlay.reset_stream_state();

        let state = overlay.stream_state.lock().unwrap();
        assert!(!state.flushing);
        assert!(!state.eos);
    }

    #[test]
    fn analytics_metadata_becomes_rectangle_and_label_commands() {
        gst::init().unwrap();

        let mut buffer = gst::Buffer::new();
        {
            let mut relation = AnalyticsRelationMeta::add(buffer.make_mut());
            relation
                .add_od_mtd(glib::Quark::from_str("person"), 12, 24, 48, 64, 0.85)
                .unwrap();
        }

        let settings = Settings {
            render_enabled: true,
            object_detection_outline_color: 0xFF00_FF00,
            draw_labels: true,
            draw_tracking_labels: true,
            labels_color: 0xFFFF_FFFF,
            filled_box: false,
            expire_overlay: DEFAULT_EXPIRE_OVERLAY,
            tracking_outline_colors: DEFAULT_TRACKING_OUTLINE_COLORS,
        };

        let (analytics, commands) =
            analytics_to_draw_commands(buffer.as_ref(), settings, test_frame_bounds());

        assert_eq!(analytics.object_count, 1);
        assert_eq!(commands.len(), 2);
        assert_eq!(
            commands[0],
            DrawCommand::Rectangle {
                x: 12.0,
                y: 24.0,
                width: 48.0,
                height: 64.0,
                rotation: 0.0,
                argb: 0xFF00_FF00,
                filled: false,
            }
        );

        match &commands[1] {
            DrawCommand::Text { x, y, text, argb } => {
                assert_eq!((*x, *y, *argb), (12.0, 24.0, 0xFFFF_FFFF));
                assert_eq!(text, "person (c=0.85)");
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn related_classification_and_tracking_metadata_override_text_content() {
        gst::init().unwrap();

        let mut buffer = gst::Buffer::new();
        {
            let mut relation = AnalyticsRelationMeta::add(buffer.make_mut());
            let od_id = relation
                .add_od_mtd(glib::Quark::from_str("person"), 12, 24, 48, 64, 0.85)
                .unwrap()
                .id();
            let cls_id = relation
                .add_one_cls_mtd(0.42, glib::Quark::from_str("bus"))
                .unwrap()
                .id();
            relation
                .set_relation(RelTypes::RELATE_TO, od_id, cls_id)
                .unwrap();

            let tracking_id = relation
                .add_tracking_mtd(17, gst::ClockTime::from_seconds(1))
                .unwrap()
                .id();
            relation
                .set_relation(RelTypes::RELATE_TO, od_id, tracking_id)
                .unwrap();
        }

        let settings = Settings {
            render_enabled: true,
            object_detection_outline_color: 0xFF00_FF00,
            draw_labels: true,
            draw_tracking_labels: true,
            labels_color: 0xFFFF_FFFF,
            filled_box: false,
            expire_overlay: DEFAULT_EXPIRE_OVERLAY,
            tracking_outline_colors: DEFAULT_TRACKING_OUTLINE_COLORS,
        };

        let (analytics, commands) =
            analytics_to_draw_commands(buffer.as_ref(), settings, test_frame_bounds());

        assert_eq!(analytics.object_count, 1);
        assert_eq!(commands.len(), 3);

        match &commands[1] {
            DrawCommand::Text { text, .. } => assert_eq!(text, "bus (c=0.42)"),
            other => panic!("unexpected command: {other:?}"),
        }

        match &commands[2] {
            DrawCommand::Text { text, .. } => assert_eq!(text, "Track: 17"),
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn tracking_outline_colors_uses_track_based_color() {
        gst::init().unwrap();

        let mut buffer = gst::Buffer::new();
        {
            let mut relation = AnalyticsRelationMeta::add(buffer.make_mut());
            let od_id = relation
                .add_od_mtd(glib::Quark::from_str("person"), 12, 24, 48, 64, 0.85)
                .unwrap()
                .id();
            let tracking_id = relation
                .add_tracking_mtd(17, gst::ClockTime::from_seconds(1))
                .unwrap()
                .id();
            relation
                .set_relation(RelTypes::RELATE_TO, od_id, tracking_id)
                .unwrap();
        }

        let settings = Settings {
            render_enabled: true,
            object_detection_outline_color: 0xFF00_FF00,
            draw_labels: false,
            draw_tracking_labels: false,
            labels_color: 0xFFFF_FFFF,
            filled_box: false,
            expire_overlay: DEFAULT_EXPIRE_OVERLAY,
            tracking_outline_colors: true,
        };

        let (_, commands) =
            analytics_to_draw_commands(buffer.as_ref(), settings, test_frame_bounds());

        match &commands[0] {
            DrawCommand::Rectangle { argb, .. } => {
                assert_eq!(*argb, generate_track_color_hsv(17));
                assert_ne!(*argb, 0xFF00_FF00);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn oriented_metadata_becomes_rotated_rectangle_command() {
        gst::init().unwrap();

        let mut buffer = gst::Buffer::new();
        {
            let mut relation = AnalyticsRelationMeta::add(buffer.make_mut());
            relation
                .add_oriented_od_mtd(glib::Quark::from_str("hand"), 20, 30, 40, 50, 0.37, 0.9)
                .unwrap();
        }

        let settings = Settings {
            render_enabled: true,
            object_detection_outline_color: 0xFF00_FF00,
            draw_labels: false,
            draw_tracking_labels: false,
            labels_color: 0xFFFF_FFFF,
            filled_box: false,
            expire_overlay: DEFAULT_EXPIRE_OVERLAY,
            tracking_outline_colors: DEFAULT_TRACKING_OUTLINE_COLORS,
        };

        let (_, commands) =
            analytics_to_draw_commands(buffer.as_ref(), settings, test_frame_bounds());

        match &commands[0] {
            DrawCommand::Rectangle { rotation, .. } => {
                assert!((*rotation - 0.37).abs() < f32::EPSILON);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn analytics_metadata_without_relation_meta_produces_no_commands() {
        gst::init().unwrap();

        let buffer = gst::Buffer::new();
        let (analytics, commands) =
            analytics_to_draw_commands(buffer.as_ref(), Settings::default(), test_frame_bounds());

        assert_eq!(analytics.object_count, 0);
        assert!(commands.is_empty());
    }

    #[test]
    fn expire_overlay_none_never_expires_cached_overlay() {
        assert!(should_reuse_cached_overlay(
            Some(gst::ClockTime::from_seconds(100)),
            Some(gst::ClockTime::from_seconds(1)),
            u64::MAX,
        ));
    }

    #[test]
    fn expire_overlay_reuses_cached_overlay_within_window() {
        assert!(should_reuse_cached_overlay(
            Some(gst::ClockTime::from_seconds(5)),
            Some(gst::ClockTime::from_seconds(4)),
            gst::ClockTime::from_seconds(2).nseconds(),
        ));
    }

    #[test]
    fn expire_overlay_drops_cached_overlay_after_window() {
        assert!(!should_reuse_cached_overlay(
            Some(gst::ClockTime::from_seconds(8)),
            Some(gst::ClockTime::from_seconds(4)),
            gst::ClockTime::from_seconds(2).nseconds(),
        ));
    }

    #[test]
    fn tracking_outline_colors_disabled_uses_static_outline_color() {
        gst::init().unwrap();

        let mut buffer = gst::Buffer::new();
        {
            let mut relation = AnalyticsRelationMeta::add(buffer.make_mut());
            let od_id = relation
                .add_od_mtd(glib::Quark::from_str("person"), 12, 24, 48, 64, 0.85)
                .unwrap()
                .id();
            let tracking_id = relation
                .add_tracking_mtd(17, gst::ClockTime::from_seconds(1))
                .unwrap()
                .id();
            relation
                .set_relation(RelTypes::RELATE_TO, od_id, tracking_id)
                .unwrap();
        }

        let settings = Settings {
            render_enabled: true,
            object_detection_outline_color: 0xFF12_3456,
            draw_labels: false,
            draw_tracking_labels: false,
            labels_color: 0xFFFF_FFFF,
            filled_box: false,
            expire_overlay: DEFAULT_EXPIRE_OVERLAY,
            tracking_outline_colors: false,
        };

        let (_, commands) =
            analytics_to_draw_commands(buffer.as_ref(), settings, test_frame_bounds());

        match &commands[0] {
            DrawCommand::Rectangle { argb, .. } => assert_eq!(*argb, 0xFF12_3456),
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn draw_labels_disabled_suppresses_class_label_text() {
        gst::init().unwrap();

        let mut buffer = gst::Buffer::new();
        {
            let mut relation = AnalyticsRelationMeta::add(buffer.make_mut());
            relation
                .add_od_mtd(glib::Quark::from_str("person"), 12, 24, 48, 64, 0.85)
                .unwrap();
        }

        let settings = Settings {
            draw_labels: false,
            draw_tracking_labels: false,
            ..Settings::default()
        };

        let (_, commands) =
            analytics_to_draw_commands(buffer.as_ref(), settings, test_frame_bounds());

        assert_eq!(commands.len(), 1);
        assert!(matches!(commands[0], DrawCommand::Rectangle { .. }));
    }

    #[test]
    fn draw_tracking_labels_disabled_suppresses_tracking_text() {
        gst::init().unwrap();

        let mut buffer = gst::Buffer::new();
        {
            let mut relation = AnalyticsRelationMeta::add(buffer.make_mut());
            let od_id = relation
                .add_od_mtd(glib::Quark::from_str("person"), 12, 24, 48, 64, 0.85)
                .unwrap()
                .id();
            let tracking_id = relation
                .add_tracking_mtd(17, gst::ClockTime::from_seconds(1))
                .unwrap()
                .id();
            relation
                .set_relation(RelTypes::RELATE_TO, od_id, tracking_id)
                .unwrap();
        }

        let settings = Settings {
            draw_labels: true,
            draw_tracking_labels: false,
            labels_color: 0xFFAB_CDEF,
            ..Settings::default()
        };

        let (_, commands) =
            analytics_to_draw_commands(buffer.as_ref(), settings, test_frame_bounds());

        assert_eq!(commands.len(), 2);
        match &commands[1] {
            DrawCommand::Text { text, argb, .. } => {
                assert_eq!(text, "person (c=0.85)");
                assert_eq!(*argb, 0xFFAB_CDEF);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn filled_box_setting_propagates_to_rectangle_command() {
        gst::init().unwrap();

        let mut buffer = gst::Buffer::new();
        {
            let mut relation = AnalyticsRelationMeta::add(buffer.make_mut());
            relation
                .add_od_mtd(glib::Quark::from_str("person"), 12, 24, 48, 64, 0.85)
                .unwrap();
        }

        let settings = Settings {
            draw_labels: false,
            draw_tracking_labels: false,
            filled_box: true,
            ..Settings::default()
        };

        let (_, commands) =
            analytics_to_draw_commands(buffer.as_ref(), settings, test_frame_bounds());

        match &commands[0] {
            DrawCommand::Rectangle { filled, .. } => assert!(*filled),
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn reserve_label_anchor_falls_back_when_preferred_spot_is_occupied() {
        let mut occupied = OccupiedRegionRegistry::new(128, 128);
        let bbox = BBox {
            x: 20,
            y: 20,
            w: 30,
            h: 20,
        };

        occupied.reserve_highlight(Rect::from_xywh(20, 8, 60, 20));

        let anchor = reserve_label_anchor(&mut occupied, bbox, "person (c=0.90)", 20, 20)
            .expect("expected fallback anchor");

        assert_ne!(anchor, (20, 20));
    }
}
