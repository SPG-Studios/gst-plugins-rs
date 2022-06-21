// Copyright (C) 2022 Jochen Henneberg <jh@henneberg-systemdesign.com>
//
// SPDX-License-Identifier: Apache-2.0 or MIT

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;

use std::cmp;
use std::sync::Mutex;

use crc::{Crc, CRC_32_ISCSI};
use gst_video::video_frame::*;
use gst_video::{VideoFormat, VideoInfo};
use once_cell::sync::Lazy;

mod syncstate;
use syncstate::SyncState;
mod data;
use data::Data;
mod settings;
use settings::*;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "ocvrctrl",
        gst::DebugColorFlags::empty(),
        Some("Original content video rate controller"),
    )
});

const CASTAGNOLI: Crc<u32> = Crc::<u32>::new(&CRC_32_ISCSI);

// Original content framerate to detect - in case of 'hint' listen to
// downstream events from ocvrhint, used in syncstate child module and
// thus needs to be pub
#[derive(Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Clone, Copy, glib::Enum)]
#[repr(u32)]
#[enum_type(name = "GstOcvrCtrlContentRate")]
pub enum ContentRate {
    #[enum_value(name = "Search for matching rate", nick = "Auto")]
    Auto,
    #[enum_value(name = "Content rate 24Hz", nick = "24Hz")]
    Hz24,
    #[enum_value(name = "Content rate 30Hz", nick = "30Hz")]
    Hz30,
    #[enum_value(name = "Content rate 60Hz", nick = "60Hz")]
    Hz60,
    #[enum_value(name = "From 'ocvrhint'", nick = "Hint")]
    Hint,
}

impl ContentRate {
    pub fn next(&self, rate: CaptureRate) -> ContentRate {
        match rate {
            CaptureRate::HZ_50 => unimplemented!(),
            CaptureRate::HZ_60 => match self {
                ContentRate::Hz24 => ContentRate::Hz30,
                ContentRate::Hz30 => ContentRate::Hz60,
                ContentRate::Hz60 => ContentRate::Hz24,
                _ => unreachable!(),
            },
            _ => unreachable!(),
        }
    }

    pub fn first(rate: CaptureRate) -> ContentRate {
        match rate {
            CaptureRate::HZ_50 => unimplemented!(),
            CaptureRate::HZ_60 => ContentRate::Hz24,
            _ => unreachable!(),
        }
    }
}

// Capture framerates to check, used in syncstate child module and
// thus needs to be pub
#[glib::flags(name = "OcvrCtrlCaptureRate")]
pub enum CaptureRate {
    #[flags_value(name = "Capture rate 50Hz", nick = "50Hz")]
    HZ_50 = 0b00000001,
    #[flags_value(name = "Capture rate 60Hz", nick = "60Hz")]
    HZ_60 = 0b00000010,
}

// Method used to check rate
#[derive(Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Clone, Copy, glib::Enum)]
#[repr(u32)]
#[enum_type(name = "GstOcvrCtrlMethod")]
pub enum Method {
    #[enum_value(name = "Fallback to fuzzy if accurate fails.", nick = "Auto")]
    Auto,
    #[enum_value(name = "Accurate compare by checksum.", nick = "Accurate")]
    Accurate,
    #[enum_value(name = "Fuzzy compare.", nick = "Fuzzy")]
    Fuzzy,
}

#[derive(Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Clone, Copy, glib::Enum)]
#[repr(u32)]
#[enum_type(name = "GstOcvrCtrlTolerance")]
pub enum Tolerance {
    #[enum_value(name = "Resync unless content rate is reset.", nick = "Lazy")]
    Lazy,
    #[enum_value(name = "Strict compare and resync 'retries' times.", nick = "Strict")]
    Strict,
    #[enum_value(name = "Like 'Strict' but ignore retries.", nick = "Paranoid")]
    Paranoid,
}

pub struct OcvrCtrl {
    srcpad: gst::Pad,
    sinkpad: gst::Pad,
    settings: Mutex<Settings>,
    data: Mutex<Data>,
}

impl OcvrCtrl {
    fn calc_checksum(win: &[u8]) -> u32 {
        let crc = CASTAGNOLI;
        let mut digest = crc.digest();

        digest.update(win);
        digest.finalize()
    }

