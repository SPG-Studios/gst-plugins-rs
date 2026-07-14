// Copyright (C) 2026 Jeremy Whiting <jeremy.whiting@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

//! Shared machinery for the auto-selecting overlay bins.
//!
//! Each overlay ships as a CPU element (a `video/x-raw`, system-memory
//! [`gst_video::VideoFilter`]) and a GL twin (a `video/x-raw(memory:GLMemory)`
//! [`gst_gl::GLFilter`]). A pipeline author should not have to know which one to
//! plug: the bins here wrap both and pick the right child from the input memory
//! type, so `... ! odoverlaybin ! ...` "just works" whether the frames arrive in
//! system memory or on the GPU.
//!
//! The selection is **lazy and one-time**: the bin starts with untargeted ghost
//! pads whose template advertises the union of both children's caps, and a probe
//! on the sink pad catches the first `CAPS` event. From its memory features it
//! chooses the CPU or GL child, builds it, links the ghost pads to it and syncs
//! its state. Memory type is stable for the life of a stream in practice, so a
//! single decision suffices; no output bridging (glupload/gldownload) is inserted
//! — the chosen child keeps the input's memory type (GL→GL, sys→sys), matching the
//! underlying elements.
//!
//! Properties declared by the CPU twin are reconstructed onto the bin (see
//! [`forwarded`]) and forwarded to whichever child is active, so the bin exposes
//! the same flat property API as the plain elements. Cached values live in a
//! [`gst::Structure`] (whose `SendValue`s cross to the streaming-thread probe).

use gst::glib;
use gst::prelude::*;
use std::sync::{Arc, LazyLock, Mutex};

/// The GL memory caps feature. Only referenced in a `gl` build: that is the only
/// configuration where the template advertises it and where the child selection
/// has a GL path to choose.
#[cfg(feature = "gl")]
const GL_MEMORY_FEATURE: &str = "memory:GLMemory";

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "overlayautobin",
        gst::DebugColorFlags::empty(),
        Some("Auto-selecting overlay bin"),
    )
});

/// Pad-template caps: the union of the CPU child's system-memory `video/x-raw`
/// and (when built with GL support) the GL child's `video/x-raw(memory:GLMemory)`.
/// System memory is listed first so a pipeline that can offer either negotiates
/// the CPU path by default (the GL path needs a GL context to be useful).
fn template_caps() -> gst::Caps {
    let mut caps = gst::Caps::new_empty();
    {
        let caps = caps.get_mut().unwrap();
        caps.append_structure(gst::Structure::builder("video/x-raw").build());
        #[cfg(feature = "gl")]
        caps.append_structure_full(
            gst::Structure::builder("video/x-raw").build(),
            Some(gst::CapsFeatures::new([GL_MEMORY_FEATURE])),
        );
    }
    caps
}

/// The always-present sink and src pad templates shared by every overlay bin.
pub fn pad_templates() -> &'static [gst::PadTemplate] {
    static TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
        let caps = template_caps();
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
    TEMPLATES.as_ref()
}

/// Reconstruct an equivalent [`glib::ParamSpec`] for `pspec` (so it can be
/// installed on the bin class) together with its default value as a `SendValue`.
/// Returns `None` for value types the overlays don't use, which are then simply
/// not forwarded.
fn clone_param_spec(pspec: &glib::ParamSpec) -> Option<(glib::ParamSpec, glib::SendValue)> {
    let name = pspec.name();
    let nick = pspec.nick();
    let blurb = pspec.blurb().unwrap_or(nick);
    let flags = pspec.flags();

    if let Some(p) = pspec.downcast_ref::<glib::ParamSpecBoolean>() {
        let def = p.default_value();
        let spec = glib::ParamSpecBoolean::builder(name)
            .nick(nick)
            .blurb(blurb)
            .default_value(def)
            .flags(flags)
            .build();
        return Some((spec, def.to_send_value()));
    }
    if let Some(p) = pspec.downcast_ref::<glib::ParamSpecInt>() {
        let def = p.default_value();
        let spec = glib::ParamSpecInt::builder(name)
            .nick(nick)
            .blurb(blurb)
            .minimum(p.minimum())
            .maximum(p.maximum())
            .default_value(def)
            .flags(flags)
            .build();
        return Some((spec, def.to_send_value()));
    }
    if let Some(p) = pspec.downcast_ref::<glib::ParamSpecUInt>() {
        let def = p.default_value();
        let spec = glib::ParamSpecUInt::builder(name)
            .nick(nick)
            .blurb(blurb)
            .minimum(p.minimum())
            .maximum(p.maximum())
            .default_value(def)
            .flags(flags)
            .build();
        return Some((spec, def.to_send_value()));
    }
    if let Some(p) = pspec.downcast_ref::<glib::ParamSpecUInt64>() {
        let def = p.default_value();
        let spec = glib::ParamSpecUInt64::builder(name)
            .nick(nick)
            .blurb(blurb)
            .minimum(p.minimum())
            .maximum(p.maximum())
            .default_value(def)
            .flags(flags)
            .build();
        return Some((spec, def.to_send_value()));
    }
    if let Some(p) = pspec.downcast_ref::<glib::ParamSpecDouble>() {
        let def = p.default_value();
        let spec = glib::ParamSpecDouble::builder(name)
            .nick(nick)
            .blurb(blurb)
            .minimum(p.minimum())
            .maximum(p.maximum())
            .default_value(def)
            .flags(flags)
            .build();
        return Some((spec, def.to_send_value()));
    }
    if let Some(p) = pspec.downcast_ref::<glib::ParamSpecString>() {
        let def = p.default_value().map(|s| s.to_string());
        let spec = glib::ParamSpecString::builder(name)
            .nick(nick)
            .blurb(blurb)
            .default_value(def.as_deref())
            .flags(flags)
            .build();
        return Some((spec, def.to_send_value()));
    }

    gst::warning!(
        CAT,
        "not forwarding property '{}' of unsupported type {}",
        name,
        pspec.value_type()
    );
    None
}

