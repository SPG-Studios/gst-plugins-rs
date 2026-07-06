// Copyright (C) 2026 Jeremy Whiting <jeremy.whiting@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0
//
// Shared keypoints command generation: turns AnalyticsRelationMeta into a
// backend-agnostic list of `DrawCommand`s (markers, labels, skeleton). Used by
// both the CPU element (`imp`) and the GL element (`keypointsoverlaygl`).

use gst_analytics::{
    AnalyticsGroupMtd, AnalyticsKeypointMtd, AnalyticsMetaRefExt, AnalyticsRelationMeta, RelTypes,
};

use crate::geometry::{OccupiedRegionRegistry, Rect};
use crate::overlay_intent::{CandidateKind, LabelIntent};
use crate::placement::{
    LabelPlacement, leader_endpoints, place_label, point_label_candidates, push_leader_line,
};
use crate::render::{
    AnalyticsFrame, DrawCommand, LABEL_LAYOUT_GAP, LABEL_LAYOUT_HEIGHT, LineRole,
    measure_centered_label_text_width,
};

use std::sync::LazyLock;

/// Owner tag this element uses when claiming/reading shared regions.
pub(crate) const OVERLAY_OWNER: &str = "keypointsoverlay";

pub(crate) const DEFAULT_RENDER_ENABLED: bool = false;
pub(crate) const DEFAULT_KEYPOINT_COLOR: u32 = 0xFFFF_0000;
pub(crate) const DEFAULT_KEYPOINT_RADIUS: f64 = 3.0;
pub(crate) const DEFAULT_DRAW_LABELS: bool = true;
pub(crate) const DEFAULT_LABELS_COLOR: u32 = 0xFFFF_FFFF;
pub(crate) const DEFAULT_DRAW_SKELETON: bool = false;
pub(crate) const DEFAULT_SKELETON_COLOR: u32 = 0xFF00_FF00;
pub(crate) const DEFAULT_SKELETON_LINE_WIDTH: f64 = 2.0;
pub(crate) const DEFAULT_SUPPRESS_BUILTIN_RENDERING: bool = false;
pub(crate) const DEFAULT_DEFER_LABELS: bool = false;

pub(crate) static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "keypointsoverlay",
        gst::DebugColorFlags::empty(),
        Some("Keypoints overlay skeleton"),
    )
});

