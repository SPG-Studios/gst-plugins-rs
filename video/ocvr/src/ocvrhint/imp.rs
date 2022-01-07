// Copyright (C) 2021 Jochen Henneberg <jh@henneberg-systemdesign.com>
//
// SPDX-License-Identifier: Apache-2.0 or MIT

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst::{gst_log, gst_trace};

use std::collections::BTreeMap;
use std::sync::Mutex;

use once_cell::sync::Lazy;

mod correlation;
mod rateprobe;

use correlation::Corr;
use rateprobe::RateProbe;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "ocvrhint",
        gst::DebugColorFlags::empty(),
        Some("Original content video rate hinter"),
    )
});

// Original content framerates to detect - the order matters, the
// correlation is checked with increasing value
#[glib::flags(name = "OcvrHintRate")]
enum Rate {
    #[flags_value(name = "24Hz", nick = "24")]
    HZ_24 = 0b00000001,
    #[flags_value(name = "30Hz", nick = "30")]
    HZ_30 = 0b00000010,
    #[flags_VALUE(name = "60Hz", nick = "60")]
    HZ_60 = 0b00000100,
}

// Capture framerates to check
#[glib::flags(name = "OcvrHintProbeRate")]
enum ProbeRate {
    #[flags_value(name = "30Hz", nick = "30")]
    HZ_30 = 0b00000001,
    #[flags_value(name = "50Hz", nick = "50")]
    HZ_50 = 0b00000010,
    #[flags_value(name = "60Hz", nick = "60")]
    HZ_60 = 0b00000100,
}

// Test vectors for different framerates at capture rates
const HZ24_IN_HZ30: &[&[i64]] = &[&[1, 1], &[1, 1]];
const HZ30_IN_HZ30: &[&[i64]] = &[&[1, 1], &[1, 1]];

const HZ24_IN_HZ50: &[&[i64]] = &[&[10, 1], &[1, 10]];
const HZ30_IN_HZ50: &[&[i64]] = &[&[10, 1], &[1, 10]];

const HZ24_IN_HZ60: &[&[i64]] = &[&[10, 1, 10, 1, 1], &[10, 1, 1, 10, 1]];
const HZ30_IN_HZ60: &[&[i64]] = &[&[10, 1], &[1, 10]];
const HZ60_IN_HZ60: &[&[i64]] = &[&[1, 1], &[1, 1]];

// Default values of properties
const DEFAULT_WINDOW_SIZE: usize = 4;
const DEFAULT_THRESHOLD: f64 = 0.9;
const DEFAULT_RATES: Rate = Rate::all();
const DEFAULT_PROBE_RATES: ProbeRate = ProbeRate::all();

// Property value storage
#[derive(Debug, Clone, Copy)]
struct Settings {
    window_size: usize,
    threshold: f64,
    rates: Rate,
    probe_rates: ProbeRate,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            window_size: DEFAULT_WINDOW_SIZE,
            threshold: DEFAULT_THRESHOLD,
            rates: DEFAULT_RATES,
            probe_rates: DEFAULT_PROBE_RATES,
        }
    }
}

// Runtime value storage
#[derive(Debug)]
struct Data {
    window: Vec<i64>,
    gop_count: usize,
    gop_size: Option<usize>,
    probes: BTreeMap<Rate, RateProbe>,
    rate: Option<ProbeRate>,
    content_rate: Option<Rate>,
    frame_counter: usize,
}

impl Data {
    pub fn reset_window(&mut self) {
        self.window.clear();
        self.gop_count = 0;
    }

    pub fn reset(&mut self) {
        self.reset_window();
        self.gop_size = None;
        self.probes.clear();
        self.rate = None;
        self.content_rate = None;
        self.frame_counter = 0;
    }
}

impl Default for Data {
    fn default() -> Self {
        Data {
            window: vec![],
            gop_count: 0,
            gop_size: None,
            probes: BTreeMap::new(),
            rate: None,
            content_rate: None,
            frame_counter: 0,
        }
    }
}