    fn compare_fuzzy(ping: &[u8], pong: &[u8], threshold: u32) -> bool {
        let it = ping.iter();
        let mut r = true;
        pong.iter().zip(it).all(|(a, b)| {
            let ax = cmp::max(a, b);
            let bx = cmp::min(a, b);
            let d = ax - bx;
            if d > threshold as u8 {
                r = false;
            }
            r
        });

        gst::log!(CAT, "Fuzzy frame compare: {:?}", r);
        r
    }

    fn save_frame(&self, buffer: &gst::Buffer) {
        let mut data = self.data.lock().unwrap();

        if !data.sync_state.needs_save(data.capture_rate.unwrap()) {
            gst::log!(CAT, "Frame save not needed -> {:?}", data.sync_state);
            return;
        }

        gst::log!(CAT, "Save frame -> {:?}", data.sync_state);
        let info = VideoInfo::builder(
            data.frame_format.unwrap(),
            data.frame_size.0,
            data.frame_size.1,
        )
        .build()
        .unwrap();
        let frame =
            VideoFrameRef::<&gst::BufferRef>::from_buffer_ref_readable(buffer, &info).unwrap();
        let plane = match data.frame_format.unwrap() {
            VideoFormat::I420 => 0, // luma
            VideoFormat::Nv12 => 0, // luma
            _ => 0,                 // FIXME: select most important plane for other formats
        };
        let frame_data = frame.plane_data(plane).unwrap();
        let width = frame.width() as usize;
        let height = frame.height() as usize;

        let wn: &str;
        let win = if data.is_ping {
            wn = "pong";
            &mut data.pong_window
        } else {
            wn = "ping";
            &mut data.ping_window
        };
        gst::log!(CAT, "Save frame to {:?}", wn);

        win.clear();
        let settings = self.settings.lock().unwrap();
        for height in (0..height).step_by(height / settings.rows as usize) {
            let offset = height * width;
            win.extend_from_slice(&frame_data[offset..offset + width]);
        }

        data.is_ping = !data.is_ping;
    }

    fn compare_frames(&self) -> Result<bool, ()> {
        let data = self.data.lock().unwrap();

        if !data.can_compare() {
            return Err(());
        }

        let n = data.sync_state.needs_compare(data.capture_rate.unwrap());

        if !n {
            gst::log!(CAT, "Comparison not needed -> {:?}", data.sync_state);
            return Ok(true);
        }

        let r = match data.method {
            Method::Fuzzy => {
                gst::log!(CAT, "Compare frames 'fuzzy' -> {:?}", data.sync_state);
                let settings = self.settings.lock().unwrap();
                Self::compare_fuzzy(&data.ping_window, &data.pong_window, settings.threshold)
            }
            Method::Accurate | Method::Auto => {
                gst::log!(CAT, "Compare frames 'accurate' -> {:?}", data.sync_state);
                let crc_ping = Self::calc_checksum(&data.ping_window);
                let crc_pong = Self::calc_checksum(&data.pong_window);
                crc_ping == crc_pong
            }
        };

        Ok(r)
    }