#[derive(Debug, Clone)]
pub(crate) struct Settings {
    pub(crate) render_enabled: bool,
    pub(crate) keypoint_color: u32,
    pub(crate) keypoint_radius: f64,
    pub(crate) draw_labels: bool,
    pub(crate) labels_color: u32,
    pub(crate) draw_skeleton: bool,
    pub(crate) skeleton_color: u32,
    pub(crate) skeleton_line_width: f64,
    pub(crate) semantic_tag: Option<String>,
    pub(crate) suppress_builtin_rendering: bool,
    /// Cross-element priority (see [`crate::coordination`]).
    pub(crate) priority: i32,
    /// When set, emit labels as deferred intents for a downstream compositor
    /// (see [`crate::overlay_intent`]) instead of placing/rendering them here.
    pub(crate) defer_labels: bool,
    /// Publish drawn content as claimed regions for downstream coordination.
    pub(crate) publish_claimed_regions: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            render_enabled: DEFAULT_RENDER_ENABLED,
            keypoint_color: DEFAULT_KEYPOINT_COLOR,
            keypoint_radius: DEFAULT_KEYPOINT_RADIUS,
            draw_labels: DEFAULT_DRAW_LABELS,
            labels_color: DEFAULT_LABELS_COLOR,
            draw_skeleton: DEFAULT_DRAW_SKELETON,
            skeleton_color: DEFAULT_SKELETON_COLOR,
            skeleton_line_width: DEFAULT_SKELETON_LINE_WIDTH,
            semantic_tag: None,
            suppress_builtin_rendering: DEFAULT_SUPPRESS_BUILTIN_RENDERING,
            priority: crate::coordination::DEFAULT_PRIORITY,
            defer_labels: DEFAULT_DEFER_LABELS,
            publish_claimed_regions: crate::coordination::DEFAULT_PUBLISH_CLAIMED_REGIONS,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct KeypointSample {
    id: u32,
    x: i32,
    y: i32,
    confidence: f32,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct FrameBounds {
    pub(crate) width: i32,
    pub(crate) height: i32,
}

fn keypoint_in_frame(bounds: FrameBounds, x: i32, y: i32) -> bool {
    x >= 0 && y >= 0 && x < bounds.width && y < bounds.height
}

fn clamp_to_frame(bounds: FrameBounds, x: i32, y: i32) -> (i32, i32) {
    let max_x = bounds.width.saturating_sub(1);
    let max_y = bounds.height.saturating_sub(1);

    (x.clamp(0, max_x), y.clamp(0, max_y))
}

fn keypoint_position(
    mtd: &gst_analytics::AnalyticsMtdRef<'_, AnalyticsKeypointMtd>,
) -> Option<(i32, i32)> {
    let pos = mtd.position().ok()?;
    Some((pos.x, pos.y))
}

fn keypoint_confidence(mtd: &gst_analytics::AnalyticsMtdRef<'_, AnalyticsKeypointMtd>) -> f32 {
    mtd.confidence().unwrap_or(1.0)
}

fn confidence_label(confidence: f32) -> String {
    format!("{confidence:.2}")
}

/// A keypoint label's preferred (default) rect: centered horizontally, sitting
/// just above the keypoint.
fn keypoint_label_default(sample: KeypointSample, label_w: i32) -> Rect {
    Rect::from_xywh(
        sample.x.saturating_sub(label_w / 2),
        sample
            .y
            .saturating_sub(LABEL_LAYOUT_HEIGHT)
            .saturating_sub(LABEL_LAYOUT_GAP),
        label_w,
        LABEL_LAYOUT_HEIGHT,
    )
}

/// Place a keypoint's confidence label: preferred just above the keypoint, with
/// the shared candidate ring as fallbacks.
fn place_keypoint_label(
    registry: &mut OccupiedRegionRegistry,
    sample: KeypointSample,
    text: &str,
) -> Option<LabelPlacement> {
    let label_w = measure_centered_label_text_width(text);
    let default = keypoint_label_default(sample, label_w);
    let candidates = point_label_candidates(sample.x, sample.y, label_w, LABEL_LAYOUT_HEIGHT);

    place_label(registry, default, &candidates)
}

/// Build a deferred intent for a keypoint label, with the same preferred rect
/// `place_keypoint_label` would use, so a compositor reproduces its placement.
fn keypoint_label_intent(sample: KeypointSample, text: String, settings: &Settings) -> LabelIntent {
    let label_w = measure_centered_label_text_width(&text);
    LabelIntent {
        preferred: keypoint_label_default(sample, label_w),
        anchor: Rect::from_xywh(sample.x, sample.y, 0, 0),
        text,
        color: settings.labels_color,
        kind: CandidateKind::Point,
        priority: settings.priority,
        owner: OVERLAY_OWNER.to_string(),
    }
}

/// Draw a keypoint's label at its placed position, with a leader line back to
/// the keypoint when the label was displaced.
fn push_keypoint_label(
    commands: &mut Vec<DrawCommand>,
    placement: LabelPlacement,
    sample: KeypointSample,
    text: String,
    argb: u32,
) {
    let (cx, cy) = placement.rect.center();

    if placement.displaced {
        // The keypoint is a point feature, so use a zero-sized rect at it.
        let keypoint = Rect::from_xywh(sample.x, sample.y, 0, 0);
        let (from, to) = leader_endpoints(keypoint, placement.rect);
        push_leader_line(commands, from, to, argb);
    }

    commands.push(DrawCommand::TextCentered {
        x: cx as f32,
        y: cy as f32,
        text,
        argb,
    });
}

struct KeypointCommandContext<'a> {
    settings: &'a Settings,
    draw_skeletons: bool,
    draw_group_label_once: bool,
    meta: &'a gst::MetaRef<'a, AnalyticsRelationMeta>,
    bounds: FrameBounds,
    occupied: &'a mut OccupiedRegionRegistry,
    /// In defer mode, labels are collected here as intents instead of placed.
    deferred: &'a mut Vec<LabelIntent>,
}

fn push_keypoint_commands(
    commands: &mut Vec<DrawCommand>,
    samples: &[KeypointSample],
    ctx: &mut KeypointCommandContext<'_>,
) {
    push_keypoints(commands, samples, ctx);
    push_relation_skeleton(commands, samples, ctx);
}

/// Draw the keypoint markers and their confidence label(s). Shared by every
/// renderer; only the skeleton differs between them.
fn push_keypoints(
    commands: &mut Vec<DrawCommand>,
    samples: &[KeypointSample],
    ctx: &mut KeypointCommandContext<'_>,
) {
    let mut first_in_frame: Option<KeypointSample> = None;
    let keypoint_radius_px = ctx.settings.keypoint_radius.ceil() as i32;

    // Pass 1: register and draw every keypoint marker (a model-fixed highlight,
    // so markers may overlap). Done before any label so labels avoid them all.
    for sample in samples {
        if !keypoint_in_frame(ctx.bounds, sample.x, sample.y) {
            continue;
        }

        if first_in_frame.is_none() {
            first_in_frame = Some(*sample);
        }

        ctx.occupied.reserve_highlight(Rect::from_xywh(
            sample.x.saturating_sub(keypoint_radius_px),
            sample.y.saturating_sub(keypoint_radius_px),
            keypoint_radius_px.saturating_mul(2).saturating_add(1),
            keypoint_radius_px.saturating_mul(2).saturating_add(1),
        ));

        commands.push(DrawCommand::Circle {
            cx: sample.x as f32,
            cy: sample.y as f32,
            radius: ctx.settings.keypoint_radius as f32,
            argb: ctx.settings.keypoint_color,
        });
    }

    if !ctx.settings.draw_labels {
        return;
    }

    // Pass 2: labels, now avoiding every marker registered above. In defer mode
    // collect them as intents for a downstream compositor instead of placing.
    let mut emit_label = |ctx: &mut KeypointCommandContext<'_>, sample: KeypointSample| {
        let label = confidence_label(sample.confidence);
        if ctx.settings.defer_labels {
            ctx.deferred
                .push(keypoint_label_intent(sample, label, ctx.settings));
        } else if let Some(placement) = place_keypoint_label(ctx.occupied, sample, &label) {
            push_keypoint_label(
                commands,
                placement,
                sample,
                label,
                ctx.settings.labels_color,
            );
        }
    };