pub struct OcvrHint {
    srcpad: gst::Pad,
    sinkpad: gst::Pad,
    settings: Mutex<Settings>,
    data: Mutex<Data>,
}

impl OcvrHint {
    fn check_rate(window: &Vec<i64>, gop_size: usize, threshold: f64, probe: &RateProbe) -> bool {
        let mut res: f64 = 0.0;
        let mut corr: Corr = Default::default();

        for v in probe.iter() {
            corr.set_x(v);
            window.chunks(gop_size).all(|wc| {
                res = match corr.corr_y(&wc) {
                    Some(c) => res.max(c),
                    None => res,
                };
                res < threshold
            });
        }
        res > threshold
    }

    fn build_probes(rate: ProbeRate, gop_size: usize, probes: &mut BTreeMap<Rate, RateProbe>) {
        match rate {
            ProbeRate::HZ_30 => {
                probes.insert(Rate::HZ_24, RateProbe::new(gop_size, HZ24_IN_HZ30));
                probes.insert(Rate::HZ_30, RateProbe::new(gop_size, HZ30_IN_HZ30));
            }
            ProbeRate::HZ_50 => {
                probes.insert(Rate::HZ_24, RateProbe::new(gop_size, HZ24_IN_HZ50));
                probes.insert(Rate::HZ_30, RateProbe::new(gop_size, HZ30_IN_HZ50));
            }
            ProbeRate::HZ_60 => {
                probes.insert(Rate::HZ_24, RateProbe::new(gop_size, HZ24_IN_HZ60));
                probes.insert(Rate::HZ_30, RateProbe::new(gop_size, HZ30_IN_HZ60));
                probes.insert(Rate::HZ_60, RateProbe::new(gop_size, HZ60_IN_HZ60));
            }
            _ => unimplemented!(),
        }
    }

    fn rate_to_int(r: Option<Rate>) -> u32 {
        if r.is_none() {
            return 0;
        }

        match r.unwrap() {
            Rate::HZ_24 => return 24,
            Rate::HZ_30 => return 30,
            Rate::HZ_60 => return 60,
            _ => unreachable!(),
        }
    }

    fn sink_chain(
        &self,
        pad: &gst::Pad,
        element: &super::OcvrHint,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut data = self.data.lock().unwrap();

        // If this is an input data rate that we should not care about
        // just forward the buffer
        if data.rate.is_none() {
            return self.srcpad.push(buffer);
        }

        // If we should not check for this input rate just forward the
        // buffer
        let settings = self.settings.lock().unwrap();
        if !settings.probe_rates.contains(data.rate.unwrap()) {
            return self.srcpad.push(buffer);
        }

        // If this is not a reference frame advance the frame counter,
        // remember the buffer size and push the buffer
        if buffer.flags().contains(gst::BufferFlags::DELTA_UNIT) {
            data.window.push(buffer.size() as i64);
            data.frame_counter += 1;
            return self.srcpad.push(buffer);
        }

        // We have a reference frame - let's go

        // If the GOP size changed or we didn't have one set it
        if data.frame_counter > 0
            && (data.gop_size.is_none() || data.gop_size.unwrap() != data.frame_counter)
        {
            let s = data.frame_counter;
            data.gop_size = Some(s);
            gst_log!(CAT, obj: pad, "Found GOP size {:?}", s);

            // Reserve enough space for the window vector
            let w = s * settings.window_size;
            data.window.reserve(w);

            // If we have an input frame rate that we should probe
            // let's set the probe vectors
            Self::build_probes(data.rate.unwrap(), data.gop_size.unwrap(), &mut data.probes);
        }

        // Advance the GOP counter - might be reset later from
        // reset_window() if we have collected enough GOPs to satisfy
        // window_size
        if data.frame_counter != 0 {
            data.gop_count += 1;
        }

        // If the window is complete check for framerate matches
        let mut m: Option<Rate> = None;
        if data.gop_count == settings.window_size {
            gst_trace!(
                CAT,
                obj: pad,
                "Processing {:?} GOPs in data window of size {:?}",
                data.gop_count,
                data.window.len()
            );

            for (r, p) in data.probes.iter() {
                if !settings.rates.contains(*r) {
                    continue;
                }

                gst_trace!(
                    CAT,
                    obj: pad,
                    "Correlate {:?} GOPs for rate {:?}",
                    data.gop_count,
                    r
                );
                if !Self::check_rate(&data.window, data.gop_size.unwrap(), settings.threshold, &p) {
                    continue;
                }

                // As soon as we have a match we can leave
                m = Some(*r);
                break;
            }

            if m != data.content_rate {
                gst_log!(
                    CAT,
                    obj: pad,
                    "Original content frame rate changed to {:?}",
                    m
                );
                data.content_rate = m;

                // Now send message with newly detected original content rate
                let r = Self::rate_to_int(m);
                let s = gst::Structure::new("ocvr", &[("rate", &r)]);
                let _ =
                    element.post_message(gst::message::Element::builder(s).src(element).build());
            }

            // Correlation done, reset window
            data.reset_window();
        }

        // A new GOP started or the current GOP is complete so the
        // frame counter can be reset
        data.frame_counter = 1;

        // Remember the buffer size
        data.window.push(buffer.size() as i64);

        self.srcpad.push(buffer)
    }

