// Copyright (C) 2025 Axel Tobieson <axel.tobieson@spiideo.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use once_cell::sync::Lazy;
use std::sync::{LazyLock, Mutex};

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "loopfilesrc",
        gst::DebugColorFlags::empty(),
        Some("Looping file source"),
    )
});

#[derive(Default)]
struct Settings {
    uri: Option<String>,
}

#[derive(Default)]
pub struct LoopFileSrc {
    settings: Mutex<Settings>,
    source: Mutex<Option<gst::Element>>,
    parsebinloop: Mutex<Option<gst::Element>>,
}

#[glib::object_subclass]
impl ObjectSubclass for LoopFileSrc {
    const NAME: &'static str = "GstLoopFileSrc";
    type Type = super::LoopFileSrc;
    type ParentType = gst::Bin;
    type Interfaces = (gst::URIHandler,);
}

impl ObjectImpl for LoopFileSrc {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![glib::ParamSpecString::builder("uri")
                .nick("URI")
                .blurb("The looping URI (e.g., file+loop:///path/to/file.mp4)")
                .mutable_ready()
                .build()]
        });
        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "uri" => {
                let uri: Option<String> = value.get().expect("type checked upstream");
                if let Some(uri) = uri {
                    if let Err(e) = self.obj().set_uri(&uri) {
                        gst::error!(CAT, imp = self, "Failed to set URI property: {}", e);
                    }
                } else {
                    let mut settings = self.settings.lock().unwrap();
                    settings.uri = None;
                }
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "uri" => {
                let settings = self.settings.lock().unwrap();
                settings.uri.clone().to_value()
            }
            _ => unimplemented!(),
        }
    }

    fn constructed(&self) {
        self.parent_constructed();

        let obj = self.obj();

        match gst::ElementFactory::make("parsebinloop").build() {
            Ok(parsebinloop) => {
                if let Err(e) = obj.add(&parsebinloop) {
                    gst::error!(CAT, imp = self, "Failed to add parsebinloop: {}", e);
                    return;
                }

                let bin_weak = obj.downgrade();
                parsebinloop.connect_pad_added(move |_, src_pad| {
                    let Some(bin) = bin_weak.upgrade() else {
                        return;
                    };

                    gst::debug!(
                        CAT,
                        imp = bin.imp(),
                        "parsebinloop pad added: {}",
                        src_pad.name()
                    );

                    match gst::GhostPad::builder_with_target(src_pad) {
                        Ok(builder) => {
                            let ghost_pad = builder.name(src_pad.name()).build();
                            if let Err(e) = bin.add_pad(&ghost_pad) {
                                gst::error!(CAT, imp = bin.imp(), "Failed to add ghost pad: {}", e);
                            }
                        }
                        Err(e) => {
                            gst::error!(CAT, imp = bin.imp(), "Failed to create ghost pad: {}", e);
                        }
                    }
                });

                let bin_weak = obj.downgrade();
                parsebinloop.connect_pad_removed(move |_, src_pad| {
                    let Some(bin) = bin_weak.upgrade() else {
                        return;
                    };

                    if let Some(ghost_pad) = bin.static_pad(&src_pad.name()) {
                        let _ = bin.remove_pad(&ghost_pad);
                    }
                });

                *self.parsebinloop.lock().unwrap() = Some(parsebinloop);
            }
            Err(e) => {
                gst::error!(CAT, imp = self, "Failed to create parsebinloop: {}", e);
            }
        }
    }
}

impl GstObjectImpl for LoopFileSrc {}

impl ElementImpl for LoopFileSrc {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "Looping file source",
                "Source",
                "Loops any URI-based source input using parsebinloop",
                "Spiideo",
            )
        });
        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let caps = gst::Caps::new_any();
            let src_pad_template = gst::PadTemplate::new(
                "src_%u",
                gst::PadDirection::Src,
                gst::PadPresence::Sometimes,
                &caps,
            )
            .unwrap();

            vec![src_pad_template]
        });
        PAD_TEMPLATES.as_ref()
    }

    fn change_state(
        &self,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        if transition == gst::StateChange::ReadyToPaused {
            if let Err(e) = self.start() {
                gst::error!(CAT, imp = self, "Failed to start: {}", e);
                return Err(gst::StateChangeError);
            }
        }

        let res = self.parent_change_state(transition);

        if transition == gst::StateChange::PausedToReady {
            self.stop();
        }

        res
    }
}

impl BinImpl for LoopFileSrc {}

impl URIHandlerImpl for LoopFileSrc {
    const URI_TYPE: gst::URIType = gst::URIType::Src;