    if ctx.draw_group_label_once {
        if let Some(sample) = first_in_frame {
            emit_label(ctx, sample);
        }
    } else {
        for sample in samples {
            if !keypoint_in_frame(ctx.bounds, sample.x, sample.y) {
                continue;
            }
            emit_label(ctx, *sample);
        }
    }
}

/// Generic skeleton: connect keypoints linked by `RELATE_TO` relations.
fn push_relation_skeleton(
    commands: &mut Vec<DrawCommand>,
    samples: &[KeypointSample],
    ctx: &mut KeypointCommandContext<'_>,
) {
    if !ctx.draw_skeletons || !ctx.settings.draw_skeleton {
        return;
    }

    for sample in samples {
        for related in ctx
            .meta
            .iter_direct_related::<AnalyticsKeypointMtd>(sample.id, RelTypes::RELATE_TO)
        {
            if sample.id >= related.id() {
                continue;
            }

            if let Some((x2, y2)) = keypoint_position(&related) {
                let (x0, y0) = clamp_to_frame(ctx.bounds, sample.x, sample.y);
                let (x1, y1) = clamp_to_frame(ctx.bounds, x2, y2);

                commands.push(DrawCommand::Line {
                    x0: x0 as f32,
                    y0: y0 as f32,
                    x1: x1 as f32,
                    y1: y1 as f32,
                    argb: ctx.settings.skeleton_color,
                    width: ctx.settings.skeleton_line_width as f32,
                    role: LineRole::Skeleton,
                });
            }
        }
    }
}

/// Semantic tag of the 21-point hand keypoint model.
const HAND_KP_21_TAG: &str = "hand-kp-21";

/// Bone connections for the 21-point hand model, as index pairs into the
/// group's ordered keypoints (MediaPipe-style topology: wrist + 4 joints per
/// finger).
const HAND_KP_21_BONES: [(usize, usize); 21] = [
    (0, 1),
    (1, 2),
    (2, 3),
    (3, 4), // thumb
    (0, 5),
    (5, 6),
    (6, 7),
    (7, 8), // index
    (5, 9),
    (9, 10),
    (10, 11),
    (11, 12), // middle
    (9, 13),
    (13, 14),
    (14, 15),
    (15, 16), // ring
    (13, 17),
    (17, 18),
    (18, 19),
    (19, 20), // pinky
    (0, 17),  // palm base
];

