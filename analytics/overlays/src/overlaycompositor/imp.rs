// Copyright (C) 2026 Jeremy Whiting <jeremy.whiting@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

//! Deferred-label compositor (deferred label rendering).
//!
//! Upstream overlay elements running in defer mode render their anchored content
//! (boxes/keypoints/masks) in place and *claim* it, but emit their labels as
//! [`LabelIntent`](crate::overlay_intent::LabelIntent)s instead of placing them.
//! This element collects every element's intents and claims, places all the
//! labels globally — highest priority first, relocating lower-priority labels
//! around anchored content and around higher-priority labels — and renders them
//! once.
//!
//! NOTE: a deferring element with no compositor downstream loses its labels, so
//! a pipeline that uses defer mode must terminate the overlay chain with this
//! element.

use gst::glib;
use gst::subclass::prelude::*;
use gst_base::subclass::prelude::*;
use gst_video::prelude::*;
use gst_video::subclass::prelude::*;

use std::sync::{LazyLock, Mutex};

use crate::coordination::claimed_regions;
use crate::overlay_intent::label_intents;
use crate::placement::place_labels;
use crate::render::{AnalyticsFrame, DrawCommand, RenderContext};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "overlaycompositor",
        gst::DebugColorFlags::empty(),
        Some("Deferred overlay-label compositor"),
    )
});

#[derive(Default)]
pub struct OverlayCompositor {
    render_context: Mutex<RenderContext>,
}

impl OverlayCompositor {
    /// Render `commands` into a transparent overlay buffer for blending onto the
    /// frame. Mirrors the overlay elements' compositing path.
    fn build_overlay_composition(
        &self,
        width: u32,
        height: u32,
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
            // `Buffer::with_size` is uninitialized; clear to transparent so only
            // the drawn labels are blended onto the frame.
            if let Ok(data) = frame.plane_data_mut(0) {
                data.fill(0);
            }
            self.render_context
                .lock()
                .unwrap()
                .render(&mut frame, &AnalyticsFrame::default(), commands)
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

#[glib::object_subclass]
impl ObjectSubclass for OverlayCompositor {
    const NAME: &'static str = "GstRsOverlayCompositor";
    type Type = super::OverlayCompositor;
    type ParentType = gst_video::VideoFilter;
}

impl ObjectImpl for OverlayCompositor {}
impl GstObjectImpl for OverlayCompositor {}

impl ElementImpl for OverlayCompositor {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Overlay Label Compositor",
                "Filter/Editor/Video",
                "Places and renders deferred overlay labels by priority",
                "Jeremy Whiting <jeremy.whiting@collabora.com>",
            )
        });
        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let caps = gst::Caps::builder("video/x-raw").build();
            let src = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();
            let sink = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();
            vec![src, sink]
        });
        PAD_TEMPLATES.as_ref()
    }
}

impl BaseTransformImpl for OverlayCompositor {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::AlwaysInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = false;
}

impl VideoFilterImpl for OverlayCompositor {
    fn transform_frame_ip(
        &self,
        frame: &mut gst_video::VideoFrameRef<&mut gst::BufferRef>,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let buffer = frame.buffer();
        let intents = label_intents(buffer);
        if intents.is_empty() {
            return Ok(gst::FlowSuccess::Ok);
        }
        let claims = claimed_regions(buffer);
        let width = frame.width();
        let height = frame.height();

        let commands = place_labels(width as i32, height as i32, &claims, &intents);
        if let Some(composition) = self.build_overlay_composition(width, height, &commands) {
            composition.blend(frame).map_err(|_| {
                gst::error!(CAT, imp = self, "failed to blend deferred labels");
                gst::FlowError::Error
            })?;
        }
        Ok(gst::FlowSuccess::Ok)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordination::ClaimedRegion;
    use crate::geometry::Rect;
    use crate::overlay_intent::{CandidateKind, LabelIntent};

    fn box_label(text: &str, anchor: Rect, preferred: Rect, priority: i32) -> LabelIntent {
        LabelIntent {
            text: text.to_string(),
            color: 0xFFFF_FFFF,
            anchor,
            preferred,
            kind: CandidateKind::Box,
            priority,
            owner: "test".to_string(),
        }
    }

    fn label_rect(command: &DrawCommand) -> Option<(f32, f32)> {
        match command {
            DrawCommand::Text { x, y, .. } => Some((*x, *y)),
            DrawCommand::TextCentered { x, y, .. } => Some((*x, *y)),
            _ => None,
        }
    }

    fn count_leaders(commands: &[DrawCommand]) -> usize {
        commands
            .iter()
            .filter(|c| matches!(c, DrawCommand::Line { .. }))
            .count()
    }

    #[test]
    fn places_at_preferred_when_clear() {
        let preferred = Rect::from_xywh(10, 0, 40, 12);
        let anchor = Rect::from_xywh(10, 12, 40, 30);
        let intents = vec![box_label("person", anchor, preferred, 5)];

        let commands = place_labels(200, 200, &[], &intents);

        // No claims, no other labels: placed at preferred (bottom-left), no leader.
        assert_eq!(count_leaders(&commands), 0);
        assert_eq!(label_rect(&commands[0]), Some((10.0, 12.0)));
    }

    #[test]
    fn relocates_around_higher_priority_claim() {
        let preferred = Rect::from_xywh(10, 0, 40, 12);
        let anchor = Rect::from_xywh(10, 12, 40, 30);
        let intents = vec![box_label("person", anchor, preferred, 5)];
        // A higher-priority claim covering the preferred position.
        let claims = vec![ClaimedRegion::occlude(preferred, "keypointsoverlay", 9)];

        let commands = place_labels(200, 200, &claims, &intents);

        // The label is moved off the claimed spot and a leader line is drawn.
        assert_eq!(count_leaders(&commands), 1);
        assert_ne!(label_rect(commands.last().unwrap()), Some((10.0, 12.0)));
    }

    #[test]
    fn overdraws_lower_priority_claim() {
        let preferred = Rect::from_xywh(10, 0, 40, 12);
        let anchor = Rect::from_xywh(10, 12, 40, 30);
        let intents = vec![box_label("person", anchor, preferred, 5)];
        // A lower-priority claim is ignored (not avoided), so the label stays put.
        let claims = vec![ClaimedRegion::occlude(preferred, "labeloverlay", 1)];

        let commands = place_labels(200, 200, &claims, &intents);

        assert_eq!(count_leaders(&commands), 0);
        assert_eq!(label_rect(&commands[0]), Some((10.0, 12.0)));
    }

    #[test]
    fn two_labels_do_not_overlap() {
        // Two equal-priority labels whose preferred rects coincide.
        let preferred = Rect::from_xywh(10, 0, 40, 12);
        let anchor = Rect::from_xywh(10, 12, 40, 30);
        let intents = vec![
            box_label("a", anchor, preferred, 5),
            box_label("b", anchor, preferred, 5),
        ];

        let commands = place_labels(200, 200, &[], &intents);

        // First takes the preferred spot; the second is relocated (one leader).
        assert_eq!(count_leaders(&commands), 1);
    }
}
