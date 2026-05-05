// Copyright (C) 2025 Roberto Viola <rviola@vicomtech.org>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use crate::dashsink2::manifest::{Manifest, ManifestType, MediaRepresentation};
use gio::{File as GioFile, FileCreateFlags, OutputStream, prelude::*};
use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use std::collections::HashMap;
use std::sync::LazyLock;
use std::sync::Mutex;

const DEFAULT_TARGET_DURATION: u32 = 10000;
const DEFAULT_LATENCY: gst::ClockTime =
    gst::ClockTime::from_mseconds((DEFAULT_TARGET_DURATION / 2) as u64);
const DEFAULT_DYNAMIC: bool = false;
const DEFAULT_SYNC: bool = true;
const DEFAULT_FILENAME: &str = "manifest.mpd";
const TEMPLATE_INIT_FILENAME: &str = "init.cmfi";
const TEMPLATE_VIDEO_SEGMENT_FILENAME: &str = "segment_%d.cmfv";
const TEMPLATE_AUDIO_SEGMENT_FILENAME: &str = "segment_%d.cmfa";
const DEFAULT_MIN_BUFFER_TIME: u32 = 10000;

const SIGNAL_GET_MANIFEST_STREAM: &str = "get-manifest-stream";
const SIGNAL_GET_INIT_STREAM: &str = "get-init-stream";
const SIGNAL_GET_SEGMENT_STREAM: &str = "get-segment-stream";

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "dashsink2",
        gst::DebugColorFlags::empty(),
        Some("DASH Sink"),
    )
});

struct DashSink2Settings {
    dynamic: bool,
    mpd_root_path: Option<String>,
    mpd_filename: String,
    target_duration: u32,
    sync: bool,
    latency: gst::ClockTime,
    min_buffer_time: u32,
    minimum_update_period: Option<u32>,
    utc_timing_url: Option<String>,
}

struct DashSink2Stream {
    segment_idx: u32,
    bandwidth: Option<u64>,
    cmafmux: gst::Element,
    appsink: gst_app::AppSink,
}

#[derive(Default)]
pub struct DashSink2 {
    settings: Mutex<DashSink2Settings>,
    streams: Mutex<HashMap<String, DashSink2Stream>>,
    manifest: Mutex<Option<Manifest>>,
}

#[glib::object_subclass]
impl ObjectSubclass for DashSink2 {
    const NAME: &'static str = "DashSink2";
    type Type = super::DashSink2;
    type ParentType = gst::Bin;
}

impl Default for DashSink2Settings {
    fn default() -> Self {
        Self {
            dynamic: DEFAULT_DYNAMIC,
            mpd_root_path: None,
            mpd_filename: String::from(DEFAULT_FILENAME),
            target_duration: DEFAULT_TARGET_DURATION,
            sync: DEFAULT_SYNC,
            latency: DEFAULT_LATENCY,
            min_buffer_time: DEFAULT_MIN_BUFFER_TIME,
            minimum_update_period: None,
            utc_timing_url: None,
        }
    }
}

impl Default for DashSink2Stream {
    fn default() -> Self {
        let cmafmux = gst::ElementFactory::make("cmafmux")
            .property(
                "fragment-duration",
                gst::ClockTime::from_mseconds(DEFAULT_TARGET_DURATION as u64),
            )
            .property("latency", DEFAULT_LATENCY)
            .property_from_str("fragment-boundary-mode", "closest")
            .build()
            .expect("Could not create cmafmux");

        let appsink = gst_app::AppSink::builder()
            .buffer_list(true)
            .sync(DEFAULT_SYNC)
            .build();

        Self {
            segment_idx: 0,
            bandwidth: None,
            cmafmux,
            appsink,
        }
    }
}

impl BinImpl for DashSink2 {}

