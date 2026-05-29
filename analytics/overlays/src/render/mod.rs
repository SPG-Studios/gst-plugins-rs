// Copyright (C) 2026 Jeremy Whiting <jeremy.whiting@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

#![allow(dead_code)]

use gst::BufferRef;
use gst_video::VideoFrameRef;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum RenderBackendKind {
    #[default]
    Skia,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct AnalyticsFrame<'a> {
    pub object_count: usize,
    pub segment_count: usize,
    pub keypoint_count: usize,
    pub semantic_tag: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum DrawCommand {
    NoOp,
    Rectangle {
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        argb: u32,
        filled: bool,
    },
    Circle {
        cx: f32,
        cy: f32,
        radius: f32,
        argb: u32,
    },
    Line {
        x0: f32,
        y0: f32,
        x1: f32,
        y1: f32,
        argb: u32,
        width: f32,
    },
    Text {
        x: f32,
        y: f32,
        text: String,
        argb: u32,
    },
}

trait RenderBackend {
    fn render_frame(
        &mut self,
        frame: &mut VideoFrameRef<&mut BufferRef>,
        analytics: &AnalyticsFrame,
        commands: &[DrawCommand],
    ) -> Result<(), gst::FlowError>;
}

#[derive(Debug, Default)]
struct SkiaBackend;

impl RenderBackend for SkiaBackend {
    fn render_frame(
        &mut self,
        frame: &mut VideoFrameRef<&mut BufferRef>,
        analytics: &AnalyticsFrame,
        commands: &[DrawCommand],
    ) -> Result<(), gst::FlowError> {
        // Placeholder backend. This keeps the render abstraction in place while
        // we migrate overlay drawing logic behind DrawCommand.
        let _ = frame;
        let _ = analytics;
        let _ = commands;
        Ok(())
    }
}

#[derive(Debug, Default)]
pub(crate) struct RenderContext {
    backend_kind: RenderBackendKind,
    skia: SkiaBackend,
}

impl RenderContext {
    pub(crate) fn new(backend_kind: RenderBackendKind) -> Self {
        Self {
            backend_kind,
            skia: SkiaBackend,
        }
    }

    pub(crate) fn backend_kind(&self) -> RenderBackendKind {
        self.backend_kind
    }

    pub(crate) fn render(
        &mut self,
        frame: &mut VideoFrameRef<&mut BufferRef>,
        analytics: &AnalyticsFrame,
        commands: &[DrawCommand],
    ) -> Result<(), gst::FlowError> {
        match self.backend_kind {
            RenderBackendKind::Skia => self.skia.render_frame(frame, analytics, commands),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_context_defaults_to_skia_backend() {
        let context = RenderContext::default();
        assert_eq!(context.backend_kind(), RenderBackendKind::Skia);
    }

    #[test]
    fn analytics_frame_defaults_to_empty() {
        let frame = AnalyticsFrame::default();

        assert_eq!(frame.object_count, 0);
        assert_eq!(frame.segment_count, 0);
        assert_eq!(frame.keypoint_count, 0);
        assert_eq!(frame.semantic_tag, None);
    }

    #[test]
    fn draw_command_variants_are_constructible() {
        let commands = [
            DrawCommand::NoOp,
            DrawCommand::Rectangle {
                x: 1.0,
                y: 2.0,
                width: 10.0,
                height: 20.0,
                argb: 0xFFFF_FFFF,
                filled: false,
            },
            DrawCommand::Circle {
                cx: 3.0,
                cy: 4.0,
                radius: 5.0,
                argb: 0xFFFF_0000,
            },
            DrawCommand::Line {
                x0: 0.0,
                y0: 0.0,
                x1: 10.0,
                y1: 10.0,
                argb: 0xFF00_FF00,
                width: 2.0,
            },
            DrawCommand::Text {
                x: 8.0,
                y: 9.0,
                text: "hello".to_string(),
                argb: 0xFFFF_FFFF,
            },
        ];

        assert_eq!(commands.len(), 5);
    }
}