    fn sink_event(&self, pad: &gst::Pad, _element: &super::OcvrHint, event: gst::Event) -> bool {
        let mut data = self.data.lock().unwrap();
        match event.view() {
            gst::EventView::Caps(e) => {
                gst_log!(CAT, obj: pad, "Handling event {:?}", event);
                data.reset();

                // Extract the framerate from caps
                let c = e.caps();
                let r = c.structure(0).unwrap().get::<gst::Fraction>("framerate");
                if r.is_ok() {
                    data.rate = match r.unwrap().round().numer() {
                        30 => Some(ProbeRate::HZ_30),
                        50 => Some(ProbeRate::HZ_50),
                        60 => Some(ProbeRate::HZ_60),
                        _ => None,
                    };
                    gst_log!(CAT, obj: pad, "Input framerate {:?}", data.rate);
                }
            }
            _ => (),
        }
        self.srcpad.push_event(event)
    }

    fn sink_query(
        &self,
        pad: &gst::Pad,
        _element: &super::OcvrHint,
        query: &mut gst::QueryRef,
    ) -> bool {
        gst_log!(CAT, obj: pad, "Handling query {:?}", query);
        let ret = match query.view_mut() {
            gst::QueryView::Caps(ref mut q) => {
                let pad_caps = self.sinkpad.pad_template_caps();
                let caps = q
                    .filter()
                    .map(|f| {
                        f.intersect_with_mode(pad_caps.as_ref(), gst::CapsIntersectMode::First)
                    })
                    .unwrap_or_else(|| pad_caps.clone());

                q.set_result(&caps);
                true
            }
            _ => self.srcpad.peer_query(query),
        };
        ret
    }

    fn src_event(&self, _pad: &gst::Pad, _element: &super::OcvrHint, event: gst::Event) -> bool {
        self.sinkpad.push_event(event)
    }

    fn src_query(
        &self,
        _pad: &gst::Pad,
        _element: &super::OcvrHint,
        query: &mut gst::QueryRef,
    ) -> bool {
        self.sinkpad.peer_query(query)
    }
}