impl ObjectImpl for DashSink2 {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecBoolean::builder("dynamic")
                    .nick("Dynamic")
                    .blurb("Whether to generate dynamic manifest (MPD)")
                    .default_value(DEFAULT_DYNAMIC)
                    .build(),
                glib::ParamSpecString::builder("mpd-root-path")
                    .nick("MPD Root Path")
                    .blurb("Root path to write the manifest (MPD) file")
                    .default_value(None)
                    .build(),
                glib::ParamSpecString::builder("mpd-filename")
                    .nick("MPD Filename")
                    .blurb("Filename of the manifest (MPD) file")
                    .default_value(Some(DEFAULT_FILENAME))
                    .build(),
                glib::ParamSpecUInt::builder("target-duration")
                    .nick("Target Duration")
                    .blurb("Target duration in milliseconds for each segment")
                    .default_value(DEFAULT_TARGET_DURATION)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoolean::builder("sync")
                    .nick("Sync")
                    .blurb("Whether to sync appsink to the pipeline clock")
                    .default_value(DEFAULT_SYNC)
                    .build(),
                glib::ParamSpecUInt64::builder("latency")
                    .nick("Latency")
                    .blurb("Latency in milliseconds")
                    .default_value(DEFAULT_LATENCY.mseconds())
                    .build(),
                glib::ParamSpecUInt::builder("min-buffer-time")
                    .nick("Minimum Buffer Time")
                    .blurb("Minimum buffer time in milliseconds for MPD")
                    .default_value(2)
                    .minimum(1)
                    .build(),
                glib::ParamSpecUInt::builder("minimum-update-period")
                    .nick("Minimum Update Period")
                    .blurb("Optional update period in milliseconds for dynamic MPD")
                    .default_value(0)
                    .build(),
                glib::ParamSpecString::builder("utc-timing-url")
                    .nick("UTC Timing URL")
                    .blurb("URL to be used in UTCTiming with http-xsdate scheme")
                    .default_value(None)
                    .build(),
            ]
        });
        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let mut settings = self.settings.lock().unwrap();
        match pspec.name() {
            "dynamic" => {
                settings.dynamic = value.get().expect("type checked upstream");
                let mut manifest = self.manifest.lock().unwrap();
                if settings.dynamic {
                    manifest
                        .as_mut()
                        .unwrap()
                        .set_mpd_type(ManifestType::Dynamic);
                } else {
                    manifest
                        .as_mut()
                        .unwrap()
                        .set_mpd_type(ManifestType::Static);
                }
            }
            "mpd-root-path" => {
                settings.mpd_root_path = value
                    .get::<Option<String>>()
                    .expect("type checked upstream");
            }
            "mpd-filename" => {
                settings.mpd_filename = value
                    .get::<Option<String>>()
                    .expect("type checked upstream")
                    .unwrap_or_else(|| DEFAULT_FILENAME.into());
            }
            "target-duration" => {
                settings.target_duration = value.get().expect("type checked upstream");
            }
            "sync" => {
                settings.sync = value.get().expect("type checked upstream");
            }
            "latency" => {
                let latency_ns = value.get::<u64>().expect("type checked upstream");
                settings.latency = gst::ClockTime::from_nseconds(latency_ns);
            }
            "min-buffer-time" => {
                settings.min_buffer_time = value.get().expect("type checked upstream");
                let mut manifest = self.manifest.lock().unwrap();
                manifest
                    .as_mut()
                    .unwrap()
                    .set_min_buffer_time(settings.min_buffer_time);
            }
            "minimum-update-period" => {
                let val = value.get::<u32>().expect("type checked upstream");
                if val == 0 {
                    settings.minimum_update_period = None;
                } else {
                    settings.minimum_update_period = Some(val);
                    let mut manifest = self.manifest.lock().unwrap();
                    manifest.as_mut().unwrap().set_minimum_update_period(val);
                }
            }
            "utc-timing-url" => {
                settings.utc_timing_url = value
                    .get::<Option<String>>()
                    .expect("type checked upstream");
                if let Some(url) = &settings.utc_timing_url {
                    let mut manifest = self.manifest.lock().unwrap();
                    manifest.as_mut().unwrap().set_utc_timing_url(url.clone());
                }
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();
        match pspec.name() {
            "dynamic" => settings.dynamic.to_value(),
            "mpd-root-path" => settings.mpd_root_path.to_value(),
            "mpd-filename" => settings.mpd_filename.to_value(),
            "target-duration" => settings.target_duration.to_value(),
            "sync" => settings.sync.to_value(),
            "latency" => settings.latency.mseconds().to_value(),
            "min-buffer-time" => settings.min_buffer_time.to_value(),
            "minimum-update-period" => settings.minimum_update_period.unwrap_or(0).to_value(),
            "utc-timing-url" => settings.utc_timing_url.to_value(),
            _ => unimplemented!("Property {} not implemented", pspec.name()),
        }
    }

    fn signals() -> &'static [glib::subclass::Signal] {
        use glib::subclass::Signal;
        use glib::types::Type as GType;
        static SIGNALS: LazyLock<Vec<Signal>> = LazyLock::new(|| {
            vec![
                glib::subclass::Signal::builder(SIGNAL_GET_INIT_STREAM)
                    .param_types([GType::STRING])
                    .return_type::<Option<gio::OutputStream>>()
                    .class_handler(|args| {
                        let location: String = args[1].get().expect("location");
                        let s = DashSink2::new_file_stream(&location).ok();
                        Some(s.to_value())
                    })
                    .accumulator(|_hint, _acc, value| std::ops::ControlFlow::Break(value.clone()))
                    .build(),
                glib::subclass::Signal::builder(SIGNAL_GET_SEGMENT_STREAM)
                    .param_types([GType::STRING])
                    .return_type::<Option<gio::OutputStream>>()
                    .class_handler(|args| {
                        let location: String = args[1].get().expect("location");
                        let s = DashSink2::new_file_stream(&location).ok();
                        Some(s.to_value())
                    })
                    .accumulator(|_hint, _acc, value| std::ops::ControlFlow::Break(value.clone()))
                    .build(),
                glib::subclass::Signal::builder(SIGNAL_GET_MANIFEST_STREAM)
                    .param_types([GType::STRING])
                    .return_type::<Option<gio::OutputStream>>()
                    .class_handler(|args| {
                        let location: String = args[1].get().expect("location");
                        let s = DashSink2::new_file_stream(&location).ok();
                        Some(s.to_value())
                    })
                    .accumulator(|_hint, _acc, value| std::ops::ControlFlow::Break(value.clone()))
                    .build(),
            ]
        });
        SIGNALS.as_ref()
    }

    fn constructed(&self) {
        self.parent_constructed();
        self.manifest
            .lock()
            .unwrap()
            .get_or_insert_with(Manifest::new);
    }
}