/// Build the forwarded property specs (and a [`gst::Structure`] of their default
/// values) for a bin from its CPU child factory. Only properties *declared by the
/// child element itself* are forwarded — base-class properties (`qos`, `name`, …)
/// are left to the parent classes. The specs are installed via
/// `ObjectImpl::properties`; the defaults structure backs reads made before a
/// child is selected.
pub fn forwarded(cpu_factory: &str) -> (Vec<glib::ParamSpec>, gst::Structure) {
    let mut specs = Vec::new();
    let mut defaults = gst::Structure::new_empty("overlay-bin-defaults");

    let child = match gst::ElementFactory::make(cpu_factory).build() {
        Ok(child) => child,
        Err(err) => {
            gst::warning!(
                CAT,
                "cannot introspect '{cpu_factory}' to forward its properties: {err}"
            );
            return (specs, defaults);
        }
    };

    let child_type = child.type_();
    for pspec in child.list_properties() {
        // Only forward properties the overlay element itself declares.
        if pspec.owner_type() != child_type {
            continue;
        }
        if let Some((spec, default)) = clone_param_spec(&pspec) {
            defaults.set_value(spec.name(), default);
            specs.push(spec);
        }
    }

    (specs, defaults)
}

/// Mutable shared state, held behind an `Arc` so the sink-pad probe (which runs
/// on the streaming thread) and the property accessors (which run on any thread)
/// operate on the same data.
struct Inner {
    cpu_factory: &'static str,
    /// Only consulted in a `gl` build, where the sink caps may be GL memory.
    #[cfg_attr(not(feature = "gl"), allow(dead_code))]
    gl_factory: &'static str,
    child: Mutex<Option<gst::Element>>,
    /// Property values explicitly set on the bin, forwarded to the child when it
    /// is built. Only user overrides are stored (the child keeps its own defaults).
    overrides: Mutex<gst::Structure>,
    /// Default values, used only for reads made before the child is selected.
    defaults: gst::Structure,
}

/// The reusable core of an auto-selecting overlay bin. A concrete bin embeds one
/// of these and delegates its `ObjectImpl`/`ElementImpl` hooks to it.
pub struct AutoOverlayBin {
    sinkpad: gst::GhostPad,
    srcpad: gst::GhostPad,
    inner: Arc<Inner>,
}

impl AutoOverlayBin {
    /// Create the bin state: two untargeted ghost pads and the property defaults
    /// (from [`forwarded`]) used for reads before a child exists.
    pub fn new(
        sinkpad: gst::GhostPad,
        srcpad: gst::GhostPad,
        cpu_factory: &'static str,
        gl_factory: &'static str,
        defaults: gst::Structure,
    ) -> Self {
        Self {
            sinkpad,
            srcpad,
            inner: Arc::new(Inner {
                cpu_factory,
                gl_factory,
                child: Mutex::new(None),
                overrides: Mutex::new(gst::Structure::new_empty("overlay-bin-overrides")),
                defaults,
            }),
        }
    }

