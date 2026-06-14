// Copyright (C) 2026 Jeremy Whiting <jeremy.whiting@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

//! Pre/post custom drawing hooks.
//!
//! A host can inject its own drawing before and after an overlay element's
//! built-in content. A hook returns [`DrawCommand`]s which the element renders
//! through the same backend as its built-ins, so hosts share the overlay
//! drawing vocabulary without touching the rendering backend.
//!
//! Ordering is z-order: commands are drawn in sequence, so a **pre** hook draws
//! *beneath* the built-ins (but over the video) and a **post** hook draws *over*
//! everything:
//!
//! ```text
//! [ pre-hook commands ] [ built-in commands ] [ post-hook commands ]
//!   under built-ins        boxes/labels/...      over built-ins
//! ```
//!
//! Hooks are set per element instance via the [`OverlayDrawHooksExt`] trait,
//! callable on any `gst::Element` (so hosts need not name the concrete element
//! types).
//!
//! This is a Rust-facing API (a host using the elements as a library). A
//! language-agnostic variant — a GObject `draw` signal emitting a drawing
//! context, à la `cairooverlay` — would be the natural extension for C/Python
//! hosts; it is intentionally out of scope here.

use gst::prelude::*;
use gst::subclass::prelude::*;

use crate::render::DrawCommand;

/// Information about the frame a draw hook is rendering into.
#[derive(Debug, Clone, Copy)]
pub struct DrawHookContext {
    pub width: i32,
    pub height: i32,
}

/// A hook that emits draw commands before or after an element's built-in
/// overlay. Any `Fn(&DrawHookContext) -> Vec<DrawCommand>` is a `DrawHook`.
pub trait DrawHook: Send + Sync {
    fn draw(&self, ctx: &DrawHookContext) -> Vec<DrawCommand>;
}

impl<F> DrawHook for F
where
    F: Fn(&DrawHookContext) -> Vec<DrawCommand> + Send + Sync,
{
    fn draw(&self, ctx: &DrawHookContext) -> Vec<DrawCommand> {
        self(ctx)
    }
}

/// The pre/post draw hooks attached to an element.
#[derive(Default)]
pub struct DrawHooks {
    pre: Option<Box<dyn DrawHook>>,
    post: Option<Box<dyn DrawHook>>,
}

impl DrawHooks {
    pub fn set_pre(&mut self, hook: Box<dyn DrawHook>) {
        self.pre = Some(hook);
    }

    pub fn set_post(&mut self, hook: Box<dyn DrawHook>) {
        self.post = Some(hook);
    }

    pub fn clear(&mut self) {
        self.pre = None;
        self.post = None;
    }

    /// Build the final command list: pre-hook output, then the element's
    /// `builtins`, then post-hook output. Hooks that are unset contribute
    /// nothing, so with no hooks this is just `builtins`.
    pub fn compose(&self, builtins: &[DrawCommand], ctx: &DrawHookContext) -> Vec<DrawCommand> {
        let mut out = Vec::with_capacity(builtins.len());
        if let Some(hook) = &self.pre {
            out.extend(hook.draw(ctx));
        }
        out.extend_from_slice(builtins);
        if let Some(hook) = &self.post {
            out.extend(hook.draw(ctx));
        }
        out
    }
}

/// Sample hook: draws an unfilled rectangular border inset from the frame edges.
///
/// Doubles as documentation of the hook API — a host registers it with, e.g.:
///
/// ```ignore
/// element.set_post_draw_hook(BorderHook { argb: 0xFF00_FF00, inset: 4.0 });
/// ```
pub struct BorderHook {
    pub argb: u32,
    pub inset: f32,
}

impl DrawHook for BorderHook {
    fn draw(&self, ctx: &DrawHookContext) -> Vec<DrawCommand> {
        vec![DrawCommand::Rectangle {
            x: self.inset,
            y: self.inset,
            width: (ctx.width as f32 - 2.0 * self.inset).max(0.0),
            height: (ctx.height as f32 - 2.0 * self.inset).max(0.0),
            rotation: 0.0,
            argb: self.argb,
            filled: false,
        }]
    }
}

