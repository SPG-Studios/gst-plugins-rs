// Copyright (C) 2022 Jochen Henneberg <jh@henneberg-systemdesign.com>
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;

use std::collections::BTreeMap;
use std::sync::Mutex;

use once_cell::sync::Lazy;

mod correlation;
use correlation::Corr;

mod rateprobe;
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
#[glib::flags(name = "OcvrHintContentRate")]
pub enum ContentRate {
    #[flags_value(name = "Content rate 24Hz (60Hz capture rate only)", nick = "24Hz")]
    HZ_24 = 0b00000001,
    #[flags_value(name = "Content rate 25Hz (50Hz capture rate only)", nick = "25Hz")]
    HZ_25 = 0b00000010,
    #[flags_value(name = "Content rate 30Hz", nick = "30Hz")]
    HZ_30 = 0b00000100,
    #[flags_value(name = "Content rate 50Hz (50Hz capture rate only)", nick = "50Hz")]
    HZ_50 = 0b00001000,
    #[flags_value(name = "Content rate 60Hz (60Hz capture rate only)", nick = "60Hz")]
    HZ_60 = 0b00010000,
}

// Capture framerates to check
#[glib::flags(name = "OcvrHintCaptureRate")]
pub enum CaptureRate {
    #[flags_value(name = "Capture rate 50Hz", nick = "50Hz")]
    HZ_50 = 0b00000001,
    #[flags_value(name = "Capture rate 60Hz", nick = "60Hz")]
    HZ_60 = 0b00000010,
}

// Test vectors for different framerates at capture rates
const HZ25_IN_HZ50: &[i64] = &[10, 1];
const HZ30_IN_HZ50: &[i64] = &[10, 1, 10, 1, 10];
const HZ50_IN_HZ50: &[i64] = &[1, 1];

const HZ24_IN_HZ60: &[i64] = &[10, 1, 10, 1, 1];
const HZ30_IN_HZ60: &[i64] = &[10, 1];
const HZ60_IN_HZ60: &[i64] = &[1, 1];

// Default values of properties
const DEFAULT_WINDOW_SIZE: usize = 4;
const DEFAULT_THRESHOLD: f64 = 0.9;
const DEFAULT_CONTENT_RATES: ContentRate = ContentRate::all();
const DEFAULT_CAPTURE_RATES: CaptureRate = CaptureRate::all();

// Property value storage
#[derive(Debug, Clone, Copy)]
struct Settings {
    window_size: usize,
    threshold: f64,
    content_rates: ContentRate,
    capture_rates: CaptureRate,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            window_size: DEFAULT_WINDOW_SIZE,
            threshold: DEFAULT_THRESHOLD,
            content_rates: DEFAULT_CONTENT_RATES,
            capture_rates: DEFAULT_CAPTURE_RATES,
        }
    }
}

// Runtime value storage
#[derive(Default, Debug)]
struct Data {
    window: Vec<i64>,
    gop_count: usize,
    gop_size: Option<usize>,
    probes: BTreeMap<ContentRate, RateProbe>,
    rate: Option<CaptureRate>,
    content_rate: Option<ContentRate>,
    frame_counter: usize,
    pause: bool,
}

impl Data {
    pub fn reset_window(&mut self) {
        self.window.clear();
        self.gop_count = 0;
    }