    fn sink_chain(
        &self,
        _pad: &gst::Pad,
        element: &super::OcvrCtrl,
        mut buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        {
            let mut data = self.data.lock().unwrap();

            // if we don't have supported caps just forward the frame
            if data.capture_rate.is_none() || data.sync_state.is_idle() {
                gst::log!(CAT, obj: element, "Passthrough");
                return self.srcpad.push(buffer);
            }

            // otherwise advance the frame counter
            data.sync_state.advance();
        }

        // save the buffer and check for frame match
        self.save_frame(&buffer);
        let mut m = match self.compare_frames() {
            Err(_) => {
                gst::log!(CAT, obj: element, "Need at least two frames for comparison");
                return self.srcpad.push(buffer);
            }
            Ok(v) => v,
        };

        let mut data = self.data.lock().unwrap();
        let r = data.capture_rate.unwrap();
        let settings = self.settings.lock().unwrap();

        // if we are synced and frame comparison failed or if we lost
        // sync let's try to recover
        if !data.sync_state.eval_compare(r, &mut m) {
            let s = data.on_sync_lost(
                settings.method,
                settings.retries,
                settings.tolerance,
                settings.content_rate,
            );
            gst::info!(
                CAT,
                obj: element,
                "Unexpected frame mismatch - lost sync (solution: {:?}) -> {:?}",
                s,
                data.sync_state
            );

            // if we are idle (no sync retries left) tell ocvrhint to
            // start pattern matching again
            if settings.in_hint_mode() && data.sync_state.is_idle() {
                // let downstream know about the original caps
                let c = data.upstream_caps.copy();
                self.srcpad.push_event(gst::event::Caps::new(&c));

                let s = gst::Structure::new("ocvrctrl", &[("synced", &false)]);
                self.srcpad
                    .push_event(gst::event::CustomDownstream::builder(s).build());
            }

            return self.srcpad.push(buffer);
        }

        let c = data.sync_state.update(r, m);
        if m {
            gst::log!(
                CAT,
                obj: element,
                "Frames match or no check needed -> {:?}",
                data.sync_state
            );
        } else {
            gst::log!(
                CAT,
                obj: element,
                "Frames mismatch -> {:?}",
                data.sync_state
            );
        }

        if c && data.sync_state.is_synced() {
            // reset sync method and retries once we are synced
            data.on_synced(settings.retries);
            gst::info!(CAT, obj: element, "Synced -> {:?}", data.sync_state);
            // if we receive hints and we are synced tell the hinter
            // to stop looking for pattern matches because we start
            // dropping frames and matching will not work anymore
            if settings.in_hint_mode() {
                let s = gst::Structure::new("ocvrctrl", &[("synced", &true)]);
                self.srcpad
                    .push_event(gst::event::CustomDownstream::builder(s).build());
            }

            // let downstream know about the new caps
            if settings.send_caps {
                let caps = data.synced_caps(settings.drop);
                self.srcpad.push_event(gst::event::Caps::new(&caps));
            }
        }

        // check if we should drop the frame
        let drop = data.sync_state.drop(settings.drop, r);

        // adjust PTS and duration if buffer is not dropped
        if data.sync_state.is_synced() && !drop {
            let pts = buffer.pts();
            let dur = buffer.duration();

            let b = buffer.make_mut();
            if data.sync_state.ts_adjust(r, &mut pts.unwrap()) {
                b.set_pts(pts);
            }

            if data
                .sync_state
                .dur_adjust(r, settings.drop, &mut dur.unwrap())
            {
                b.set_duration(dur);
            }
        }

        if !drop {
            gst::log!(CAT, obj: element, "Fwd frame");
            self.srcpad.push(buffer)
        } else {
            gst::info!(CAT, obj: element, "Drop frame -> {:?}", data.sync_state);
            Ok(gst::FlowSuccess::Ok)
        }
    }

    fn sink_event(&self, pad: &gst::Pad, _element: &super::OcvrCtrl, event: gst::Event) -> bool {
        if let gst::EventView::Caps(e) = event.view() {
            gst::log!(CAT, obj: pad, "Handling event {:?}", e);

            let mut data = self.data.lock().unwrap();
            let settings = self.settings.lock().unwrap();
            data.reset(settings.content_rate, settings.method, settings.retries);

            // Extract the framerate from caps
            let c = e.caps();
            let r = c.structure(0).unwrap().get::<gst::Fraction>("framerate");

            if let Ok(rate) = r {
                data.upstream_caps = c.copy();
                data.capture_rate = match rate.round().numer() {
                    50 => {
                        if settings.capture_rate(CaptureRate::HZ_50) {
                            Some(CaptureRate::HZ_50)
                        } else {
                            None
                        }
                    }
                    60 => {
                        if settings.capture_rate(CaptureRate::HZ_60) {
                            Some(CaptureRate::HZ_60)
                        } else {
                            None
                        }
                    }
                    _ => None,
                };
                gst::log!(CAT, obj: pad, "Input framerate {:?}", data.capture_rate);
            }

            // Extract the format from caps
            let f = c.structure(0).unwrap().get::<&str>("format");
            data.frame_format = match f {
                Ok(f) => Some(VideoFormat::from_string(f)),
                Err(_) => None,
            };
            gst::log!(CAT, obj: pad, "Input format {:?}", data.frame_format);

            // Extract the size from caps
            let w = c.structure(0).unwrap().get::<i32>("width");
            let h = c.structure(0).unwrap().get::<i32>("height");
            if let (Ok(width), Ok(height)) = (w, h) {
                data.frame_size = (width as u32, height as u32);
            } else {
                data.frame_size = (0, 0);
            }
            gst::log!(CAT, obj: pad, "Input size {:?}", data.frame_size);

            // we may have to update the content rate from the capture rate
            data.reset_content_rate(settings.content_rate);
            gst::log!(CAT, obj: pad, "{:?}", data);
        }

        self.srcpad.push_event(event)
    }