/// Renders one semantically-tagged keypoint group into draw commands.
///
/// [`renderer_for_tag`] routes known semantic tags to a specialized renderer and
/// everything else to [`GenericRelationRenderer`].
trait GroupRenderer: Sync {
    /// Identifier for debugging / dispatch tests.
    fn name(&self) -> &'static str;

    fn render(
        &self,
        commands: &mut Vec<DrawCommand>,
        samples: &[KeypointSample],
        ctx: &mut KeypointCommandContext<'_>,
    );
}

/// Fallback renderer: keypoints + labels, with the skeleton derived from
/// relation metadata. Handles any group.
struct GenericRelationRenderer;

impl GroupRenderer for GenericRelationRenderer {
    fn name(&self) -> &'static str {
        "generic-relation"
    }

    fn render(
        &self,
        commands: &mut Vec<DrawCommand>,
        samples: &[KeypointSample],
        ctx: &mut KeypointCommandContext<'_>,
    ) {
        push_keypoints(commands, samples, ctx);
        push_relation_skeleton(commands, samples, ctx);
    }
}

/// Specialized renderer for the 21-point hand model: draws the hand skeleton
/// from the fixed [`HAND_KP_21_BONES`] topology, so no relation metadata is
/// required.
struct HandKp21Renderer;

impl GroupRenderer for HandKp21Renderer {
    fn name(&self) -> &'static str {
        HAND_KP_21_TAG
    }

    fn render(
        &self,
        commands: &mut Vec<DrawCommand>,
        samples: &[KeypointSample],
        ctx: &mut KeypointCommandContext<'_>,
    ) {
        push_keypoints(commands, samples, ctx);

        if !ctx.draw_skeletons || !ctx.settings.draw_skeleton {
            return;
        }

        for (a, b) in HAND_KP_21_BONES {
            let (Some(from), Some(to)) = (samples.get(a), samples.get(b)) else {
                continue;
            };
            let (x0, y0) = clamp_to_frame(ctx.bounds, from.x, from.y);
            let (x1, y1) = clamp_to_frame(ctx.bounds, to.x, to.y);
            commands.push(DrawCommand::Line {
                x0: x0 as f32,
                y0: y0 as f32,
                x1: x1 as f32,
                y1: y1 as f32,
                argb: ctx.settings.skeleton_color,
                width: ctx.settings.skeleton_line_width as f32,
                role: LineRole::Skeleton,
            });
        }
    }
}

/// Route a group's semantic tag to its renderer: known tags get a specialized
/// renderer, everything else falls back to the generic relation renderer.
fn renderer_for_tag(semantic_tag: Option<&str>) -> &'static dyn GroupRenderer {
    static GENERIC: GenericRelationRenderer = GenericRelationRenderer;
    static HAND_KP_21: HandKp21Renderer = HandKp21Renderer;

    match semantic_tag {
        Some(HAND_KP_21_TAG) => &HAND_KP_21,
        _ => &GENERIC,
    }
}

