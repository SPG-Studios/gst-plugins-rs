// Copyright (C) 2026 Jeremy Whiting <jeremy.whiting@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0
//
// Shared object-detection command generation: turns AnalyticsRelationMeta into
// a backend-agnostic list of `DrawCommand`s. Used by both the CPU element
// (`imp`) and the GL element (`objectdetectionoverlaygl`).

use gst_analytics::{
    AnalyticsClassificationMtd, AnalyticsMetaRefExt, AnalyticsODMtd, AnalyticsRelationMeta,
    AnalyticsTrackingMtd, RelTypes,
};

use crate::color::generate_track_color_argb;
use crate::geometry::{OccupiedRegionRegistry, Rect};
use crate::overlay_intent::{CandidateKind, LabelIntent};
use crate::placement::{
    LabelPlacement, box_label_candidates, leader_endpoints, place_label, push_leader_line,
};
use crate::render::{
    AnalyticsFrame, DrawCommand, LABEL_LAYOUT_GAP, LABEL_LAYOUT_HEIGHT, measure_label_text_width,
};

/// Owner tag this element uses when claiming/reading shared regions.
pub(crate) const OVERLAY_OWNER: &str = "odoverlay";

pub(crate) const DEFAULT_RENDER_ENABLED: bool = false;
pub(crate) const DEFAULT_OBJECT_DETECTION_OUTLINE_COLOR: u32 = 0xFFFF_FFFF;
pub(crate) const DEFAULT_DRAW_LABELS: bool = true;
pub(crate) const DEFAULT_DRAW_TRACKING_LABELS: bool = true;
pub(crate) const DEFAULT_LABELS_COLOR: u32 = 0xFFFF_FFFF;
pub(crate) const DEFAULT_FILLED_BOX: bool = false;
pub(crate) const DEFAULT_EXPIRE_OVERLAY: u64 = 1_000_000_000;
pub(crate) const DEFAULT_TRACKING_OUTLINE_COLORS: bool = true;
pub(crate) const DEFAULT_SUPPRESS_BUILTIN_RENDERING: bool = false;
pub(crate) const DEFAULT_DEFER_LABELS: bool = false;
// Color generation constants for track coloring (HSV space)
const TRACK_COLOR_SATURATION: f32 = 0.85;
const TRACK_COLOR_VALUE: f32 = 0.95;

