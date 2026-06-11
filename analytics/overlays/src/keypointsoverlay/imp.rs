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
    AnalyticsGroupMtd, AnalyticsKeypointMtd, AnalyticsMetaRefExt, AnalyticsRelationMeta, RelTypes,
};
use gst_video::prelude::VideoFrameExt;

use gst_base::prelude::BaseTransformExt;
use gst_base::subclass::prelude::*;
use gst_video::subclass::prelude::*;

use crate::geometry::{OccupiedRegionRegistry, Rect};
use crate::placement::{
    LabelPlacement, leader_endpoints, place_label, point_label_candidates, push_leader_line,
};
use crate::render::{
    AnalyticsFrame, DrawCommand, LABEL_LAYOUT_GAP, LABEL_LAYOUT_HEIGHT, RenderContext,
    measure_centered_label_text_width,
};

use std::sync::{LazyLock, Mutex};

/// Owner tag this element uses when claiming/reading shared regions.
const OVERLAY_OWNER: &str = "keypointsoverlay";

const DEFAULT_RENDER_ENABLED: bool = false;
const DEFAULT_KEYPOINT_COLOR: u32 = 0xFFFF_0000;
const DEFAULT_KEYPOINT_RADIUS: f64 = 3.0;
const DEFAULT_DRAW_LABELS: bool = true;
const DEFAULT_LABELS_COLOR: u32 = 0xFFFF_FFFF;
const DEFAULT_DRAW_SKELETON: bool = false;
const DEFAULT_SKELETON_COLOR: u32 = 0xFF00_FF00;
const DEFAULT_SKELETON_LINE_WIDTH: f64 = 2.0;

#[derive(Debug, Clone)]
struct Settings {
    render_enabled: bool,
    keypoint_color: u32,
    keypoint_radius: f64,
    draw_labels: bool,
    labels_color: u32,
    draw_skeleton: bool,
    skeleton_color: u32,
    skeleton_line_width: f64,
    semantic_tag: Option<String>,
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
struct FrameBounds {
    width: i32,
    height: i32,
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

/// Place a keypoint's confidence label: preferred just above the keypoint, with
/// the shared candidate ring as fallbacks.
fn place_keypoint_label(
    registry: &mut OccupiedRegionRegistry,
    sample: KeypointSample,
    text: &str,
) -> Option<LabelPlacement> {
    let label_w = measure_centered_label_text_width(text);
    let label_h = LABEL_LAYOUT_HEIGHT;

    // Default: centered horizontally, sitting just above the keypoint.
    let default = Rect::from_xywh(
        sample.x.saturating_sub(label_w / 2),
        sample
            .y
            .saturating_sub(label_h)
            .saturating_sub(LABEL_LAYOUT_GAP),
        label_w,
        label_h,
    );

    let candidates = point_label_candidates(sample.x, sample.y, label_w, label_h);

    place_label(registry, default, &candidates)
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
}

fn push_keypoint_commands(
    commands: &mut Vec<DrawCommand>,
    samples: &[KeypointSample],
    ctx: &mut KeypointCommandContext<'_>,
) {
    let mut first_in_frame: Option<KeypointSample> = None;
    let keypoint_radius_px = ctx.settings.keypoint_radius.ceil() as i32;

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

        if ctx.settings.draw_labels && !ctx.draw_group_label_once {
            let label = confidence_label(sample.confidence);
            if let Some(placement) = place_keypoint_label(ctx.occupied, *sample, &label) {
                push_keypoint_label(
                    commands,
                    placement,
                    *sample,
                    label,
                    ctx.settings.labels_color,
                );
            }
        }
    }

    if ctx.settings.draw_labels
        && ctx.draw_group_label_once
        && let Some(sample) = first_in_frame
    {
        let label = confidence_label(sample.confidence);
        if let Some(placement) = place_keypoint_label(ctx.occupied, sample, &label) {
            push_keypoint_label(
                commands,
                placement,
                sample,
                label,
                ctx.settings.labels_color,
            );
        }
    }

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
                });
            }
        }
    }
}

fn analytics_to_draw_commands(
    buffer: &gst::BufferRef,
    settings: &Settings,
    bounds: FrameBounds,
) -> (AnalyticsFrame<'static>, Vec<DrawCommand>) {
    let Some(meta) = buffer.meta::<AnalyticsRelationMeta>() else {
        return (AnalyticsFrame::default(), Vec::new());
    };

    let mut commands = Vec::new();
    let mut keypoint_count = 0usize;
    let mut occupied = OccupiedRegionRegistry::new(bounds.width, bounds.height);

    // Avoid regions other elements upstream have already claimed.
    crate::coordination::seed_registry_from_claims(&mut occupied, buffer, OVERLAY_OWNER);

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
            };
            push_keypoint_commands(&mut commands, &group_samples, &mut ctx);
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
    )
}

#[derive(Default)]
pub struct KeypointsOverlay {
    render_context: Mutex<RenderContext>,
    settings: Mutex<Settings>,
}

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "keypointsoverlay",
        gst::DebugColorFlags::empty(),
        Some("Keypoints overlay skeleton"),
    )
});

#[glib::object_subclass]
impl ObjectSubclass for KeypointsOverlay {
    const NAME: &'static str = "GstKeypointsOverlay";
    type Type = super::KeypointsOverlay;
    type ParentType = gst_video::VideoFilter;
}