impl GstObjectImpl for DashSink2 {}

impl ElementImpl for DashSink2 {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "DASH Sink",
                "Sink/Muxer",
                "MPEG-DASH Sink",
                "Roberto Viola <rviola@vicomtech.org>",
            )
        });
        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let video_pad_template = gst::PadTemplate::new(
                "video_%u",
                gst::PadDirection::Sink,
                gst::PadPresence::Request,
                &[
                    gst::Structure::builder("video/x-h264")
                        .field("stream-format", gst::List::new(["avc", "avc3"]))
                        .field("alignment", "au")
                        .field("width", gst::IntRange::new(1, u16::MAX as i32))
                        .field("height", gst::IntRange::new(1, u16::MAX as i32))
                        .build(),
                    gst::Structure::builder("video/x-h265")
                        .field("stream-format", gst::List::new(["hvc1", "hev1"]))
                        .field("alignment", "au")
                        .field("width", gst::IntRange::new(1, u16::MAX as i32))
                        .field("height", gst::IntRange::new(1, u16::MAX as i32))
                        .build(),
                    gst::Structure::builder("video/x-av1")
                        .field("stream-format", "obu-stream")
                        .field("alignment", "tu")
                        .field("profile", gst::List::new(["main", "high", "professional"]))
                        .field(
                            "croma-format",
                            gst::List::new(["4:0:0", "4:2:0", "4:2:2", "4:4:4"]),
                        )
                        .field("bit-depth-luma", gst::List::new([8u32, 10u32, 12u32]))
                        .field("bit-depth-chroma", gst::List::new([8u32, 10u32, 12u32]))
                        .field("width", gst::IntRange::new(1, u16::MAX as i32))
                        .field("height", gst::IntRange::new(1, u16::MAX as i32))
                        .build(),
                ]
                .into_iter()
                .collect::<gst::Caps>(),
            )
            .unwrap();
            let audio_pad_template = gst::PadTemplate::new(
                "audio_%u",
                gst::PadDirection::Sink,
                gst::PadPresence::Request,
                &[
                    gst::Structure::builder("audio/mpeg")
                        .field("mpegversion", 4i32)
                        .field("stream-format", "raw")
                        .field("channels", gst::IntRange::new(1, u16::MAX as i32))
                        .field("rate", gst::IntRange::new(1, i32::MAX))
                        .build(),
                    gst::Structure::builder("audio/x-opus")
                        .field("channel-mapping-family", gst::IntRange::new(0i32, 255))
                        .field("channels", gst::IntRange::new(1i32, 8))
                        .field("rate", gst::IntRange::new(1, i32::MAX))
                        .build(),
                ]
                .into_iter()
                .collect::<gst::Caps>(),
            )
            .unwrap();
            vec![video_pad_template, audio_pad_template]
        });
        PAD_TEMPLATES.as_ref()
    }

    fn change_state(
        &self,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        let ret = self.parent_change_state(transition)?;
        if let gst::StateChange::PausedToReady = transition {
            // Finalize manifest writing
            gst::info!(CAT, imp = self, "Finalizing manifest");
            let mut manifest = self.manifest.lock().unwrap();
            if let Some(ref mut m) = *manifest {
                // Turn VOD when the manifest is finalized
                m.set_mpd_type(ManifestType::Static);
                if let Ok(xml) = m.to_string() {
                    let settings = self.settings.lock().unwrap();
                    let path = settings
                        .mpd_root_path
                        .as_ref()
                        .map(|r| format!("{}/{}", r, settings.mpd_filename))
                        .unwrap_or_else(|| settings.mpd_filename.clone());
                    if let Err(e) = std::fs::write(&path, xml) {
                        gst::error!(CAT, imp = self, "Failed to write manifest: {e}");
                    }
                }
            }
            // Cleanup internal state
            self.streams.lock().unwrap().clear();
        }
        Ok(ret)
    }

    fn request_new_pad(
        &self,
        templ: &gst::PadTemplate,
        _name: Option<&str>,
        _caps: Option<&gst::Caps>,
    ) -> Option<gst::Pad> {
        let pad_name = if templ.name().starts_with("video_") {
            format!("video_{}", self.streams.lock().unwrap().len())
        } else {
            format!("audio_{}", self.streams.lock().unwrap().len())
        };

        // Create stream components
        gst::info!(CAT, imp = self, "Requesting new pad: {pad_name}");

        let stream = DashSink2Stream::default();

        let target_duration = {
            let settings = self.settings.lock().unwrap();
            stream.cmafmux.set_property(
                "fragment-duration",
                gst::ClockTime::from_mseconds(settings.target_duration as u64),
            );
            stream.cmafmux.set_property("latency", settings.latency);
            stream.appsink.set_property("sync", settings.sync);

            settings.target_duration
        };

        // Add and link elements
        let obj = self.obj();
        obj.add_many([&stream.cmafmux, stream.appsink.upcast_ref()])
            .ok()?;
        stream.cmafmux.link(&stream.appsink).ok()?;

        let is_video = templ.name().starts_with("video_");
        let seg_filename = if is_video {
            TEMPLATE_VIDEO_SEGMENT_FILENAME
        } else {
            TEMPLATE_AUDIO_SEGMENT_FILENAME
        };
        let (init_loc, seg_template) = (
            self.build_segment_path(None, &pad_name, TEMPLATE_INIT_FILENAME, None),
            self.build_segment_path(None, &pad_name, seg_filename, None),
        );

        let pad_name_clone = pad_name.clone();
        let target_pad = stream.cmafmux.static_pad("sink").unwrap();
        let gpad = gst::GhostPad::builder_with_target(&target_pad)
            .unwrap()
            .event_function(move |pad, parent, event| {
                DashSink2::catch_panic_pad_function(
                    parent,
                    || false,
                    |dash| {
                        dash.sink_event(
                            pad,
                            parent,
                            &pad_name_clone,
                            &init_loc,
                            &seg_template,
                            target_duration,
                            event,
                        )
                    },
                )
            })
            .name(&pad_name)
            .build();
        obj.add_pad(&gpad).unwrap();

        // Appsink callback
        let stream_pad_name = pad_name.clone();
        let self_weak = self.downgrade();
        stream.appsink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let Some(imp) = self_weak.upgrade() else {
                        return Err(gst::FlowError::Eos);
                    };
                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    imp.on_new_sample(sample, &stream_pad_name)
                })
                .build(),
        );

        // Store stream
        self.streams
            .lock()
            .unwrap()
            .insert(pad_name.clone(), stream);
        Some(gpad.upcast())
    }

    fn release_pad(&self, pad: &gst::Pad) {
        let pad_name = pad.name();
        let mut streams = self.streams.lock().unwrap();
        streams.remove(pad_name.as_str());
    }
}