    pub fn set_pause(&mut self, pause: bool) {
        if !pause {
            self.reset_window();
            self.gop_size.take();
            self.probes.clear();
            self.content_rate.take();
            self.frame_counter = 0;
            self.pause = false;
        } else {
            self.pause = true;
            self.content_rate.take();
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
    fn check_rate(window: &[i64], gop_size: usize, threshold: f64, probe: &RateProbe) -> bool {
        let mut res: f64 = 0.0;
        let mut corr = Corr::default();

        for v in probe.iter() {
            corr.set_x(v);
            //gst::log!(CAT, "Probe {:?}", v);
            window.chunks(gop_size).all(|wc| {
                res = match corr.corr_y(wc) {
                    Some(c) => res.max(c),
                    None => res,
                };
                //gst::log!(CAT, "Window {:?} -> {:?}", wc, res);
                res < threshold
            });

            if res > threshold {
                break;
            }
        }
        res > threshold
    }

    fn build_probes(
        rate: CaptureRate,
        gop_size: usize,
        probes: &mut BTreeMap<ContentRate, RateProbe>,
    ) {
        match rate {
            CaptureRate::HZ_50 => {
                probes.insert(ContentRate::HZ_25, RateProbe::new(gop_size, HZ25_IN_HZ50));
                probes.insert(ContentRate::HZ_30, RateProbe::new(gop_size, HZ30_IN_HZ50));
                probes.insert(ContentRate::HZ_50, RateProbe::new(gop_size, HZ50_IN_HZ50));
            }
            CaptureRate::HZ_60 => {
                probes.insert(ContentRate::HZ_24, RateProbe::new(gop_size, HZ24_IN_HZ60));
                probes.insert(ContentRate::HZ_30, RateProbe::new(gop_size, HZ30_IN_HZ60));
                probes.insert(ContentRate::HZ_60, RateProbe::new(gop_size, HZ60_IN_HZ60));
            }
            _ => unimplemented!(),
        }
    }

    fn rate_to_int(r: Option<ContentRate>) -> u32 {
        if r.is_none() {
            return 0;
        }

        match r.unwrap() {
            ContentRate::HZ_24 => 24,
            ContentRate::HZ_25 => 25,
            ContentRate::HZ_30 => 30,
            ContentRate::HZ_50 => 50,
            ContentRate::HZ_60 => 60,
            _ => unreachable!(),
        }
    }

    fn sink_chain(
        &self,
        pad: &gst::Pad,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut data = self.data.lock().unwrap();

        // If this is an input data rate that we should not care about
        // just forward the buffer
        if data.rate.is_none() || data.pause {
            drop(data);
            return self.srcpad.push(buffer);
        }

        // If we should not check for this input rate just forward the
        // buffer
        let settings = self.settings.lock().unwrap();
        if !settings.capture_rates.contains(data.rate.unwrap()) {
            drop(data);
            drop(settings);
            return self.srcpad.push(buffer);
        }

        // If this is not a reference frame advance the frame counter,
        // remember the buffer size and push the buffer
        if buffer.flags().contains(gst::BufferFlags::DELTA_UNIT) {
            if !data.window.is_empty() {
                data.window.push(buffer.size() as i64);
                data.frame_counter += 1;
            }
            drop(data);
            drop(settings);
            return self.srcpad.push(buffer);
        }

        // We have a reference frame - let's go

        // If the GOP size changed or we didn't have one set it
        if data.frame_counter > 0
            && data
                .gop_size
                .map_or(true, |gop_size| gop_size != data.frame_counter)
        {
            let s = data.frame_counter;
            data.gop_size = Some(s);
            gst::log!(CAT, obj: pad, "Found GOP size {:?}", s);

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
        let mut m: Option<ContentRate> = None;
        let mut hint = None;
        if data.gop_count == settings.window_size {
            gst::trace!(
                CAT,
                obj: pad,
                "Processing {:?} GOPs in data window of size {:?}",
                data.gop_count,
                data.window.len()
            );

            for (r, p) in data.probes.iter() {
                if !settings.content_rates.contains(*r) {
                    continue;
                }

                gst::trace!(
                    CAT,
                    obj: pad,
                    "Correlate {:?} GOPs for rate {:?}",
                    data.gop_count,
                    r
                );
                gst::trace!(CAT, obj: pad, "Window: {:?}", &data.window);
                if !Self::check_rate(&data.window, data.gop_size.unwrap(), settings.threshold, p) {
                    continue;
                }

                // As soon as we have a match we can leave
                m = Some(*r);
                break;
            }

            if m != data.content_rate {
                data.content_rate = m;

                // Send a custom upstream event with the newly detected original content rate
                let r = Self::rate_to_int(m);
                hint = Some(gst::Structure::builder("ocvrhint").field("rate", r).build());
                gst::log!(
                    CAT,
                    obj: pad,
                    "Original content frame rate changed to {:?}",
                    m
                );
            }

            // Correlation done, reset window
            data.reset_window();
        }

        // A new GOP started or the current GOP is complete so the
        // frame counter can be reset
        data.frame_counter = 1;

        // Remember the buffer size
        data.window.push(buffer.size() as i64);

        drop(data);
        drop(settings);
        if let Some(s) = hint {
            self.sinkpad
                .push_event(gst::event::CustomUpstream::builder(s).build());
        }
        self.srcpad.push(buffer)
    }

    fn sink_event(&self, pad: &gst::Pad, event: gst::Event) -> bool {
        match event.view() {
            gst::EventView::Caps(e) => {
                gst::log!(CAT, obj: pad, "Handling event {:?}", event);

                // Extract the framerate from caps
                let c = e.caps();
                let r = c.structure(0).unwrap().get::<gst::Fraction>("framerate");
                if let Ok(cr) = r {
                    let n = *cr.round().numer();
                    let rate = match n {
                        50 => Some(CaptureRate::HZ_50),
                        60 => Some(CaptureRate::HZ_60),
                        _ => None,
                    };
                    gst::log!(CAT, obj: pad, "Input framerate {:?} from {:?}", rate, n);

                    let mut data = self.data.lock().unwrap();
                    if rate != data.rate {
                        gst::info!(
                            CAT,
                            obj: pad,
                            "Input frame rate changed - {:?} -> {:?}",
                            data.rate,
                            rate
                        );
                        *data = Data::default();
                        data.rate = rate;
                    }
                }
            }
            gst::EventView::CustomDownstream(e) => {
                // Extract the controller state event
                if let Some(s) = e.structure() {
                    if s.name() == "ocvrctrl" {
                        let mut data = self.data.lock().unwrap();
                        data.set_pause(s.get::<bool>("synced").unwrap());
                        gst::info!(
                            CAT,
                            obj: pad,
                            "'ocvrctrl' event found, synced {:?}",
                            data.pause
                        );
                    }
                }
            }
            _ => {}
        }
        self.srcpad.push_event(event)
    }
}

#[glib::object_subclass]
impl ObjectSubclass for OcvrHint {
    const NAME: &'static str = "GstOcvrHint";
    type Type = super::OcvrHint;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.pad_template("sink").unwrap();
        let sinkpad = gst::Pad::builder_from_template(&templ)
            .chain_function(|pad, parent, buffer| {
                OcvrHint::catch_panic_pad_function(
                    parent,
                    || Err(gst::FlowError::Error),
                    |ocvr_hint| ocvr_hint.sink_chain(pad, buffer),
                )
            })
            .event_function(|pad, parent, event| {
                OcvrHint::catch_panic_pad_function(
                    parent,
                    || false,
                    |ocvr_hint| ocvr_hint.sink_event(pad, event),
                )
            })
            .build();

        let templ = klass.pad_template("src").unwrap();
        let srcpad = gst::Pad::builder_from_template(&templ).build();

        let settings: Mutex<Settings> = Default::default();
        let data = Mutex::<Data>::default();

        Self {
            srcpad,
            sinkpad,
            settings,
            data,
        }
    }
}

impl ObjectImpl for OcvrHint {
    fn constructed(&self) {
        self.parent_constructed();

        let obj = self.obj();
        obj.add_pad(&self.sinkpad).unwrap();
        obj.add_pad(&self.srcpad).unwrap();
    }

    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                glib::ParamSpecUInt::builder("window-size")
                    .nick("Window size")
                    .blurb("Multiple of GOP size")
                    .minimum(1)
                    .maximum(100)
                    .default_value(DEFAULT_WINDOW_SIZE as u32)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecFloat::builder("threshold")
                    .nick("Threshold")
                    .blurb("Framerate detect threshold")
                    .minimum(0.0)
                    .maximum(1.0)
                    .default_value(DEFAULT_THRESHOLD as f32)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecFlags::builder::<ContentRate>("content-rates")
                    .nick("Content rates")
                    .blurb("Framerates to detect")
                    .default_value(ContentRate {
                        bits: DEFAULT_CONTENT_RATES.bits(),
                    })
                    .mutable_playing()
                    .build(),
                glib::ParamSpecFlags::builder::<CaptureRate>("capture-rates")
                    .nick("Capture rates")
                    .blurb("Sink pad framerates to check")
                    .default_value(CaptureRate {
                        bits: DEFAULT_CAPTURE_RATES.bits(),
                    })
                    .mutable_playing()
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let mut settings = self.settings.lock().unwrap();
        match pspec.name() {
            "window-size" => {
                let ws: u32 = value.get().expect("type checked upstream");
                settings.window_size = ws as usize;
            }
            "threshold" => {
                let threshold: f32 = value.get().expect("type checked upstream");
                settings.threshold = threshold as f64;
            }
            "content-rates" => {
                let rates = value.get().expect("type checked upstream");
                settings.content_rates = rates;
            }
            "capture-rates" => {
                let rates = value.get().expect("type checked upstream");
                settings.capture_rates = rates;
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();
        match pspec.name() {
            "window-size" => (settings.window_size as u32).to_value(),
            "threshold" => (settings.threshold as f32).to_value(),
            "content-rates" => settings.content_rates.to_value(),
            "capture-rates" => settings.capture_rates.to_value(),
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
}
