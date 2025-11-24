// Copyright (C) 2025 Axel Tobieson <axel.tobieson@spiideo.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use anyhow::{anyhow, Result};
use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "parsebinloop",
        gst::DebugColorFlags::empty(),
        Some("Loops source input"),
    )
});

struct PadData {
    identity: gst::Element,
    ghost_pad: gst::GhostPad,
}

#[derive(Default)]
pub struct ParsebinLoop {
    pad_map: Arc<Mutex<HashMap<String, PadData>>>,
    parsebin: Mutex<Option<gst::Element>>,
}

#[glib::object_subclass]
impl ObjectSubclass for ParsebinLoop {
    const NAME: &'static str = "ParsebinLoop";
    type Type = super::ParsebinLoop;
    type ParentType = gst::Bin;

    fn new() -> Self {
        Self {
            pad_map: Arc::new(Mutex::new(HashMap::new())),
            parsebin: Mutex::new(None),
        }
    }
}

impl ObjectImpl for ParsebinLoop {
    fn constructed(&self) {
        self.parent_constructed();
        gst::debug!(CAT, imp = self, "Constructing ParsebinLoop plugin");

        // Create and add sink pad
        let template = self.obj().pad_template("sink").unwrap();
        let sink_pad = gst::GhostPad::builder_from_template(&template)
            .name("sink")
            .build();
        sink_pad.set_active(true).unwrap();

        if let Err(e) = self.obj().add_pad(&sink_pad) {
            gst::error!(CAT, imp = self, "Failed to add sink pad: {}", e);
        }

        match gst::ElementFactory::make("parsebin").build() {
            Ok(parsebin) => {
                if let Err(e) = self.obj().add(&parsebin) {
                    gst::error!(CAT, imp = self, "Failed to add parsebin: {}", e);
                    return;
                }

                *self.parsebin.lock().unwrap() = Some(parsebin);
            }
            Err(e) => {
                gst::error!(CAT, imp = self, "Failed to create parsebin: {}", e);
            }
        }
    }
}

impl GstObjectImpl for ParsebinLoop {}

impl ElementImpl for ParsebinLoop {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "Loops pull-based source input for all accepted parsebin formats",
                "Generic",
                "Loops source input",
                "Spiideo",
            )
        });
        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let caps = gst::Caps::new_any();
            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            let src_pad_template = gst::PadTemplate::new(
                "src_%u",
                gst::PadDirection::Src,
                gst::PadPresence::Sometimes,
                &caps,
            )
            .unwrap();

            vec![sink_pad_template, src_pad_template]
        });
        PAD_TEMPLATES.as_ref()
    }

    fn change_state(
        &self,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst::debug!(CAT, imp = self, "Changing state: {:?}", transition);
        match transition {
            gst::StateChange::PausedToReady => {
                gst::info!(CAT, imp = self, "Unpreparing plugin");
                if let Err(e) = self.stop() {
                    gst::error!(CAT, imp = self, "Failed to unprepare plugin: {}", e);
                }
            }
            gst::StateChange::ReadyToPaused => {
                gst::info!(CAT, imp = self, "Preparing plugin");
                if let Err(e) = self.start() {
                    gst::error!(CAT, imp = self, "Failed to prepare plugin: {}", e);
                }
            }
            _ => {}
        }
        self.parent_change_state(transition)
    }
}

impl BinImpl for ParsebinLoop {}

impl ParsebinLoop {
    fn start(&self) -> Result<()> {
        let obj = self.obj();
        let parsebin = self
            .parsebin
            .lock()
            .unwrap()
            .as_ref()
            .ok_or_else(|| anyhow!("Parsebin not created"))?
            .clone();

        // Connect pad-added signal handler
        let pad_map = self.pad_map.clone();
        parsebin.connect_pad_added(move |parsebin, src_pad| {
            if let Err(e) = Self::handle_pad_added_impl(parsebin, src_pad, &pad_map) {
                gst::error!(CAT, "Failed to handle pad added: {}", e);
            }
        });

        if let Some(sink_pad) = obj.static_pad("sink") {
            let ghost_pad = sink_pad
                .downcast_ref::<gst::GhostPad>()
                .ok_or_else(|| anyhow!("Sink pad is not a GhostPad"))?;
            let parsebin_sink = parsebin
                .static_pad("sink")
                .ok_or_else(|| anyhow!("Failed to get parsebin sink pad"))?;
            ghost_pad.set_target(Some(&parsebin_sink))?;
        }

        parsebin.sync_state_with_parent()?;
        gst::info!(CAT, imp = self, "Parsebin started successfully");
        Ok(())
    }