    fn sink_query(
        &self,
        _pad: &gst::Pad,
        _element: &super::OcvrCtrl,
        query: &mut gst::QueryRef,
    ) -> bool {
        self.srcpad.peer_query(query)
    }

    fn src_event(&self, pad: &gst::Pad, _element: &super::OcvrCtrl, event: gst::Event) -> bool {
        gst::log!(CAT, obj: pad, "Handling event {:?}", event);

        if let gst::EventView::CustomUpstream(e) = event.view() {
            // Extract the framerate hint
            if let Some(s) = e.structure() {
                if s.name() != "ocvrhint" {
                    return self.sinkpad.push_event(event);
                }

                // do we listen to ocvrhint
                let settings = self.settings.lock().unwrap();
                if !settings.in_hint_mode() {
                    return self.sinkpad.push_event(event);
                }

                let mut data = self.data.lock().unwrap();
                data.reset_on_hint(settings.method, settings.retries);

                data.content_rate = match s.get::<u32>("rate").unwrap() {
                    24 => Some(ContentRate::Hz24),
                    30 => Some(ContentRate::Hz30),
                    60 => Some(ContentRate::Hz60),
                    _ => None,
                };
                gst::info!(
                    CAT,
                    obj: pad,
                    "'ocvrhint' event found, rate {:?}",
                    data.content_rate
                );
                if data.content_rate.is_some() && data.sync_state.is_idle() {
                    data.method_overwrite();
                    data.sync_state = SyncState::sync(data.content_rate.unwrap());
                }

                true
            } else {
                self.sinkpad.push_event(event)
            }
        } else {
            self.sinkpad.push_event(event)
        }
    }

    fn src_query(
        &self,
        _pad: &gst::Pad,
        _element: &super::OcvrCtrl,
        query: &mut gst::QueryRef,
    ) -> bool {
        self.sinkpad.peer_query(query)
    }
}

#[glib::object_subclass]
impl ObjectSubclass for OcvrCtrl {
    const NAME: &'static str = "OcvrCtrl";
    type Type = super::OcvrCtrl;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.pad_template("sink").unwrap();
        let sinkpad = gst::Pad::builder_with_template(&templ, Some("sink"))
            .chain_function(|pad, parent, buffer| {
                OcvrCtrl::catch_panic_pad_function(
                    parent,
                    || Err(gst::FlowError::Error),
                    |ocvr_monitor, element| ocvr_monitor.sink_chain(pad, element, buffer),
                )
            })
            .event_function(|pad, parent, event| {
                OcvrCtrl::catch_panic_pad_function(
                    parent,
                    || false,
                    |ocvr_monitor, element| ocvr_monitor.sink_event(pad, element, event),
                )
            })
            .query_function(|pad, parent, query| {
                OcvrCtrl::catch_panic_pad_function(
                    parent,
                    || false,
                    |ocvr_monitor, element| ocvr_monitor.sink_query(pad, element, query),
                )
            })
            .build();

        let templ = klass.pad_template("src").unwrap();
        let srcpad = gst::Pad::builder_with_template(&templ, Some("src"))
            .event_function(|pad, parent, event| {
                OcvrCtrl::catch_panic_pad_function(
                    parent,
                    || false,
                    |ocvr_monitor, element| ocvr_monitor.src_event(pad, element, event),
                )
            })
            .query_function(|pad, parent, query| {
                OcvrCtrl::catch_panic_pad_function(
                    parent,
                    || false,
                    |ocvr_monitor, element| ocvr_monitor.src_query(pad, element, query),
                )
            })
            .build();

        let settings: Mutex<Settings> = Default::default();
        let data: Mutex<Data> = Default::default();

        Self {
            srcpad,
            sinkpad,
            settings,
            data,
        }
    }
}

impl ObjectImpl for OcvrCtrl {
    fn constructed(&self, obj: &Self::Type) {
        self.parent_constructed(obj);
        obj.add_pad(&self.sinkpad).unwrap();
        obj.add_pad(&self.srcpad).unwrap();
    }