impl DashSink2 {
    fn build_full_path(&self, root: Option<&String>, rep_id: &str, filename: &str) -> String {
        if let Some(root_path) = root {
            format!("{}/{}_{}", root_path, rep_id, filename)
        } else {
            format!("{}_{}", rep_id, filename)
        }
    }

    fn build_segment_path(
        &self,
        root: Option<&String>,
        rep_id: &str,
        segment_template: &str,
        index: Option<u32>,
    ) -> String {
        let filename = if segment_template.contains("%d") {
            if let Some(idx) = index {
                sprintf::sprintf!(segment_template, idx).unwrap()
            } else {
                segment_template.replace("%d", "$Number$")
            }
        } else {
            segment_template.to_string()
        };
        self.build_full_path(root, rep_id, &filename)
    }

    fn new_file_stream(path: &str) -> Result<OutputStream, glib::Error> {
        let gfile = GioFile::for_path(path);

        let fos = gfile.replace(
            None,
            false,
            FileCreateFlags::empty(),
            None::<&gio::Cancellable>,
        )?;

        Ok(fos.upcast::<OutputStream>())
    }

    #[allow(clippy::too_many_arguments)]
    fn sink_event(
        &self,
        pad: &gst::GhostPad,
        parent: Option<&impl IsA<gst::Object>>,
        pad_name: &str,
        init_location: &str,
        segment_template: &str,
        target_duration: u32,
        event: gst::Event,
    ) -> bool {
        if let gst::EventView::Caps(caps_event) = event.view() {
            let caps = caps_event.caps();
            gst::info!(
                CAT,
                imp = self,
                "Caps changed on pad {}: {:?}",
                pad.name(),
                caps
            );

            if let Some(s) = caps.structure(0) {
                let media_type = s.name();
                let is_video = media_type.starts_with("video/");

                let codec: String = gst_pbutils::codec_utils_caps_get_mime_codec(caps)
                    .unwrap()
                    .to_string();

                let (width, height, framerate) = if is_video {
                    let width = s.get::<i32>("width").unwrap_or(1280);
                    let height = s.get::<i32>("height").unwrap_or(720);
                    let fps = s
                        .get::<gst::Fraction>("framerate")
                        .unwrap_or(gst::Fraction::new(30, 1));
                    (
                        Some(width as u64),
                        Some(height as u64),
                        Some(format!("{}/{}", fps.numer(), fps.denom())),
                    )
                } else {
                    (None, None, None)
                };

                let rep = MediaRepresentation {
                    id: pad_name.to_string(),
                    is_video,
                    codec,
                    width,
                    height,
                    framerate,
                    bandwidth: None,
                    init_location: init_location.to_string(),
                    segment_template: segment_template.to_string(),
                    segment_duration: target_duration,
                };

                let mut manifest = self.manifest.lock().unwrap();
                manifest.as_mut().unwrap().add_representation(rep);
            }
        }
        gst::Pad::event_default(pad, parent, event)
    }