/// Build the keypoints overlay for `buffer`: anchored draw commands (markers and
/// skeleton) plus, depending on [`Settings::defer_labels`], either placed label
/// commands (local mode) or a list of deferred [`LabelIntent`]s for a downstream
/// compositor (defer mode; the returned commands then contain no labels).
pub(crate) fn analytics_to_overlay(
    buffer: &gst::BufferRef,
    settings: &Settings,
    bounds: FrameBounds,
) -> (AnalyticsFrame<'static>, Vec<DrawCommand>, Vec<LabelIntent>) {
    let Some(meta) = buffer.meta::<AnalyticsRelationMeta>() else {
        return (AnalyticsFrame::default(), Vec::new(), Vec::new());
    };

    let mut commands = Vec::new();
    let mut deferred = Vec::new();
    let mut keypoint_count = 0usize;
    let mut occupied = OccupiedRegionRegistry::new(bounds.width, bounds.height);

    // Avoid regions other elements claimed at our priority or higher; lower-
    // priority claims are left out so we draw over them.
    crate::coordination::seed_registry_from_claims(
        &mut occupied,
        buffer,
        OVERLAY_OWNER,
        settings.priority,
    );

    if let Some(semantic_tag) = settings.semantic_tag.as_deref() {
        for group in meta.iter::<AnalyticsGroupMtd>() {
            if !group.semantic_tag_has_prefix(semantic_tag) {
                continue;
            }

            let mut group_samples = Vec::new();
            for keypoint in group.iter::<AnalyticsKeypointMtd>() {
                let Some((x, y)) = keypoint_position(&keypoint) else {
                    continue;
                };

                group_samples.push(KeypointSample {
                    id: keypoint.id(),
                    x,
                    y,
                    confidence: keypoint_confidence(&keypoint),
                });
            }

            keypoint_count += group_samples.len();
            let mut ctx = KeypointCommandContext {
                settings,
                draw_skeletons: true,
                draw_group_label_once: true,
                meta: &meta,
                bounds,
                occupied: &mut occupied,
                deferred: &mut deferred,
            };

            // Route the group to a specialized renderer by its semantic tag,
            // falling back to the generic relation renderer.
            let group_tag = group.semantic_tag().ok();
            let renderer = renderer_for_tag(group_tag.as_deref());
            gst::trace!(
                CAT,
                "routing keypoint group (tag {:?}) to {} renderer",
                group_tag.as_deref(),
                renderer.name()
            );
            renderer.render(&mut commands, &group_samples, &mut ctx);
        }
    } else {
        let mut samples = Vec::new();
        for keypoint in meta.iter::<AnalyticsKeypointMtd>() {
            let Some((x, y)) = keypoint_position(&keypoint) else {
                continue;
            };

            samples.push(KeypointSample {
                id: keypoint.id(),
                x,
                y,
                confidence: keypoint_confidence(&keypoint),
            });
        }

        keypoint_count = samples.len();
        let mut ctx = KeypointCommandContext {
            settings,
            draw_skeletons: false,
            draw_group_label_once: false,
            meta: &meta,
            bounds,
            occupied: &mut occupied,
            deferred: &mut deferred,
        };
        push_keypoint_commands(&mut commands, &samples, &mut ctx);
    }

    (
        AnalyticsFrame {
            keypoint_count,
            semantic_tag: None,
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
    settings: &Settings,
    bounds: FrameBounds,
) -> (AnalyticsFrame<'static>, Vec<DrawCommand>) {
    let (frame, commands, _deferred) = analytics_to_overlay(buffer, settings, bounds);
    (frame, commands)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gst_analytics::{
        AnalyticsKeypointDimensions, AnalyticsKeypointPosition, AnalyticsKeypointVisibility,
        AnalyticsRelationMetaGroupExt, AnalyticsRelationMetaKeypointExt,
    };

    fn init() {
        use std::sync::Once;

        static INIT: Once = Once::new();

        INIT.call_once(|| {
            gst::init().unwrap();
        });
    }

    fn count_centered_labels(commands: &[DrawCommand]) -> usize {
        commands
            .iter()
            .filter(|command| matches!(command, DrawCommand::TextCentered { .. }))
            .count()
    }

    fn count_circles(commands: &[DrawCommand]) -> usize {
        commands
            .iter()
            .filter(|command| matches!(command, DrawCommand::Circle { .. }))
            .count()
    }

    fn count_lines(commands: &[DrawCommand]) -> usize {
        commands
            .iter()
            .filter(|command| matches!(command, DrawCommand::Line { .. }))
            .count()
    }

    #[test]
    fn grouped_mode_emits_single_label_per_group() {
        init();

        let mut buffer = gst::Buffer::from_mut_slice(vec![0_u8; 64 * 64 * 4]);
        {
            let buffer_ref = buffer.get_mut().unwrap();
            let mut relation = gst_analytics::AnalyticsRelationMeta::add(buffer_ref);

            let points = vec![
                AnalyticsKeypointPosition {
                    x: 16,
                    y: 16,
                    z: 0,
                    dimension: AnalyticsKeypointDimensions::_2d,
                },
                AnalyticsKeypointPosition {
                    x: 24,
                    y: 24,
                    z: 0,
                    dimension: AnalyticsKeypointDimensions::_2d,
                },
                AnalyticsKeypointPosition {
                    x: 32,
                    y: 32,
                    z: 0,
                    dimension: AnalyticsKeypointDimensions::_2d,
                },
            ];

            relation
                .add_keypoints_group_from_positions("pose/hand", &points, None, None, &[])
                .unwrap();
        }

        let settings = Settings {
            draw_labels: true,
            semantic_tag: Some("pose/".to_string()),
            ..Default::default()
        };

        let bounds = FrameBounds {
            width: 64,
            height: 64,
        };
        let (_, commands) = analytics_to_draw_commands(buffer.as_ref(), &settings, bounds);

        assert_eq!(count_circles(&commands), 3);
        assert_eq!(count_centered_labels(&commands), 1);
    }

    #[test]
    fn ungrouped_mode_emits_label_per_keypoint() {
        init();

        let mut buffer = gst::Buffer::from_mut_slice(vec![0_u8; 64 * 64 * 4]);
        {
            let buffer_ref = buffer.get_mut().unwrap();
            let mut relation = gst_analytics::AnalyticsRelationMeta::add(buffer_ref);

            relation
                .add_keypoint_mtd(
                    AnalyticsKeypointDimensions::_2d,
                    16,
                    16,
                    0,
                    AnalyticsKeypointVisibility::VISIBLE,
                    0.9,
                )
                .unwrap();
            relation
                .add_keypoint_mtd(
                    AnalyticsKeypointDimensions::_2d,
                    24,
                    24,
                    0,
                    AnalyticsKeypointVisibility::VISIBLE,
                    0.8,
                )
                .unwrap();
        }

        let settings = Settings {
            draw_labels: true,
            semantic_tag: None,
            ..Default::default()
        };

        let bounds = FrameBounds {
            width: 64,
            height: 64,
        };
        let (_, commands) = analytics_to_draw_commands(buffer.as_ref(), &settings, bounds);

        assert_eq!(count_circles(&commands), 2);
        assert_eq!(count_centered_labels(&commands), 2);
    }

    #[test]
    fn defer_labels_emits_intents_instead_of_drawing_text() {
        init();

        let mut buffer = gst::Buffer::from_mut_slice(vec![0_u8; 64 * 64 * 4]);
        {
            let buffer_ref = buffer.get_mut().unwrap();
            let mut relation = gst_analytics::AnalyticsRelationMeta::add(buffer_ref);
            relation
                .add_keypoint_mtd(
                    AnalyticsKeypointDimensions::_2d,
                    16,
                    16,
                    0,
                    AnalyticsKeypointVisibility::VISIBLE,
                    0.9,
                )
                .unwrap();
            relation
                .add_keypoint_mtd(
                    AnalyticsKeypointDimensions::_2d,
                    24,
                    24,
                    0,
                    AnalyticsKeypointVisibility::VISIBLE,
                    0.8,
                )
                .unwrap();
        }

        let settings = Settings {
            draw_labels: true,
            semantic_tag: None,
            defer_labels: true,
            ..Default::default()
        };
        let bounds = FrameBounds {
            width: 64,
            height: 64,
        };
        let (_, commands, deferred) = analytics_to_overlay(buffer.as_ref(), &settings, bounds);

        // Markers still drawn; the labels are deferred rather than placed.
        assert_eq!(count_circles(&commands), 2);
        assert_eq!(count_centered_labels(&commands), 0);
        assert_eq!(deferred.len(), 2);
        assert!(deferred.iter().all(|d| d.kind == CandidateKind::Point));
    }

    #[test]
    fn semantic_tag_filter_skips_non_matching_group() {
        init();

        let mut buffer = gst::Buffer::from_mut_slice(vec![0_u8; 64 * 64 * 4]);
        {
            let buffer_ref = buffer.get_mut().unwrap();
            let mut relation = gst_analytics::AnalyticsRelationMeta::add(buffer_ref);

            let points = vec![
                AnalyticsKeypointPosition {
                    x: 16,
                    y: 16,
                    z: 0,
                    dimension: AnalyticsKeypointDimensions::_2d,
                },
                AnalyticsKeypointPosition {
                    x: 24,
                    y: 24,
                    z: 0,
                    dimension: AnalyticsKeypointDimensions::_2d,
                },
            ];

            relation
                .add_keypoints_group_from_positions("hand/left", &points, None, None, &[])
                .unwrap();
        }

        let settings = Settings {
            draw_labels: true,
            semantic_tag: Some("pose/".to_string()),
            ..Default::default()
        };

        let bounds = FrameBounds {
            width: 64,
            height: 64,
        };
        let (_, commands) = analytics_to_draw_commands(buffer.as_ref(), &settings, bounds);

        assert!(commands.is_empty());
    }

    #[test]
    fn grouped_mode_with_all_points_outside_frame_emits_no_labels() {
        init();

        let mut buffer = gst::Buffer::from_mut_slice(vec![0_u8; 64 * 64 * 4]);
        {
            let buffer_ref = buffer.get_mut().unwrap();
            let mut relation = gst_analytics::AnalyticsRelationMeta::add(buffer_ref);

            let points = vec![
                AnalyticsKeypointPosition {
                    x: -10,
                    y: 16,
                    z: 0,
                    dimension: AnalyticsKeypointDimensions::_2d,
                },
                AnalyticsKeypointPosition {
                    x: -20,
                    y: 24,
                    z: 0,
                    dimension: AnalyticsKeypointDimensions::_2d,
                },
            ];

            relation
                .add_keypoints_group_from_positions("pose/hand", &points, None, None, &[])
                .unwrap();
        }

        let settings = Settings {
            draw_labels: true,
            semantic_tag: Some("pose/".to_string()),
            ..Default::default()
        };

        let bounds = FrameBounds {
            width: 64,
            height: 64,
        };
        let (_, commands) = analytics_to_draw_commands(buffer.as_ref(), &settings, bounds);

        assert_eq!(count_circles(&commands), 0);
        assert_eq!(count_centered_labels(&commands), 0);
    }

    #[test]
    fn keypoint_label_falls_back_and_is_displaced_when_default_is_occupied() {
        let mut occupied = OccupiedRegionRegistry::new(128, 128);
        let sample = KeypointSample {
            id: 1,
            x: 30,
            y: 30,
            confidence: 0.9,
        };

        // Occupy the default position (centered just above the keypoint).
        let label_w = measure_centered_label_text_width("0.90");
        let default = Rect::from_xywh(
            sample.x - label_w / 2,
            sample.y - LABEL_LAYOUT_HEIGHT - LABEL_LAYOUT_GAP,
            label_w,
            LABEL_LAYOUT_HEIGHT,
        );
        occupied.reserve_highlight(default);

        let placement =
            place_keypoint_label(&mut occupied, sample, "0.90").expect("expected a placement");

        assert_ne!(placement.rect, default);
        assert!(placement.displaced);
    }

    #[test]
    fn displaced_keypoint_label_emits_a_leader_line() {
        let mut commands = Vec::new();
        let mut occupied = OccupiedRegionRegistry::new(128, 128);
        let sample = KeypointSample {
            id: 1,
            x: 30,
            y: 30,
            confidence: 0.9,
        };

        // Block the default position so the label must be displaced.
        let label_w = measure_centered_label_text_width("0.90");
        occupied.reserve_highlight(Rect::from_xywh(
            sample.x - label_w / 2,
            sample.y - LABEL_LAYOUT_HEIGHT - LABEL_LAYOUT_GAP,
            label_w,
            LABEL_LAYOUT_HEIGHT,
        ));

        let placement =
            place_keypoint_label(&mut occupied, sample, "0.90").expect("expected a placement");
        push_keypoint_label(
            &mut commands,
            placement,
            sample,
            "0.90".to_string(),
            0xFFFF_FFFF,
        );

        assert_eq!(count_centered_labels(&commands), 1);
        assert_eq!(count_lines(&commands), 1);
    }

    #[test]
    fn keypoint_label_at_default_position_has_no_leader_line() {
        let mut commands = Vec::new();
        let mut occupied = OccupiedRegionRegistry::new(128, 128);
        let sample = KeypointSample {
            id: 1,
            x: 30,
            y: 30,
            confidence: 0.9,
        };

        let placement =
            place_keypoint_label(&mut occupied, sample, "0.90").expect("expected a placement");
        assert!(!placement.displaced);
        push_keypoint_label(
            &mut commands,
            placement,
            sample,
            "0.90".to_string(),
            0xFFFF_FFFF,
        );

        assert_eq!(count_centered_labels(&commands), 1);
        assert_eq!(count_lines(&commands), 0);
    }

    fn hand_points(count: i32) -> Vec<AnalyticsKeypointPosition> {
        (0..count)
            .map(|i| AnalyticsKeypointPosition {
                x: 5 + i,
                y: 5 + i,
                z: 0,
                dimension: AnalyticsKeypointDimensions::_2d,
            })
            .collect()
    }

    #[test]
    fn dispatch_routes_known_tag_to_specialized_renderer() {
        assert_eq!(
            renderer_for_tag(Some(HAND_KP_21_TAG)).name(),
            HAND_KP_21_TAG
        );
        // Unknown tags and untagged groups fall back to the generic renderer.
        assert_eq!(
            renderer_for_tag(Some("pose/body")).name(),
            "generic-relation"
        );
        assert_eq!(renderer_for_tag(None).name(), "generic-relation");
    }

    #[test]
    fn hand_kp_21_renderer_draws_skeleton_from_topology_without_relations() {
        init();

        // 21 hand keypoints with NO relations between them.
        let mut buffer = gst::Buffer::from_mut_slice(vec![0_u8; 64 * 64 * 4]);
        {
            let buffer_ref = buffer.get_mut().unwrap();
            let mut relation = gst_analytics::AnalyticsRelationMeta::add(buffer_ref);
            relation
                .add_keypoints_group_from_positions(
                    HAND_KP_21_TAG,
                    &hand_points(21),
                    None,
                    None,
                    &[],
                )
                .unwrap();
        }

        let settings = Settings {
            draw_labels: false,
            draw_skeleton: true,
            semantic_tag: Some(HAND_KP_21_TAG.to_string()),
            ..Default::default()
        };
        let bounds = FrameBounds {
            width: 64,
            height: 64,
        };
        let (_, commands) = analytics_to_draw_commands(buffer.as_ref(), &settings, bounds);

        // The specialized renderer draws the skeleton from the fixed topology,
        // so all 21 bones become lines even though there are no relations.
        assert_eq!(count_lines(&commands), HAND_KP_21_BONES.len());
    }

    #[test]
    fn generic_renderer_draws_no_skeleton_without_relations() {
        init();

        // Same keypoints, but an unspecialized tag -> generic renderer, which
        // derives the skeleton from relation metadata (there are none here).
        let mut buffer = gst::Buffer::from_mut_slice(vec![0_u8; 64 * 64 * 4]);
        {
            let buffer_ref = buffer.get_mut().unwrap();
            let mut relation = gst_analytics::AnalyticsRelationMeta::add(buffer_ref);
            relation
                .add_keypoints_group_from_positions("pose/hand", &hand_points(21), None, None, &[])
                .unwrap();
        }

        let settings = Settings {
            draw_labels: false,
            draw_skeleton: true,
            semantic_tag: Some("pose/".to_string()),
            ..Default::default()
        };
        let bounds = FrameBounds {
            width: 64,
            height: 64,
        };
        let (_, commands) = analytics_to_draw_commands(buffer.as_ref(), &settings, bounds);

        assert_eq!(count_lines(&commands), 0);
    }

    #[test]
    fn labels_do_not_overlap_keypoint_markers() {
        init();

        // Two keypoints close enough that their markers overlap.
        let mut buffer = gst::Buffer::from_mut_slice(vec![0_u8; 128 * 128 * 4]);
        {
            let buffer_ref = buffer.get_mut().unwrap();
            let mut relation = gst_analytics::AnalyticsRelationMeta::add(buffer_ref);
            relation
                .add_keypoint_mtd(
                    AnalyticsKeypointDimensions::_2d,
                    50,
                    50,
                    0,
                    AnalyticsKeypointVisibility::VISIBLE,
                    0.9,
                )
                .unwrap();
            relation
                .add_keypoint_mtd(
                    AnalyticsKeypointDimensions::_2d,
                    54,
                    52,
                    0,
                    AnalyticsKeypointVisibility::VISIBLE,
                    0.9,
                )
                .unwrap();
        }

        // Ungrouped mode (no semantic tag) draws a label per keypoint.
        let settings = Settings {
            draw_labels: true,
            ..Default::default()
        };
        let bounds = FrameBounds {
            width: 128,
            height: 128,
        };
        let (_, commands) = analytics_to_draw_commands(buffer.as_ref(), &settings, bounds);

        let markers: Vec<Rect> = commands
            .iter()
            .filter(|c| matches!(c, DrawCommand::Circle { .. }))
            .filter_map(crate::render::content_bounds)
            .collect();
        let labels: Vec<Rect> = commands
            .iter()
            .filter(|c| matches!(c, DrawCommand::TextCentered { .. }))
            .filter_map(crate::render::content_bounds)
            .collect();

        assert_eq!(markers.len(), 2);
        assert_eq!(labels.len(), 2);
        for label in &labels {
            for marker in &markers {
                assert!(
                    !label.intersects(*marker),
                    "label {label:?} overlaps keypoint marker {marker:?}"
                );
            }
        }
    }
}