#[derive(Debug, Clone, Copy)]
pub(crate) struct Settings {
    // pub(crate) so the sibling CPU element module (`imp`) can read/write these
    // when servicing its GObject properties; the GL element reuses Settings and
    // sets the rendering-relevant fields from its own properties too (see
    // objectdetectionoverlaygl).
    pub(crate) render_enabled: bool,
    pub(crate) object_detection_outline_color: u32,
    pub(crate) draw_labels: bool,
    pub(crate) draw_tracking_labels: bool,
    pub(crate) labels_color: u32,
    pub(crate) filled_box: bool,
    pub(crate) expire_overlay: u64,
    pub(crate) tracking_outline_colors: bool,
    pub(crate) suppress_builtin_rendering: bool,
    /// Cross-element priority (see [`crate::coordination`]).
    pub(crate) priority: i32,
    /// When set, emit labels as deferred intents for a downstream compositor
    /// (see [`crate::overlay_intent`]) instead of placing/rendering them here.
    pub(crate) defer_labels: bool,
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
            priority: crate::coordination::DEFAULT_PRIORITY,
            defer_labels: DEFAULT_DEFER_LABELS,
        }
    }
}

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
pub(crate) struct FrameBounds {
    pub(crate) width: i32,
    pub(crate) height: i32,
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

fn place_od_label(
    registry: &mut OccupiedRegionRegistry,
    bbox: BBox,
    text: &str,
    preferred_x: i32,
    preferred_y: i32,
) -> Option<LabelPlacement> {
    let default = estimate_label_rect(preferred_x, preferred_y, text);
    let box_rect = Rect::from_xywh(bbox.x, bbox.y, bbox.w, bbox.h);
    let candidates = box_label_candidates(box_rect, measure_label_text_width(text));

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

/// Build the object-detection overlay for `buffer`: the anchored draw commands
/// (boxes) plus, depending on [`Settings::defer_labels`], either placed label
/// commands (local mode) or a list of deferred [`LabelIntent`]s for a downstream
/// compositor (defer mode; the returned commands then contain only boxes).
pub(crate) fn analytics_to_overlay(
    buffer: &gst::BufferRef,
    settings: Settings,
    bounds: FrameBounds,
) -> (AnalyticsFrame<'static>, Vec<DrawCommand>, Vec<LabelIntent>) {
    let Some(meta) = buffer.meta::<AnalyticsRelationMeta>() else {
        return (AnalyticsFrame::default(), Vec::new(), Vec::new());
    };

    let mut commands = Vec::new();
    let mut object_count = 0;
    let mut occupied = OccupiedRegionRegistry::new(bounds.width, bounds.height);

    // Avoid regions other elements claimed at our priority or higher; lower-
    // priority claims are left out so we draw over them.
    crate::coordination::seed_registry_from_claims(
        &mut occupied,
        buffer,
        OVERLAY_OWNER,
        settings.priority,
    );

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

    // Pass 2: either place the labels locally now that every box is registered
    // (so they avoid all boxes), or defer them as intents for the compositor.
    let mut deferred = Vec::new();
    if settings.defer_labels {
        for job in pending_labels {
            deferred.push(LabelIntent {
                preferred: estimate_label_rect(job.preferred_x, job.preferred_y, &job.text),
                anchor: job.box_rect,
                text: job.text,
                color: settings.labels_color,
                kind: CandidateKind::Box,
                priority: settings.priority,
                owner: OVERLAY_OWNER.to_string(),
            });
        }
    } else {
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
    }

    (
        AnalyticsFrame {
            object_count,
            ..Default::default()
        },
        commands,
        deferred,
    )
}

/// Local-mode convenience used by the unit tests: the placed overlay commands
/// only (deferred intents dropped). Production code calls [`analytics_to_overlay`].
#[cfg(test)]
fn analytics_to_draw_commands(
    buffer: &gst::BufferRef,
    settings: Settings,
    bounds: FrameBounds,
) -> (AnalyticsFrame<'static>, Vec<DrawCommand>) {
    let (frame, commands, _deferred) = analytics_to_overlay(buffer, settings, bounds);
    (frame, commands)
}

#[cfg(test)]
fn test_frame_bounds() -> FrameBounds {
    FrameBounds {
        width: 192,
        height: 192,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gst::glib;
    use gst_analytics::{
        AnalyticsRelationMetaClassificationExt, AnalyticsRelationMetaODExt,
        AnalyticsRelationMetaTrackingExt,
    };

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
            priority: crate::coordination::DEFAULT_PRIORITY,
            defer_labels: DEFAULT_DEFER_LABELS,
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
            priority: crate::coordination::DEFAULT_PRIORITY,
            defer_labels: DEFAULT_DEFER_LABELS,
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
            priority: crate::coordination::DEFAULT_PRIORITY,
            defer_labels: DEFAULT_DEFER_LABELS,
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
            priority: crate::coordination::DEFAULT_PRIORITY,
            defer_labels: DEFAULT_DEFER_LABELS,
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
    fn defer_labels_emits_intents_instead_of_drawing_text() {
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
            draw_labels: true,
            defer_labels: true,
            ..Settings::default()
        };
        let (_, commands, deferred) =
            analytics_to_overlay(buffer.as_ref(), settings, test_frame_bounds());

        // The box is still drawn, but the label is deferred rather than placed.
        assert!(
            commands
                .iter()
                .any(|c| matches!(c, DrawCommand::Rectangle { .. }))
        );
        assert!(
            !commands
                .iter()
                .any(|c| matches!(c, DrawCommand::Text { .. }))
        );
        assert_eq!(deferred.len(), 1);
        assert_eq!(deferred[0].kind, CandidateKind::Box);
        assert_eq!(deferred[0].text, "person (c=0.85)");
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
            priority: crate::coordination::DEFAULT_PRIORITY,
            defer_labels: DEFAULT_DEFER_LABELS,
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
                0,
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