#[glib::object_subclass]
impl ObjectSubclass for OcvrHint {
    const NAME: &'static str = "OcvrHint";
    type Type = super::OcvrHint;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.pad_template("sink").unwrap();
        let sinkpad = gst::Pad::builder_with_template(&templ, Some("sink"))
            .chain_function(|pad, parent, buffer| {
                OcvrHint::catch_panic_pad_function(
                    parent,
                    || Err(gst::FlowError::Error),
                    |ocvr_hint, element| ocvr_hint.sink_chain(pad, element, buffer),
                )
            })
            .event_function(|pad, parent, event| {
                OcvrHint::catch_panic_pad_function(
                    parent,
                    || false,
                    |ocvr_hint, element| ocvr_hint.sink_event(pad, element, event),
                )
            })
            .query_function(|pad, parent, query| {
                OcvrHint::catch_panic_pad_function(
                    parent,
                    || false,
                    |ocvr_hint, element| ocvr_hint.sink_query(pad, element, query),
                )
            })
            .build();

        let templ = klass.pad_template("src").unwrap();
        let srcpad = gst::Pad::builder_with_template(&templ, Some("src"))
            .event_function(|pad, parent, event| {
                OcvrHint::catch_panic_pad_function(
                    parent,
                    || false,
                    |ocvr_hint, element| ocvr_hint.src_event(pad, element, event),
                )
            })
            .query_function(|pad, parent, query| {
                OcvrHint::catch_panic_pad_function(
                    parent,
                    || false,
                    |ocvr_hint, element| ocvr_hint.src_query(pad, element, query),
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

impl ObjectImpl for OcvrHint {
    fn constructed(&self, obj: &Self::Type) {
        self.parent_constructed(obj);
        obj.add_pad(&self.sinkpad).unwrap();
        obj.add_pad(&self.srcpad).unwrap();
    }

    fn properties() -> &'static [glib::ParamSpec] {
        // Metadata for the properties
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                glib::ParamSpecUInt64::new(
                    "window-size",
                    "Window size",
                    "Multiple of GOP size",
                    0,
                    100,
                    DEFAULT_WINDOW_SIZE as u64,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_PLAYING,
                ),
                glib::ParamSpecFloat::new(
                    "threshold",
                    "Threshold",
                    "Framerate detect threshold",
                    0.0,
                    1.0,
                    DEFAULT_THRESHOLD as f32,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_PLAYING,
                ),
                glib::ParamSpecFlags::new(
                    "rates",
                    "Rates",
                    "Framerates to detect",
                    Rate::static_type(),
                    DEFAULT_RATES.bits() as u32,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_PLAYING,
                ),
                glib::ParamSpecFlags::new(
                    "probe-rates",
                    "Probe rates",
                    "Sink pad framerates to check",
                    ProbeRate::static_type(),
                    DEFAULT_PROBE_RATES.bits() as u32,
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
            "window-size" => {
                let ws: u32 = value.get().expect("type checked upstream");
                settings.window_size = ws as usize;
            }
            "threshold" => {
                let threshold = value.get().expect("type checked upstream");
                settings.threshold = threshold;
            }
            "rates" => {
                let rates = value.get().expect("type checked upstream");
                settings.rates = rates;
            }
            "probe-rates" => {
                let probe_rates = value.get().expect("type checked upstream");
                settings.probe_rates = probe_rates;
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _obj: &Self::Type, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();
        match pspec.name() {
            "window-size" => (settings.window_size as u32).to_value(),
            "threshold" => settings.threshold.to_value(),
            "rates" => settings.rates.to_value(),
            "probe-rates" => settings.probe_rates.to_value(),
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for OcvrHint {}

impl ElementImpl for OcvrHint {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "Original content video rate hinter",
                "Parser/Video",
                "Detects the original content framerate from encoded video",
                "Jochen Henneberg <jh@henneberg-systemdesign.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let mut caps = gst::Caps::new_empty();
            {
                let caps = caps.get_mut().unwrap();

                caps.append(
                    gst::Caps::builder("video/x-h265")
                        .field("alignment", "au")
                        .build(),
                );
                caps.append(
                    gst::Caps::builder("video/x-h264")
                        .field("alignment", "au")
                        .build(),
                );
            }

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
        gst_trace!(CAT, obj: element, "Changing state {:?}", transition);
        self.parent_change_state(element, transition)
    }
}