    /// Add the ghost pads and arrange for the child to be built. Call from the
    /// concrete bin's `ObjectImpl::constructed`.
    ///
    /// Without GL support there is only ever one possible input memory type
    /// (system memory), so there is nothing to select: the CPU child is built
    /// eagerly here, making the bin a transparent wrapper and avoiding adding an
    /// element from the streaming thread. With GL support the choice is deferred
    /// to the first CAPS event, where the input memory type is known.
    pub fn constructed(&self, obj: &gst::Bin) {
        obj.add_pad(&self.sinkpad).unwrap();
        obj.add_pad(&self.srcpad).unwrap();

        #[cfg(not(feature = "gl"))]
        {
            let factory = self.inner.cpu_factory;
            if let Err(err) =
                install_child(obj, &self.inner, &self.sinkpad, &self.srcpad, factory)
            {
                gst::element_error!(
                    obj,
                    gst::CoreError::Failed,
                    ["failed to build overlay child '{factory}': {err}"]
                );
            }
        }

        #[cfg(feature = "gl")]
        {
            let inner = self.inner.clone();
            let sinkpad = self.sinkpad.clone();
            let srcpad = self.srcpad.clone();
            let obj_weak = obj.downgrade();

            self.sinkpad
                .add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_pad, info| {
                    let Some(event) = info.event() else {
                        return gst::PadProbeReturn::Ok;
                    };
                    let gst::EventView::Caps(caps_event) = event.view() else {
                        return gst::PadProbeReturn::Ok;
                    };
                    let caps = caps_event.caps_owned();
                    let Some(obj) = obj_weak.upgrade() else {
                        return gst::PadProbeReturn::Ok;
                    };

                    let is_gl = caps
                        .features(0)
                        .is_some_and(|features| features.contains(GL_MEMORY_FEATURE));
                    let factory = if is_gl {
                        inner.gl_factory
                    } else {
                        inner.cpu_factory
                    };

                    gst::info!(
                        CAT,
                        obj = obj,
                        "selecting '{factory}' for {} input",
                        if is_gl { "GL" } else { "system-memory" }
                    );

                    match install_child(&obj, &inner, &sinkpad, &srcpad, factory) {
                        Ok(()) => gst::PadProbeReturn::Remove,
                        Err(err) => {
                            gst::element_error!(
                                obj,
                                gst::CoreError::Negotiation,
                                ["failed to select an overlay child for caps {caps}: {err}"]
                            );
                            gst::PadProbeReturn::Drop
                        }
                    }
                });
        }
    }

    /// Forward a property set to the active child, caching it so a child built
    /// later is configured consistently.
    pub fn set_property(&self, name: &str, value: &glib::Value) {
        // SAFETY: only properties this bin installed reach here, and every one is
        // a `Send` scalar type (bool/int/uint/uint64/double/string).
        let send_value = unsafe { value.clone().into_send_value() };
        self.inner
            .overrides
            .lock()
            .unwrap()
            .set_value(name, send_value);

        if let Some(child) = self.inner.child.lock().unwrap().as_ref() {
            child.set_property_from_value(name, value);
        }
    }

    /// Read a property from the active child, or the override/default cache if no
    /// child exists yet.
    pub fn property(&self, name: &str) -> glib::Value {
        if let Some(child) = self.inner.child.lock().unwrap().as_ref() {
            return child.property_value(name);
        }
        if let Ok(value) = self.inner.overrides.lock().unwrap().value(name) {
            return value.to_value();
        }
        self.inner
            .defaults
            .value(name)
            .map(|v| v.to_value())
            .unwrap_or_else(|_| glib::Value::from_type(glib::Type::UNIT))
    }
}

/// Build, add, configure and link `factory` as the bin's child. Guarded so it
/// runs at most once — the GL caps probe may fire on a redundant caps event, and
/// the eager path runs exactly once anyway.
fn install_child(
    obj: &gst::Bin,
    inner: &Arc<Inner>,
    sinkpad: &gst::GhostPad,
    srcpad: &gst::GhostPad,
    factory: &str,
) -> Result<(), glib::BoolError> {
    let mut child_slot = inner.child.lock().unwrap();
    if child_slot.is_some() {
        // Already selected; nothing to do.
        return Ok(());
    }

    let child = gst::ElementFactory::make(factory)
        .build()
        .map_err(|_| glib::bool_error!("overlay child factory '{}' is not available", factory))?;

    // Apply the user-set property values before the child goes live so it is
    // configured identically to the plain element.
    for (name, value) in inner.overrides.lock().unwrap().iter() {
        if child.find_property(name).is_some() {
            child.set_property_from_value(name, value);
        }
    }

    obj.add(&child)?;

    let child_sink = child
        .static_pad("sink")
        .ok_or_else(|| glib::bool_error!("overlay child has no sink pad"))?;
    let child_src = child
        .static_pad("src")
        .ok_or_else(|| glib::bool_error!("overlay child has no src pad"))?;

    sinkpad.set_target(Some(&child_sink))?;
    srcpad.set_target(Some(&child_src))?;

    child.sync_state_with_parent()?;

    *child_slot = Some(child);
    Ok(())
}
