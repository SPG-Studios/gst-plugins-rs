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

use crate::color::generate_track_color_argb;
use crate::geometry::{OccupiedRegionRegistry, Rect};
use crate::lifecycle::{LifecycleEventKind, OverlayLifecycle, lifecycle_event_kind};
use crate::placement::{LabelPlacement, leader_endpoints, place_label, push_leader_line};
use crate::render::{
    AnalyticsFrame, DrawCommand, LABEL_LAYOUT_GAP, LABEL_LAYOUT_HEIGHT, RenderContext,
    measure_label_text_width,
};

use std::sync::{LazyLock, Mutex};

/// Owner tag this element uses when claiming/reading shared regions.
const OVERLAY_OWNER: &str = "odoverlay";

const DEFAULT_RENDER_ENABLED: bool = false;
const DEFAULT_OBJECT_DETECTION_OUTLINE_COLOR: u32 = 0xFFFF_FFFF;
const DEFAULT_DRAW_LABELS: bool = true;
const DEFAULT_DRAW_TRACKING_LABELS: bool = true;
const DEFAULT_LABELS_COLOR: u32 = 0xFFFF_FFFF;
const DEFAULT_FILLED_BOX: bool = false;
const DEFAULT_EXPIRE_OVERLAY: u64 = 1_000_000_000;
const DEFAULT_TRACKING_OUTLINE_COLORS: bool = true;
const DEFAULT_SUPPRESS_BUILTIN_RENDERING: bool = false;
// Color generation constants for track coloring (HSV space)
const TRACK_COLOR_SATURATION: f32 = 0.85;
const TRACK_COLOR_VALUE: f32 = 0.95;

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
    suppress_builtin_rendering: bool,
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
            suppress_builtin_rendering: DEFAULT_SUPPRESS_BUILTIN_RENDERING,
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
    draw_hooks: Mutex<crate::hooks::DrawHooks>,
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

/// Offset of the far (extended) ring of label candidates from the box, added on
/// top of [`LABEL_LAYOUT_GAP`].
const LABEL_CANDIDATE_EXT: i32 = 12;

fn estimate_label_rect(anchor_x: i32, anchor_y: i32, text: &str) -> Rect {
    let text_width = measure_label_text_width(text);
    Rect::from_xywh(
        anchor_x,
        anchor_y.saturating_sub(LABEL_LAYOUT_HEIGHT),
        text_width,
        LABEL_LAYOUT_HEIGHT,
    )
}

/// Fallback label regions around a detection box, ordered near ring then far
/// ring (above, below, right, left in each). Object labels are drawn at the
/// bottom-left of their rectangle, so the baseline anchor `y` maps to the
/// rectangle bottom.
fn box_label_candidates(bbox: BBox, label_w: i32) -> Vec<Rect> {
    let h = LABEL_LAYOUT_HEIGHT;
    let rect_at =
        |x: i32, baseline_y: i32| Rect::from_xywh(x, baseline_y.saturating_sub(h), label_w, h);

    let right_x = bbox.x.saturating_add(bbox.w);
    let left_x = bbox.x.saturating_sub(label_w);
    // Side labels sit one line below the box top so they don't overhang it.
    let side_y = bbox.y.saturating_add(h);
    let box_bottom = bbox.y.saturating_add(bbox.h);

    [
        LABEL_LAYOUT_GAP,
        LABEL_LAYOUT_GAP.saturating_add(LABEL_CANDIDATE_EXT),
    ]
    .into_iter()
    .flat_map(|off| {
        [
            rect_at(bbox.x, bbox.y.saturating_sub(off).max(h)), // above
            rect_at(bbox.x, box_bottom.saturating_add(h).saturating_add(off)), // below
            rect_at(right_x.saturating_add(off), side_y),       // right
            rect_at(left_x.saturating_sub(off), side_y),        // left
        ]
    })
    .collect()
}

fn place_od_label(
    registry: &mut OccupiedRegionRegistry,
    bbox: BBox,
    text: &str,
    preferred_x: i32,
    preferred_y: i32,
) -> Option<LabelPlacement> {
    let default = estimate_label_rect(preferred_x, preferred_y, text);
    let candidates = box_label_candidates(bbox, measure_label_text_width(text));

    place_label(registry, default, &candidates)
}