    fn protocols() -> &'static [&'static str] {
        &["file+loop", "smb+loop"]
    }

    fn uri(&self) -> Option<String> {
        let settings = self.settings.lock().unwrap();
        settings.uri.clone()
    }

    fn set_uri(&self, uri: &str) -> Result<(), glib::Error> {
        gst::debug!(CAT, imp = self, "Setting URI: {}", uri);

        // Simple validation - check if scheme ends with +loop
        if let Some(scheme_end) = uri.find("://") {
            let scheme = &uri[..scheme_end];
            if !scheme.ends_with("+loop") {
                return Err(glib::Error::new(
                    gst::URIError::BadUri,
                    &format!("URI scheme must end with '+loop', got: {}", scheme),
                ));
            }
        } else {
            return Err(glib::Error::new(
                gst::URIError::BadUri,
                "Invalid URI format",
            ));
        }

        let mut settings = self.settings.lock().unwrap();
        settings.uri = Some(uri.to_string());

        Ok(())
    }
}

impl LoopFileSrc {
    fn start(&self) -> Result<(), glib::Error> {
        let settings = self.settings.lock().unwrap();
        let Some(uri) = settings.uri.as_ref() else {
            return Err(glib::Error::new(gst::URIError::BadReference, "No URI set"));
        };

        // Parse the looping URI to extract base URI
        let base_uri = Self::parse_looping_uri(uri)?;
        drop(settings);

        gst::info!(CAT, imp = self, "Creating source for URI: {}", base_uri);

        // Create source element from base URI
        let source =
            gst::Element::make_from_uri(gst::URIType::Src, &base_uri, None).map_err(|e| {
                glib::Error::new(
                    gst::URIError::BadReference,
                    &format!("Failed to create source element: {}", e),
                )
            })?;

        let obj = self.obj();
        obj.add(&source).map_err(|e| {
            glib::Error::new(
                gst::LibraryError::Failed,
                &format!("Failed to add source to bin: {}", e),
            )
        })?;

        // Link source to parsebinloop
        let parsebinloop_sink = {
            let parsebinloop = self.parsebinloop.lock().unwrap();
            parsebinloop
                .as_ref()
                .and_then(|p| p.static_pad("sink"))
                .ok_or_else(|| {
                    glib::Error::new(gst::LibraryError::Failed, "parsebinloop has no sink pad")
                })?
        };

        let source_src = source
            .static_pad("src")
            .ok_or_else(|| glib::Error::new(gst::LibraryError::Failed, "source has no src pad"))?;

        source_src.link(&parsebinloop_sink).map_err(|e| {
            glib::Error::new(
                gst::LibraryError::Failed,
                &format!("Failed to link source to parsebinloop: {}", e),
            )
        })?;

        source.sync_state_with_parent().map_err(|e| {
            glib::Error::new(
                gst::LibraryError::Failed,
                &format!("Failed to sync source state: {}", e),
            )
        })?;

        if let Some(ref parsebinloop) = *self.parsebinloop.lock().unwrap() {
            parsebinloop.sync_state_with_parent().map_err(|e| {
                glib::Error::new(
                    gst::LibraryError::Failed,
                    &format!("Failed to sync parsebinloop state: {}", e),
                )
            })?;
        }

        *self.source.lock().unwrap() = Some(source);

        gst::info!(CAT, imp = self, "Successfully started loopfilesrc");
        Ok(())
    }

    fn stop(&self) {
        gst::info!(CAT, imp = self, "Stopping loopfilesrc");

        let mut source_guard = self.source.lock().unwrap();
        if let Some(source) = source_guard.take() {
            let _ = source.set_state(gst::State::Null);
            let _ = self.obj().remove(&source);
        }
    }

    fn parse_looping_uri(uri: &str) -> Result<String, glib::Error> {
        // Parse "file+loop://path" into "file://path"
        let scheme_end = uri.find("://").ok_or_else(|| {
            glib::Error::new(gst::URIError::BadUri, "Invalid URI format - missing ://")
        })?;

        let scheme = &uri[..scheme_end];

        if !scheme.ends_with("+loop") {
            return Err(glib::Error::new(
                gst::URIError::BadUri,
                &format!("URI scheme must end with '+loop', got: {}", scheme),
            ));
        }

        let base_scheme = scheme.trim_end_matches("+loop");

        // Replace scheme in URI string
        let base_uri = uri.replacen(&format!("{}://", scheme), &format!("{}://", base_scheme), 1);

        Ok(base_uri)
    }
}