/// Set custom pre/post draw hooks on a hook-capable overlay element.
///
/// Implemented for any [`gst::Element`], so a host can call it on the element it
/// got from the factory without depending on the concrete element types. Calls
/// on elements that are not hook-capable overlays (currently `odoverlay` and
/// `keypointsoverlay`) are no-ops.
pub trait OverlayDrawHooksExt: IsA<gst::Element> {
    /// Inject custom drawing *beneath* the built-in overlay (over the video).
    fn set_pre_draw_hook(&self, hook: impl DrawHook + 'static);
    /// Inject custom drawing *over* the built-in overlay.
    fn set_post_draw_hook(&self, hook: impl DrawHook + 'static);
    /// Remove any previously set pre/post draw hooks.
    fn clear_draw_hooks(&self);
}

impl<O: IsA<gst::Element>> OverlayDrawHooksExt for O {
    fn set_pre_draw_hook(&self, hook: impl DrawHook + 'static) {
        let element = self.upcast_ref::<gst::Element>();
        let hook: Box<dyn DrawHook> = Box::new(hook);
        if let Some(od) =
            element.downcast_ref::<crate::objectdetectionoverlay::ObjectDetectionOverlay>()
        {
            od.imp().set_pre_draw_hook(hook);
        } else if let Some(kp) = element.downcast_ref::<crate::keypointsoverlay::KeypointsOverlay>()
        {
            kp.imp().set_pre_draw_hook(hook);
        }
    }

    fn set_post_draw_hook(&self, hook: impl DrawHook + 'static) {
        let element = self.upcast_ref::<gst::Element>();
        let hook: Box<dyn DrawHook> = Box::new(hook);
        if let Some(od) =
            element.downcast_ref::<crate::objectdetectionoverlay::ObjectDetectionOverlay>()
        {
            od.imp().set_post_draw_hook(hook);
        } else if let Some(kp) = element.downcast_ref::<crate::keypointsoverlay::KeypointsOverlay>()
        {
            kp.imp().set_post_draw_hook(hook);
        }
    }

    fn clear_draw_hooks(&self) {
        let element = self.upcast_ref::<gst::Element>();
        if let Some(od) =
            element.downcast_ref::<crate::objectdetectionoverlay::ObjectDetectionOverlay>()
        {
            od.imp().clear_draw_hooks();
        } else if let Some(kp) = element.downcast_ref::<crate::keypointsoverlay::KeypointsOverlay>()
        {
            kp.imp().clear_draw_hooks();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marker(x: f32) -> DrawCommand {
        DrawCommand::Circle {
            cx: x,
            cy: 0.0,
            radius: 1.0,
            argb: 0,
        }
    }

    #[test]
    fn no_hooks_returns_builtins_unchanged() {
        let hooks = DrawHooks::default();
        let builtins = vec![marker(1.0), marker(2.0)];
        let ctx = DrawHookContext {
            width: 100,
            height: 100,
        };
        assert_eq!(hooks.compose(&builtins, &ctx), builtins);
    }

    #[test]
    fn pre_hook_draws_before_and_post_hook_after_builtins() {
        let mut hooks = DrawHooks::default();
        hooks.set_pre(Box::new(|_: &DrawHookContext| vec![marker(0.0)]));
        hooks.set_post(Box::new(|_: &DrawHookContext| vec![marker(9.0)]));

        let builtins = vec![marker(5.0)];
        let ctx = DrawHookContext {
            width: 100,
            height: 100,
        };
        let composed = hooks.compose(&builtins, &ctx);

        // Order is pre, built-ins, post.
        assert_eq!(composed, vec![marker(0.0), marker(5.0), marker(9.0)]);
    }

    #[test]
    fn hook_receives_frame_dimensions() {
        let mut hooks = DrawHooks::default();
        hooks.set_post(Box::new(|ctx: &DrawHookContext| {
            vec![marker(ctx.width as f32)]
        }));

        let ctx = DrawHookContext {
            width: 640,
            height: 480,
        };
        let composed = hooks.compose(&[], &ctx);
        assert_eq!(composed, vec![marker(640.0)]);
    }

    #[test]
    fn clear_removes_both_hooks() {
        let mut hooks = DrawHooks::default();
        hooks.set_pre(Box::new(|_: &DrawHookContext| vec![marker(0.0)]));
        hooks.set_post(Box::new(|_: &DrawHookContext| vec![marker(9.0)]));
        hooks.clear();

        let builtins = vec![marker(5.0)];
        let ctx = DrawHookContext {
            width: 1,
            height: 1,
        };
        assert_eq!(hooks.compose(&builtins, &ctx), builtins);
    }

    #[test]
    fn border_hook_insets_from_the_frame_edges() {
        let hook = BorderHook {
            argb: 0xFF00_FF00,
            inset: 4.0,
        };
        let commands = hook.draw(&DrawHookContext {
            width: 100,
            height: 80,
        });

        assert_eq!(
            commands,
            vec![DrawCommand::Rectangle {
                x: 4.0,
                y: 4.0,
                width: 92.0,
                height: 72.0,
                rotation: 0.0,
                argb: 0xFF00_FF00,
                filled: false,
            }]
        );
    }
}