    fn on_init_segment(&self, pad_name: &str) -> Result<OutputStream, glib::Error> {
        let mpd_root_path = {
            let settings = self.settings.lock().unwrap();
            settings.mpd_root_path.clone()
        };
        let filename = self.build_segment_path(
            mpd_root_path.as_ref(),
            pad_name,
            TEMPLATE_INIT_FILENAME,
            None,
        );
        let obj = self.obj();
        let stream: Option<gio::OutputStream> =
            obj.emit_by_name(SIGNAL_GET_INIT_STREAM, &[&filename]);
        stream.ok_or_else(|| {
            glib::Error::new(
                glib::FileError::Failed,
                &format!("No OutputStream returned for init: {}", filename),
            )
        })
    }

    fn on_new_segment(&self, pad_name: &str) -> Result<(OutputStream, String), glib::Error> {
        // Lock settings (read-only)
        let mpd_root_path = {
            let settings = self.settings.lock().unwrap();
            settings.mpd_root_path.clone()
        };

        // Lock and get mutable reference to the stream
        let mut streams = self.streams.lock().unwrap();
        let stream = streams.get_mut(pad_name).expect("Invalid pad name");

        // Generate path
        let seg_filename = if pad_name.starts_with("video_") {
            TEMPLATE_VIDEO_SEGMENT_FILENAME
        } else {
            TEMPLATE_AUDIO_SEGMENT_FILENAME
        };
        let filename = self.build_segment_path(
            mpd_root_path.as_ref(),
            pad_name,
            seg_filename,
            Some(stream.segment_idx),
        );

        stream.segment_idx += 1;
        drop(streams);

        // Create file
        let obj = self.obj();
        let file: Option<gio::OutputStream> =
            obj.emit_by_name(SIGNAL_GET_SEGMENT_STREAM, &[&filename]);
        let file = file.ok_or_else(|| {
            glib::Error::new(
                glib::FileError::Failed,
                &format!("No OutputStream returned for segment: {}", filename),
            )
        })?;

        Ok((file, filename))
    }