    fn properties() -> &'static [glib::ParamSpec] {
        // Metadata for the properties
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                glib::ParamSpecEnum::new(
                    "content-rate",
                    "Content rate",
                    "Framerate to detect",
                    ContentRate::static_type(),
                    DEFAULT_CONTENT_RATE as i32,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_PLAYING,
                ),
                glib::ParamSpecFlags::new(
                    "capture-rates",
                    "Capture rates",
                    "Sink pad framerates to check",
                    CaptureRate::static_type(),
                    DEFAULT_CAPTURE_RATES.bits() as u32,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_PLAYING,
                ),
                glib::ParamSpecEnum::new(
                    "method",
                    "Check method",
                    "Method to check for duplicate frames",
                    Method::static_type(),
                    DEFAULT_METHOD as i32,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_PLAYING,
                ),
                glib::ParamSpecUInt::new(
                    "threshold",
                    "Threshold",
                    "Compare threshold for method 'fuzzy'",
                    1,
                    (u8::MAX - 1) as u32,
                    DEFAULT_THRESHOLD,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_PLAYING,
                ),
                glib::ParamSpecEnum::new(
                    "tolerance",
                    "Check tolerance",
                    "How to handle failed comparisons",
                    Tolerance::static_type(),
                    DEFAULT_TOLERANCE as i32,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_PLAYING,
                ),
                glib::ParamSpecUInt::new(
                    "retries",
                    "Times to retries check",
                    "Retry times for tolerance 'lazy' or 'strict'",
                    0,
                    100,
                    DEFAULT_RETRIES,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_PLAYING,
                ),
                glib::ParamSpecUInt::new(
                    "rows",
                    "Rows to consider",
                    "Number of pixel rows considered for 'fuzzy' compare",
                    0,
                    u32::MAX - 1,
                    DEFAULT_ROWS,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_PLAYING,
                ),
                glib::ParamSpecBoolean::new(
                    "drop",
                    "Make 30Hz from 60Hz",
                    "Change 60Hz capture rate to 30Hz always",
                    DEFAULT_DROP as bool,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_PLAYING,
                ),
                glib::ParamSpecBoolean::new(
                    "send-caps",
                    "Send caps event on rate change",
                    "Send caps event downstream on content rate change",
                    DEFAULT_SEND_CAPS as bool,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_PLAYING,
                ),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(
        &self,
        _obj: &Self::Type,
        _id: usize,
        value: &glib::Value,
        pspec: &glib::ParamSpec,
    ) {
        let mut settings = self.settings.lock().unwrap();
        match pspec.name() {
            "content-rate" => {
                let rate = value.get().expect("type checked upstream");
                settings.content_rate = rate;
                let mut data = self.data.lock().unwrap();
                data.sync_state = SyncState::sync(rate);
            }
            "capture-rates" => {
                let rates = value.get().expect("type checked upstream");
                settings.capture_rates = rates;
            }
            "method" => {
                let method = value.get().expect("type checked upstream");
                settings.method = method;
            }
            "threshold" => {
                let threshold = value.get().expect("type checked upstream");
                settings.threshold = threshold;
            }
            "tolerance" => {
                let tolerance = value.get().expect("type checked upstream");
                settings.tolerance = tolerance;
            }
            "retries" => {
                let retries = value.get().expect("type checked upstream");
                settings.retries = retries;
            }
            "rows" => {
                let rows = value.get().expect("type checked upstream");
                settings.rows = rows;
            }
            "drop" => {
                let drop = value.get().expect("type checked upstream");
                settings.drop = drop;
            }
            "send-caps" => {
                let sc = value.get().expect("type checked upstream");
                settings.send_caps = sc;
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _obj: &Self::Type, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();
        match pspec.name() {
            "content-rate" => settings.content_rate.to_value(),
            "capture-rates" => settings.capture_rates.to_value(),
            "method" => settings.method.to_value(),
            "threshold" => (settings.threshold).to_value(),
            "tolerance" => settings.tolerance.to_value(),
            "retries" => (settings.retries as u32).to_value(),
            "rows" => (settings.rows as u32).to_value(),
            "drop" => settings.drop.to_value(),
            "send-caps" => settings.send_caps.to_value(),
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for OcvrCtrl {}

impl ElementImpl for OcvrCtrl {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "Original content video rate controller",
                "Parser/Video",
                "Precise check estimated framerate",
                "Jochen Henneberg <jh@henneberg-systemdesign.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let caps = gst::Caps::builder("video/x-raw")
                .field(
                    "format",
                    gst::List::new([VideoFormat::I420.to_str(), VideoFormat::Nv12.to_str()]),
                )
                .build();

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

    fn change_state(
        &self,
        element: &Self::Type,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst::trace!(CAT, obj: element, "Changing state {:?}", transition);
        self.parent_change_state(element, transition)
    }
}