/// Draw an object label at its placed position, with a leader line between the
/// box edge and the label edge when the label was displaced.
fn push_od_label(
    commands: &mut Vec<DrawCommand>,
    placement: LabelPlacement,
    text: String,
    box_rect: Rect,
    argb: u32,
) {
    if placement.displaced {
        let (from, to) = leader_endpoints(box_rect, placement.rect);
        push_leader_line(commands, from, to, argb);
    }

    commands.push(DrawCommand::Text {
        x: placement.rect.left as f32,
        y: placement.rect.bottom as f32,
        text,
        argb,
    });
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

    // Avoid regions other elements upstream have already claimed.
    crate::coordination::seed_registry_from_claims(&mut occupied, buffer, OVERLAY_OWNER);

    // A label deferred to the second pass.
    struct PendingLabel {
        bbox: BBox,
        box_rect: Rect,
        text: String,
        preferred_x: i32,
        preferred_y: i32,
    }
    let mut pending_labels: Vec<PendingLabel> = Vec::new();

    // Pass 1: register every box as a highlight (boxes are model-fixed and may
    // overlap each other) and draw the rectangles. Labels are deferred so they
    // can avoid *all* boxes, not just the ones seen so far.
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
                .map(|id| {
                    generate_track_color_argb(
                        id & 0x0FFF_FFFF,
                        TRACK_COLOR_SATURATION,
                        TRACK_COLOR_VALUE,
                    )
                })
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
        let box_rect = Rect::from_xywh(bbox.x, bbox.y, bbox.w, bbox.h);

        occupied.reserve_highlight(box_rect);

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
            pending_labels.push(PendingLabel {
                bbox,
                box_rect,
                text: label_text_with_related(&meta, &od_mtd),
                preferred_x: location.x,
                preferred_y: location.y,
            });
        }

        if settings.draw_tracking_labels
            && let Some(tracking_id) = tracking_id
        {
            // The label's default position sits just below the box so it does
            // not overlap the box highlight; the leader line (when needed) is
            // drawn between the box edge and the label edge.
            let default_baseline = location
                .y
                .saturating_add(location.h)
                .saturating_add(LABEL_LAYOUT_HEIGHT)
                .saturating_add(LABEL_LAYOUT_GAP);
            pending_labels.push(PendingLabel {
                bbox,
                box_rect,
                text: tracking_label_text(tracking_id),
                preferred_x: location.x,
                preferred_y: default_baseline,
            });
        }
    }

    // Pass 2: place labels now that every box is registered, so they avoid all
    // boxes and render on top of them.
    for job in pending_labels {
        if let Some(placement) = place_od_label(
            &mut occupied,
            job.bbox,
            &job.text,
            job.preferred_x,
            job.preferred_y,
        ) {
            push_od_label(
                &mut commands,
                placement,
                job.text,
                job.box_rect,
                settings.labels_color,
            );
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
    // Distinct from the C element's "GstObjectDetectionOverlay" so both plugins
    // can be loaded in the same process (the factory name stays "odoverlay").
    const NAME: &'static str = "GstRsObjectDetectionOverlay";
    type Type = super::ObjectDetectionOverlay;
    type ParentType = gst_video::VideoFilter;
}

impl ObjectDetectionOverlay {
    pub(crate) fn set_pre_draw_hook(&self, hook: Box<dyn crate::hooks::DrawHook>) {
        self.draw_hooks.lock().unwrap().set_pre(hook);
    }

    pub(crate) fn set_post_draw_hook(&self, hook: Box<dyn crate::hooks::DrawHook>) {
        self.draw_hooks.lock().unwrap().set_post(hook);
    }

    pub(crate) fn clear_draw_hooks(&self) {
        self.draw_hooks.lock().unwrap().clear();
    }

    /// Wrap built-in commands with any host pre/post draw hooks. In suppression
    /// mode the built-ins are dropped, so the host's hooks fully replace them
    /// (the pre and post hooks still run).
    fn compose_with_hooks(
        &self,
        builtins: &[DrawCommand],
        width: i32,
        height: i32,
    ) -> Vec<DrawCommand> {
        let ctx = crate::hooks::DrawHookContext { width, height };
        let suppress = self.settings.lock().unwrap().suppress_builtin_rendering;
        let hooks = self.draw_hooks.lock().unwrap();
        // Only suppress when a hook is set, so suppression replaces built-ins
        // rather than silently blanking the overlay when nothing draws.
        let builtins: &[DrawCommand] = if suppress && hooks.has_hooks() {
            &[]
        } else {
            builtins
        };
        hooks.compose(builtins, &ctx)
    }

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
            // `Buffer::with_size` returns uninitialized memory; clear the overlay
            // to fully transparent so only the drawn commands are blended onto
            // the video frame (otherwise the undrawn pixels blend as garbage).
            if let Ok(data) = frame.plane_data_mut(0) {
                data.fill(0);
            }
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
                glib::ParamSpecBoolean::builder("suppress-builtin-rendering")
                    .nick("Suppress built-in rendering")
                    .blurb(
                        "Skip the element's own boxes/labels so custom draw hooks fully replace them",
                    )
                    .default_value(DEFAULT_SUPPRESS_BUILTIN_RENDERING)
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
            "suppress-builtin-rendering" => {
                let mut settings = self.settings.lock().unwrap();
                settings.suppress_builtin_rendering = value.get().expect("type checked upstream");
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
            "suppress-builtin-rendering" => {
                let settings = self.settings.lock().unwrap();
                settings.suppress_builtin_rendering.to_value()
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

        // Wrap the built-ins with any host-supplied pre/post draw hooks. The
        // built-in `commands` are kept separate so coordination claims only
        // cover this element's own content, not host drawing.
        let render_commands =
            self.compose_with_hooks(&commands, frame.width() as i32, frame.height() as i32);

        let attach = *self.attach_composition.lock().unwrap();

        if attach {
            if let Some(composition) = self.build_overlay_composition(
                frame.width(),
                frame.height(),
                &analytics,
                &render_commands,
            ) {
                // SAFETY: The frame is writable and uniquely borrowed here.
                let buffer = unsafe { gst::BufferRef::from_mut_ptr((*frame.as_mut_ptr()).buffer) };
                gst_video::VideoOverlayCompositionMeta::add(buffer, &composition);
            }
        } else if let Some(composition) = self.build_overlay_composition(
            frame.width(),
            frame.height(),
            &analytics,
            &render_commands,
        ) {
            composition
                .blend(frame)
                .map_err(|_| gst::FlowError::Error)?;
        }

        // Publish what we drew so downstream overlays avoid occluding it. When
        // suppression is active the built-ins are not rendered, so nothing is
        // claimed (suppression only applies when a hook replaces them).
        let builtins_suppressed =
            settings.suppress_builtin_rendering && self.draw_hooks.lock().unwrap().has_hooks();
        if !builtins_suppressed && !commands.is_empty() {
            // SAFETY: the frame is writable and uniquely borrowed here.
            let buffer = unsafe { gst::BufferRef::from_mut_ptr((*frame.as_mut_ptr()).buffer) };
            crate::coordination::claim_commands(buffer, &commands, OVERLAY_OWNER);
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
            suppress_builtin_rendering: DEFAULT_SUPPRESS_BUILTIN_RENDERING,
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
            suppress_builtin_rendering: DEFAULT_SUPPRESS_BUILTIN_RENDERING,
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
            suppress_builtin_rendering: DEFAULT_SUPPRESS_BUILTIN_RENDERING,
        };

        let (_, commands) =
            analytics_to_draw_commands(buffer.as_ref(), settings, test_frame_bounds());

        match &commands[0] {
            DrawCommand::Rectangle { argb, .. } => {
                assert_eq!(
                    *argb,
                    generate_track_color_argb(17, TRACK_COLOR_SATURATION, TRACK_COLOR_VALUE)
                );
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
            suppress_builtin_rendering: DEFAULT_SUPPRESS_BUILTIN_RENDERING,
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
            suppress_builtin_rendering: DEFAULT_SUPPRESS_BUILTIN_RENDERING,
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

    fn count_lines(commands: &[DrawCommand]) -> usize {
        commands
            .iter()
            .filter(|command| matches!(command, DrawCommand::Line { .. }))
            .count()
    }

    #[test]
    fn od_label_falls_back_and_is_displaced_when_preferred_spot_is_occupied() {
        let mut occupied = OccupiedRegionRegistry::new(128, 128);
        let bbox = BBox {
            x: 20,
            y: 20,
            w: 30,
            h: 20,
        };

        // The preferred label rect sits just above the box top (baseline at y=20).
        let default = estimate_label_rect(20, 20, "person (c=0.90)");
        occupied.reserve_highlight(default);

        let placement = place_od_label(&mut occupied, bbox, "person (c=0.90)", 20, 20)
            .expect("expected a fallback placement");

        assert_ne!(placement.rect, default);
        assert!(placement.displaced);
    }

    #[test]
    fn displaced_od_label_emits_a_leader_line() {
        let mut commands = Vec::new();
        let mut occupied = OccupiedRegionRegistry::new(128, 128);
        let bbox = BBox {
            x: 20,
            y: 20,
            w: 30,
            h: 20,
        };

        occupied.reserve_highlight(estimate_label_rect(20, 20, "person (c=0.90)"));

        let placement = place_od_label(&mut occupied, bbox, "person (c=0.90)", 20, 20)
            .expect("expected a placement");
        push_od_label(
            &mut commands,
            placement,
            "person (c=0.90)".to_string(),
            Rect::from_xywh(bbox.x, bbox.y, bbox.w, bbox.h),
            0xFFFF_FFFF,
        );

        assert_eq!(count_lines(&commands), 1);
        assert!(matches!(commands.last(), Some(DrawCommand::Text { .. })));
    }

    #[test]
    fn od_label_at_preferred_spot_has_no_leader_line() {
        let mut commands = Vec::new();
        let mut occupied = OccupiedRegionRegistry::new(128, 128);
        let bbox = BBox {
            x: 20,
            y: 20,
            w: 30,
            h: 20,
        };

        let placement = place_od_label(&mut occupied, bbox, "person (c=0.90)", 20, 20)
            .expect("expected a placement");
        assert!(!placement.displaced);
        push_od_label(
            &mut commands,
            placement,
            "person (c=0.90)".to_string(),
            Rect::from_xywh(bbox.x, bbox.y, bbox.w, bbox.h),
            0xFFFF_FFFF,
        );

        assert_eq!(count_lines(&commands), 0);
        assert_eq!(commands.len(), 1);
    }

    #[test]
    fn overlapping_objects_are_labelled_deterministically_with_leader_lines() {
        gst::init().unwrap();

        // Four boxes stacked at nearly the same spot so their default label
        // positions collide and the candidate / least-overlap path engages.
        let build_buffer = || {
            let mut buffer = gst::Buffer::new();
            {
                let mut relation = AnalyticsRelationMeta::add(buffer.make_mut());
                for i in 0..4 {
                    let off = i * 4;
                    relation
                        .add_od_mtd(
                            glib::Quark::from_str("person"),
                            20 + off,
                            40 + off,
                            40,
                            30,
                            0.80,
                        )
                        .unwrap();
                }
            }
            buffer
        };

        let settings = Settings {
            render_enabled: true,
            draw_labels: true,
            draw_tracking_labels: false,
            ..Settings::default()
        };

        let (analytics, commands) =
            analytics_to_draw_commands(build_buffer().as_ref(), settings, test_frame_bounds());
        let (_, commands_again) =
            analytics_to_draw_commands(build_buffer().as_ref(), settings, test_frame_bounds());

        // Deterministic: identical input produces identical draw commands.
        assert_eq!(commands, commands_again);

        // Complete: every object is labelled (none dropped by the placement).
        let labels = commands
            .iter()
            .filter(|c| matches!(c, DrawCommand::Text { .. }))
            .count();
        assert_eq!(analytics.object_count, 4);
        assert_eq!(labels, 4);

        // The crowding forces at least one label off its default position, which
        // must emit a leader line back to its box.
        assert!(count_lines(&commands) >= 1);
    }

    #[test]
    fn label_avoids_a_region_claimed_by_another_element() {
        gst::init().unwrap();
        crate::coordination::register();

        let settings = Settings {
            render_enabled: true,
            draw_labels: true,
            draw_tracking_labels: false,
            ..Settings::default()
        };

        // A single object whose label sits, by default, just above its box.
        let label = "person (c=0.85)";
        let build_buffer = || {
            let mut buffer = gst::Buffer::new();
            {
                let mut relation = AnalyticsRelationMeta::add(buffer.make_mut());
                relation
                    .add_od_mtd(glib::Quark::from_str("person"), 20, 40, 40, 30, 0.85)
                    .unwrap();
            }
            buffer
        };

        // Baseline: no claims, so the label takes its default spot (no leader).
        let (_, baseline) =
            analytics_to_draw_commands(build_buffer().as_ref(), settings, test_frame_bounds());
        assert_eq!(count_lines(&baseline), 0);

        // Another element claims exactly the default label position.
        let mut buffer = build_buffer();
        crate::coordination::add_claimed_regions(
            buffer.make_mut(),
            &[crate::coordination::ClaimedRegion::occlude(
                estimate_label_rect(20, 40, label),
                "hair-spikes",
            )],
        );

        let (_, commands) =
            analytics_to_draw_commands(buffer.as_ref(), settings, test_frame_bounds());

        // The label is still drawn, but displaced off the claimed area, so a
        // leader line now connects it back to the box.
        let labels = commands
            .iter()
            .filter(|c| matches!(c, DrawCommand::Text { .. }))
            .count();
        assert_eq!(labels, 1);
        assert!(count_lines(&commands) >= 1);
    }

    #[test]
    fn host_post_draw_hook_is_appended_to_built_in_commands() {
        gst::init().unwrap();

        // The host registers the sample hook through the public ext trait.
        use crate::hooks::OverlayDrawHooksExt;
        let element =
            glib::Object::builder::<crate::objectdetectionoverlay::ObjectDetectionOverlay>()
                .build();
        element.set_post_draw_hook(crate::hooks::BorderHook {
            argb: 0xFF00_FF00,
            inset: 0.0,
        });

        let builtins = vec![DrawCommand::Rectangle {
            x: 1.0,
            y: 1.0,
            width: 2.0,
            height: 2.0,
            rotation: 0.0,
            argb: 0,
            filled: false,
        }];
        let composed = element.imp().compose_with_hooks(&builtins, 100, 80);

        // Built-in first, then the host's full-frame border on top.
        assert_eq!(composed.len(), 2);
        assert_eq!(composed[0], builtins[0]);
        assert!(matches!(
            composed[1],
            DrawCommand::Rectangle {
                width,
                height,
                filled: false,
                ..
            } if width == 100.0 && height == 80.0
        ));
    }

    #[test]
    fn suppress_builtin_rendering_replaces_builtins_with_hooks() {
        gst::init().unwrap();
        use crate::hooks::OverlayDrawHooksExt;

        let element =
            glib::Object::builder::<crate::objectdetectionoverlay::ObjectDetectionOverlay>()
                .build();
        element.set_property("suppress-builtin-rendering", true);
        element.set_post_draw_hook(crate::hooks::BorderHook {
            argb: 0xFF00_FF00,
            inset: 0.0,
        });

        let builtins = vec![DrawCommand::Rectangle {
            x: 1.0,
            y: 1.0,
            width: 2.0,
            height: 2.0,
            rotation: 0.0,
            argb: 0,
            filled: false,
        }];
        let composed = element.imp().compose_with_hooks(&builtins, 100, 80);

        // The built-ins are dropped; only the host's border remains.
        assert_eq!(composed.len(), 1);
        assert!(matches!(
            composed[0],
            DrawCommand::Rectangle { width, height, .. } if width == 100.0 && height == 80.0
        ));
    }

    #[test]
    fn suppress_builtin_rendering_without_hooks_keeps_builtins() {
        gst::init().unwrap();

        let element =
            glib::Object::builder::<crate::objectdetectionoverlay::ObjectDetectionOverlay>()
                .build();
        element.set_property("suppress-builtin-rendering", true);

        let builtins = vec![DrawCommand::Rectangle {
            x: 1.0,
            y: 1.0,
            width: 2.0,
            height: 2.0,
            rotation: 0.0,
            argb: 0,
            filled: false,
        }];
        // Suppression only applies when a hook replaces the built-ins; with no
        // hook set it is a no-op so the built-ins are kept (never silently blank).
        assert_eq!(
            element.imp().compose_with_hooks(&builtins, 100, 80),
            builtins
        );
    }

    #[test]
    fn labels_do_not_overlap_any_box_even_when_boxes_overlap() {
        gst::init().unwrap();

        // Two overlapping boxes. The label of the first must avoid the second
        // (which is only possible if all boxes are registered before labels).
        let mut buffer = gst::Buffer::new();
        {
            let mut relation = AnalyticsRelationMeta::add(buffer.make_mut());
            relation
                .add_od_mtd(glib::Quark::from_str("a"), 40, 40, 40, 30, 0.9)
                .unwrap();
            relation
                .add_od_mtd(glib::Quark::from_str("b"), 60, 55, 40, 30, 0.9)
                .unwrap();
        }

        let settings = Settings {
            render_enabled: true,
            draw_labels: true,
            draw_tracking_labels: false,
            ..Settings::default()
        };
        let (analytics, commands) =
            analytics_to_draw_commands(buffer.as_ref(), settings, test_frame_bounds());
        assert_eq!(analytics.object_count, 2);

        let boxes: Vec<Rect> = commands
            .iter()
            .filter_map(|c| match c {
                DrawCommand::Rectangle {
                    x,
                    y,
                    width,
                    height,
                    ..
                } => Some(Rect::from_xywh(
                    *x as i32,
                    *y as i32,
                    *width as i32,
                    *height as i32,
                )),
                _ => None,
            })
            .collect();
        let labels: Vec<Rect> = commands
            .iter()
            .filter(|c| matches!(c, DrawCommand::Text { .. }))
            .filter_map(crate::render::content_bounds)
            .collect();

        assert_eq!(boxes.len(), 2);
        assert_eq!(labels.len(), 2);
        for label in &labels {
            for b in &boxes {
                assert!(!label.intersects(*b), "label {label:?} overlaps box {b:?}");
            }
        }
    }
}