    fn add_segment(
        &self,
        pad_name: String,
        index: u32,
        bandwidth: u64,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let settings = self.settings.lock().unwrap();
        let manifest_path = if let Some(ref root) = settings.mpd_root_path {
            format!("{}/{}", root, settings.mpd_filename)
        } else {
            settings.mpd_filename.clone()
        };
        drop(settings);

        gst::info!(CAT, imp = self, "writing manifest to {}", manifest_path);

        let mut manifest = self.manifest.lock().unwrap();
        manifest
            .as_mut()
            .unwrap()
            .add_segment(pad_name, index, bandwidth);

        let xml = manifest.as_ref().unwrap().to_string().map_err(|e| {
            gst::error!(CAT, imp = self, "Failed to serialize MPD: {e}");
            gst::FlowError::Error
        })?;

        let stream: Option<gio::OutputStream> = self
            .obj()
            .emit_by_name(SIGNAL_GET_MANIFEST_STREAM, &[&manifest_path]);

        let stream = stream.ok_or_else(|| {
            gst::error!(
                CAT,
                imp = self,
                "No OutputStream for manifest {}",
                manifest_path
            );
            gst::FlowError::Error
        })?;

        stream
            .write_all(xml.as_bytes(), None::<&gio::Cancellable>)
            .map_err(|_| {
                gst::error!(CAT, imp = self, "Failed to write manifest to stream");
                gst::FlowError::Error
            })?;
        stream.flush(None::<&gio::Cancellable>).map_err(|_| {
            gst::error!(CAT, imp = self, "Failed to flush manifest stream");
            gst::FlowError::Error
        })?;

        Ok(gst::FlowSuccess::Ok)
    }

