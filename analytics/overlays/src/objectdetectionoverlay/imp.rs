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
use gst_analytics::AnalyticsRelationMeta;
use gst_video::prelude::VideoFrameExt;

use gst_base::prelude::{BaseTransformExt, BaseTransformExtManual};
use gst_base::subclass::prelude::*;
use gst_video::subclass::prelude::*;

use crate::lifecycle::{LifecycleEventKind, OverlayLifecycle, lifecycle_event_kind};
use crate::render::{AnalyticsFrame, DrawCommand, RenderContext};

use super::commands::{
    DEFAULT_DEFER_LABELS, DEFAULT_DRAW_LABELS, DEFAULT_DRAW_TRACKING_LABELS,
    DEFAULT_EXPIRE_OVERLAY, DEFAULT_FILLED_BOX, DEFAULT_LABELS_COLOR,
    DEFAULT_OBJECT_DETECTION_OUTLINE_COLOR, DEFAULT_RENDER_ENABLED,
    DEFAULT_SUPPRESS_BUILTIN_RENDERING, DEFAULT_TRACKING_OUTLINE_COLORS, FrameBounds,
    OVERLAY_OWNER, Settings, analytics_to_overlay,
};

use std::sync::{LazyLock, Mutex};

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
                glib::ParamSpecBoolean::builder("defer-labels")
                    .nick("Defer labels")
                    .blurb(
                        "Emit labels as deferred intents for a downstream overlaycompositor \
                         (which relocates them globally by priority) instead of placing them \
                         here. Requires a compositor downstream, or the labels are not drawn.",
                    )
                    .default_value(DEFAULT_DEFER_LABELS)
                    .mutable_playing()
                    .build(),
                crate::coordination::priority_param_spec(),
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
            "priority" => {
                let mut settings = self.settings.lock().unwrap();
                settings.priority = value.get().expect("type checked upstream");
            }
            "defer-labels" => {
                let mut settings = self.settings.lock().unwrap();
                settings.defer_labels = value.get().expect("type checked upstream");
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
            "priority" => {
                let settings = self.settings.lock().unwrap();
                settings.priority.to_value()
            }
            "defer-labels" => {
                let settings = self.settings.lock().unwrap();
                settings.defer_labels.to_value()
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

        let (analytics, commands, deferred_labels) = if has_meta {
            let (analytics, commands, deferred_labels) = analytics_to_overlay(
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
            (analytics, commands, deferred_labels)
        } else {
            let mut cache = self.overlay_cache.lock().unwrap();
            if should_reuse_cached_overlay(
                running_time,
                cache.last_update_running_time,
                settings.expire_overlay,
            ) {
                (cache.analytics.clone(), cache.commands.clone(), Vec::new())
            } else {
                cache.analytics = AnalyticsFrame::default();
                cache.commands.clear();
                cache.last_update_running_time = None;
                (AnalyticsFrame::default(), Vec::new(), Vec::new())
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
            crate::coordination::claim_commands(
                buffer,
                &commands,
                OVERLAY_OWNER,
                settings.priority,
            );
        }

        // In defer mode, hand our labels to a downstream compositor to place and
        // render (we drew only the anchored content above).
        if !deferred_labels.is_empty() {
            // SAFETY: the frame is writable and uniquely borrowed here.
            let buffer = unsafe { gst::BufferRef::from_mut_ptr((*frame.as_mut_ptr()).buffer) };
            crate::overlay_intent::add_label_intents(buffer, &deferred_labels);
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
}
