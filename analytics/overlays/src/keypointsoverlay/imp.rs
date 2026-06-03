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

use crate::render::{AnalyticsFrame, DrawCommand, RenderContext};

use std::sync::{LazyLock, Mutex};

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

fn push_keypoint_commands(
    commands: &mut Vec<DrawCommand>,
    samples: &[KeypointSample],
    settings: &Settings,
    draw_skeletons: bool,
    draw_group_label_once: bool,
    meta: &gst::MetaRef<'_, AnalyticsRelationMeta>,
    bounds: FrameBounds,
) {
    let mut first_in_frame: Option<KeypointSample> = None;

    for sample in samples {
        if !keypoint_in_frame(bounds, sample.x, sample.y) {
            continue;
        }

        if first_in_frame.is_none() {
            first_in_frame = Some(*sample);
        }

        commands.push(DrawCommand::Circle {
            cx: sample.x as f32,
            cy: sample.y as f32,
            radius: settings.keypoint_radius as f32,
            argb: settings.keypoint_color,
        });

        if settings.draw_labels && !draw_group_label_once {
            commands.push(DrawCommand::TextCentered {
                x: sample.x as f32 + settings.keypoint_radius as f32,
                y: sample.y as f32,
                text: confidence_label(sample.confidence),
                argb: settings.labels_color,
            });
        }
    }

    if settings.draw_labels
        && draw_group_label_once
        && let Some(sample) = first_in_frame
    {
        commands.push(DrawCommand::TextCentered {
            x: sample.x as f32 + settings.keypoint_radius as f32,
            y: sample.y as f32,
            text: confidence_label(sample.confidence),
            argb: settings.labels_color,
        });
    }

    if !draw_skeletons || !settings.draw_skeleton {
        return;
    }

    for sample in samples {
        for related in
            meta.iter_direct_related::<AnalyticsKeypointMtd>(sample.id, RelTypes::RELATE_TO)
        {
            if sample.id >= related.id() {
                continue;
            }

            if let Some((x2, y2)) = keypoint_position(&related) {
                let (x0, y0) = clamp_to_frame(bounds, sample.x, sample.y);
                let (x1, y1) = clamp_to_frame(bounds, x2, y2);

                commands.push(DrawCommand::Line {
                    x0: x0 as f32,
                    y0: y0 as f32,
                    x1: x1 as f32,
                    y1: y1 as f32,
                    argb: settings.skeleton_color,
                    width: settings.skeleton_line_width as f32,
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
            push_keypoint_commands(
                &mut commands,
                &group_samples,
                settings,
                true,
                true,
                &meta,
                bounds,
            );
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
        push_keypoint_commands(
            &mut commands,
            &samples,
            settings,
            false,
            false,
            &meta,
            bounds,
        );
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
}