impl ObjectImpl for KeypointsOverlay {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecBoolean::builder("render-enabled")
                    .nick("Render enabled")
                    .blurb("When false, element runs in passthrough mode")
                    .default_value(DEFAULT_RENDER_ENABLED)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecUInt::builder("keypoint-color")
                    .nick("Keypoint color")
                    .blurb("Color used to draw keypoints")
                    .minimum(0)
                    .maximum(u32::MAX)
                    .default_value(DEFAULT_KEYPOINT_COLOR)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecDouble::builder("keypoint-radius")
                    .nick("Keypoint radius")
                    .blurb("Radius in pixels used for keypoints")
                    .minimum(1.0)
                    .maximum(20.0)
                    .default_value(DEFAULT_KEYPOINT_RADIUS)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecBoolean::builder("draw-labels")
                    .nick("Draw labels")
                    .blurb("Draw keypoint confidence labels")
                    .default_value(DEFAULT_DRAW_LABELS)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecUInt::builder("labels-color")
                    .nick("Labels color")
                    .blurb("Color used for keypoint labels")
                    .minimum(0)
                    .maximum(u32::MAX)
                    .default_value(DEFAULT_LABELS_COLOR)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecBoolean::builder("draw-skeleton")
                    .nick("Draw skeleton")
                    .blurb("Draw skeleton relations between keypoints")
                    .default_value(DEFAULT_DRAW_SKELETON)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecUInt::builder("skeleton-color")
                    .nick("Skeleton color")
                    .blurb("Color used for skeleton lines")
                    .minimum(0)
                    .maximum(u32::MAX)
                    .default_value(DEFAULT_SKELETON_COLOR)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecDouble::builder("skeleton-line-width")
                    .nick("Skeleton line width")
                    .blurb("Line width in pixels for skeleton rendering")
                    .minimum(1.0)
                    .maximum(10.0)
                    .default_value(DEFAULT_SKELETON_LINE_WIDTH)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecString::builder("semantic-tag")
                    .nick("Semantic tag")
                    .blurb("Semantic tag prefix used to filter grouped keypoints")
                    .default_value(None)
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
            "keypoint-color" => {
                let mut settings = self.settings.lock().unwrap();
                settings.keypoint_color = value.get().expect("type checked upstream");
            }
            "keypoint-radius" => {
                let mut settings = self.settings.lock().unwrap();
                settings.keypoint_radius = value.get().expect("type checked upstream");
            }
            "draw-labels" => {
                let mut settings = self.settings.lock().unwrap();
                settings.draw_labels = value.get().expect("type checked upstream");
            }
            "labels-color" => {
                let mut settings = self.settings.lock().unwrap();
                settings.labels_color = value.get().expect("type checked upstream");
            }
            "draw-skeleton" => {
                let mut settings = self.settings.lock().unwrap();
                settings.draw_skeleton = value.get().expect("type checked upstream");
            }
            "skeleton-color" => {
                let mut settings = self.settings.lock().unwrap();
                settings.skeleton_color = value.get().expect("type checked upstream");
            }
            "skeleton-line-width" => {
                let mut settings = self.settings.lock().unwrap();
                settings.skeleton_line_width = value.get().expect("type checked upstream");
            }
            "semantic-tag" => {
                let mut settings = self.settings.lock().unwrap();
                settings.semantic_tag = value.get().expect("type checked upstream");
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
            "keypoint-color" => {
                let settings = self.settings.lock().unwrap();
                settings.keypoint_color.to_value()
            }
            "keypoint-radius" => {
                let settings = self.settings.lock().unwrap();
                settings.keypoint_radius.to_value()
            }
            "draw-labels" => {
                let settings = self.settings.lock().unwrap();
                settings.draw_labels.to_value()
            }
            "labels-color" => {
                let settings = self.settings.lock().unwrap();
                settings.labels_color.to_value()
            }
            "draw-skeleton" => {
                let settings = self.settings.lock().unwrap();
                settings.draw_skeleton.to_value()
            }
            "skeleton-color" => {
                let settings = self.settings.lock().unwrap();
                settings.skeleton_color.to_value()
            }
            "skeleton-line-width" => {
                let settings = self.settings.lock().unwrap();
                settings.skeleton_line_width.to_value()
            }
            "semantic-tag" => {
                let settings = self.settings.lock().unwrap();
                settings.semantic_tag.to_value()
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

impl GstObjectImpl for KeypointsOverlay {}

impl ElementImpl for KeypointsOverlay {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Keypoints Overlay (skeleton)",
                "Filter/Editor/Video",
                "Keypoints overlay skeleton with passthrough toggle",
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

impl BaseTransformImpl for KeypointsOverlay {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::AlwaysInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = false;
}

impl VideoFilterImpl for KeypointsOverlay {
    fn transform_frame_ip(
        &self,
        frame: &mut gst_video::VideoFrameRef<&mut gst::BufferRef>,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let settings = self.settings.lock().unwrap().clone();
        let bounds = FrameBounds {
            width: frame.width() as i32,
            height: frame.height() as i32,
        };
        let (analytics, commands) = analytics_to_draw_commands(frame.buffer(), &settings, bounds);

        let mut render_context = self.render_context.lock().unwrap();
        render_context.render(frame, &analytics, &commands)?;
        drop(render_context);

        // Publish what we drew so downstream overlays avoid occluding it.
        if !commands.is_empty() {
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
}