    fn on_new_sample(
        &self,
        sample: gst::Sample,
        pad_name: &str,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut buffer_list = sample.buffer_list_owned().ok_or(gst::FlowError::Error)?;
        let first = buffer_list.get(0).ok_or(gst::FlowError::Error)?;

        // Check for init segment (DISCONT or HEADER flags)
        if first
            .flags()
            .contains(gst::BufferFlags::DISCONT | gst::BufferFlags::HEADER)
        {
            let stream = self.on_init_segment(pad_name).map_err(|err| {
                gst::error!(
                    CAT,
                    imp = self,
                    "Couldn't get output stream for init segment: {err}",
                );
                gst::FlowError::Error
            })?;

            let map = first.map_readable().map_err(|_| {
                gst::error!(CAT, imp = self, "Failed to map init segment buffer");
                gst::FlowError::Error
            })?;

            stream
                .write_all(map.as_slice(), None::<&gio::Cancellable>)
                .map_err(|_| {
                    gst::error!(
                        CAT,
                        imp = self,
                        "Couldn't write init segment to output stream"
                    );
                    gst::FlowError::Error
                })?;

            stream.flush(None::<&gio::Cancellable>).map_err(|_| {
                gst::error!(CAT, imp = self, "Couldn't flush init segment stream");
                gst::FlowError::Error
            })?;

            drop(map);

            // Remove init segment from buffer list
            buffer_list.make_mut().remove(0..1);

            if buffer_list.is_empty() {
                return Ok(gst::FlowSuccess::Ok);
            }
        }

        // Get actual segment duration from the segment header (first remaining buffer).
        // cmafmux sets accurate PTS and duration on the segment header buffer.
        let actual_duration_ms = buffer_list
            .get(0)
            .and_then(|b| b.duration())
            .map(|d| d.mseconds())
            .unwrap_or_else(|| self.settings.lock().unwrap().target_duration as u64);

        // Get output stream + filename
        let (stream, _filename) = self.on_new_segment(pad_name).map_err(|err| {
            gst::error!(
                CAT,
                imp = self,
                "Couldn't get output stream for fragment: {err}",
            );
            gst::FlowError::Error
        })?;

        let mut total_size = 0;
        // Write all fragment buffers
        for buffer in &*buffer_list {
            let map = buffer.map_readable().map_err(|_| {
                gst::error!(CAT, imp = self, "Failed to map fragment buffer");
                gst::FlowError::Error
            })?;

            stream
                .write_all(map.as_slice(), None::<&gio::Cancellable>)
                .map_err(|_| {
                    gst::error!(CAT, imp = self, "Couldn't write fragment to output stream");
                    gst::FlowError::Error
                })?;
            total_size += map.size();
        }

        stream.flush(None::<&gio::Cancellable>).map_err(|_| {
            gst::error!(CAT, imp = self, "Couldn't flush fragment stream");
            gst::FlowError::Error
        })?;

        let (index, bandwidth) = {
            let mut streams = self.streams.lock().unwrap();
            let dash_stream = streams
                .get_mut(pad_name)
                .expect("pad name not found in streams");

            // Bandwidth: total_size (bytes) * 8 (bits) / actual duration (seconds)
            let duration_secs = actual_duration_ms as f64 / 1000.0;
            let current_bandwidth = if duration_secs > 0.0 {
                (total_size as f64 * 8.0 / duration_secs) as u64
            } else {
                0
            };

            // Running average across all segments
            let previous_bandwidth = dash_stream.bandwidth.unwrap_or(0);
            let previous_segments = dash_stream.segment_idx.saturating_sub(1) as u64;
            let overall_bandwidth = if dash_stream.segment_idx > 0 {
                (previous_bandwidth * previous_segments + current_bandwidth)
                    / dash_stream.segment_idx as u64
            } else {
                current_bandwidth
            };
            dash_stream.bandwidth = Some(overall_bandwidth);

            gst::info!(
                CAT,
                imp = self,
                "Segment {} total size: {} bytes, duration: {}ms, bandwidth: {} bps",
                dash_stream.segment_idx,
                total_size,
                actual_duration_ms,
                overall_bandwidth,
            );

            (dash_stream.segment_idx, overall_bandwidth)
        };

        self.add_segment(pad_name.to_string(), index, bandwidth)
    }
}