    fn stop(&self) -> Result<()> {
        let obj = self.obj();
        let mut pad_map = self
            .pad_map
            .lock()
            .map_err(|_| anyhow!("Failed to lock pad map"))?;

        for (_, pad_data) in pad_map.drain() {
            gst::info!(
                CAT,
                imp = self,
                "Unpreparing pad: {}",
                pad_data.ghost_pad.name()
            );
            pad_data.ghost_pad.set_target(None::<&gst::Pad>)?;
            pad_data.identity.set_state(gst::State::Null)?;

            obj.remove_pad(&pad_data.ghost_pad)?;
            obj.remove(&pad_data.identity)?;
        }

        // Unset sink pad target
        if let Some(sink_pad) = obj.static_pad("sink") {
            let ghost_pad = sink_pad
                .downcast_ref::<gst::GhostPad>()
                .ok_or_else(|| anyhow!("Sink pad is not a GhostPad"))?;
            ghost_pad.set_target(None::<&gst::Pad>)?;
        }

        gst::info!(CAT, imp = self, "ParsebinLoop stopped successfully");
        Ok(())
    }

    fn handle_pad_added_impl(
        parsebin: &gst::Element,
        src_pad: &gst::Pad,
        pad_map: &Arc<Mutex<HashMap<String, PadData>>>,
    ) -> Result<()> {
        gst::debug!(CAT, "Handling pad added: {}", src_pad.name());

        let bin = parsebin
            .parent()
            .ok_or_else(|| anyhow!("Parsebin has no parent"))?
            .downcast::<gst::Bin>()
            .map_err(|_| anyhow!("Parent is not a bin"))?;

        // The single-segment property ensures continuous buffer timestamps, enabling seamless looping.
        // Without it, each loop iteration would restart timestamps, creating playback discontinuities.
        let identity = gst::ElementFactory::make("identity")
            .property("single-segment", true)
            .build()?;

        bin.add(&identity)?;
        identity.sync_state_with_parent()?;

        let identity_sink = identity
            .static_pad("sink")
            .ok_or_else(|| anyhow!("Failed to get identity sink pad"))?;
        src_pad.link(&identity_sink)?;

        // Reuse the pad name from parsebin
        let ghost_pad_name = src_pad.name().to_string();
        let template = bin
            .pad_template("src_%u")
            .ok_or_else(|| anyhow!("Failed to get src_%u template"))?;

        let ghost_pad = gst::GhostPad::builder_from_template_with_target(
            &template,
            &identity.static_pad("src").unwrap(),
        )?
        .name(ghost_pad_name)
        .build();
        bin.add_pad(&ghost_pad)?;

        let pad_data = PadData {
            identity,
            ghost_pad,
        };

        pad_map
            .lock()
            .map_err(|_| anyhow!("Failed to lock pad map"))?
            .insert(src_pad.name().to_string(), pad_data);

        // Loop pad probes
        Self::loop_pad_probes(parsebin, src_pad)?;

        Ok(())
    }

    fn loop_pad_probes(parsebin: &gst::Element, src_pad: &gst::Pad) -> Result<()> {
        let parsebin_clone = parsebin.clone();
        src_pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_, info| {
            if let Some(gst::PadProbeData::Event(ref event)) = info.data {
                match event.view() {
                    gst::EventView::SegmentDone(_) | gst::EventView::Eos(_) => {
                        if let Err(err) = parsebin_clone.seek(
                            1.0,
                            gst::SeekFlags::SEGMENT,
                            gst::SeekType::Set,
                            gst::ClockTime::ZERO,
                            gst::SeekType::Set,
                            gst::ClockTime::NONE,
                        ) {
                            gst::warning!(CAT, "Failed segment seek: {}", err);
                        }
                    }
                    _ => {}
                }
            }
            gst::PadProbeReturn::Pass
        });

        // Treat it like no random access, no seeking allowed.
        src_pad.add_probe(gst::PadProbeType::QUERY_BOTH, move |_, info| {
            if let Some(gst::PadProbeData::Query(ref mut query)) = info.data {
                match query.view_mut() {
                    gst::QueryViewMut::Seeking(q) => {
                        let format = q.format();
                        if format == gst::Format::Time {
                            q.set(false, gst::ClockTime::ZERO, gst::ClockTime::NONE);
                            gst::PadProbeReturn::Handled
                        } else {
                            gst::PadProbeReturn::Pass
                        }
                    }
                    _ => gst::PadProbeReturn::Pass,
                }
            } else {
                gst::PadProbeReturn::Pass
            }
        });

        // initial seek
        if let Err(err) = parsebin.seek(
            1.0,
            gst::SeekFlags::FLUSH | gst::SeekFlags::SEGMENT,
            gst::SeekType::Set,
            gst::ClockTime::ZERO,
            gst::SeekType::Set,
            gst::ClockTime::NONE,
        ) {
            gst::warning!(CAT, "Failed initial flushing segment seek: {}", err);
        }
        Ok(())
    }
}
