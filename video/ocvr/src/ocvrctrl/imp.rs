use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst::{gst_log, gst_trace};

use std::cmp;
use std::sync::Mutex;

use crc::{Crc, CRC_32_ISCSI};
use gst_video::video_frame::*;
use gst_video::{VideoFormat, VideoInfo};
use once_cell::sync::Lazy;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "ocvrctrl",
        gst::DebugColorFlags::empty(),
        Some("Original content video rate controller"),
    )
});

const CASTAGNOLI: Crc<u32> = Crc::<u32>::new(&CRC_32_ISCSI);

// Original content framerates to detect - the order matters, the
// correlation is checked with increasing value
#[glib::flags(name = "OcvrCtrlRate")]
enum Rate {
    #[flags_value(name = "24Hz", nick = "24")]
    HZ_24 = 0b00000001,
    #[flags_value(name = "30Hz", nick = "30")]
    HZ_30 = 0b00000010,
    #[flags_value(name = "60Hz", nick = "60")]
    HZ_60 = 0b00000100,
    #[flags_value(name = "auto", nick = "auto")]
    AUTO = 0b00001000,
}

// Capture framerates to check
#[glib::flags(name = "OcvrCtrlProbeRate")]
enum ProbeRate {
    #[flags_value(name = "30Hz", nick = "30")]
    HZ_30 = 0b00000001,
    #[flags_value(name = "50Hz", nick = "50")]
    HZ_50 = 0b00000010,
    #[flags_value(name = "60Hz", nick = "60")]
    HZ_60 = 0b00000100,
}

// Method used to check rate
#[glib::flags(name = "OcvrCtrlMethod")]
enum Method {
    #[flags_value(name = "auto", nick = "auto")]
    AUTO = 0b00000001,
    #[flags_value(name = "checksum", nick = "checksum")]
    CHKSUM = 0b00000010,
    #[flags_value(name = "fuzzy", nick = "fuzzy")]
    FUZZY = 0b00000100,
}

// Method used to check rate
#[glib::flags(name = "OcvrCtrlTolerance")]
enum Tolerance {
    #[flags_value(name = "lazy", nick = "lazy")]
    LAZY = 0b00000001,
    #[flags_value(name = "strict", nick = "strict")]
    STRICT = 0b00000010,
    #[flags_value(name = "paranoid", nick = "paranoid")]
    PARANOID = 0b00000100,
}

// Default values of properties
const DEFAULT_RATES: Rate = Rate::AUTO;
const DEFAULT_PROBE_RATES: ProbeRate = ProbeRate::all();
const DEFAULT_METHOD: Method = Method::AUTO;
const DEFAULT_THRESHOLD: f32 = 0.0;
const DEFAULT_TOLERANCE: Tolerance = Tolerance::LAZY;
const DEFAULT_REPEAT: u32 = 3;
const DEFAULT_ROWS: u32 = 10;
const DEFAULT_DROP: bool = false;

// Property value storage
#[derive(Debug, Clone, Copy)]
struct Settings {
    rates: Rate,
    probe_rates: ProbeRate,
    method: Method,
    threshold: f32,
    tolerance: Tolerance,
    repeat: u32,
    rows: u32,
    drop: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            rates: DEFAULT_RATES,
            probe_rates: DEFAULT_PROBE_RATES,
            method: DEFAULT_METHOD,
            threshold: DEFAULT_THRESHOLD,
            tolerance: DEFAULT_TOLERANCE,
            repeat: DEFAULT_REPEAT,
            rows: DEFAULT_ROWS,
            drop: DEFAULT_DROP,
        }
    }
}

#[derive(Debug, Clone)]
enum SyncState {
    Syncing,
    Hz24 { pos: u32 },
    Hz30 { pos: u32 },
    Hz60,
}

// Runtime value storage
#[derive(Debug, Clone)]
struct Data {
    ping_window: Vec<u8>,
    pong_window: Vec<u8>,
    is_ping: bool,
    sync_state: SyncState,
    method: Method,
    repeat: u32,
}

impl Default for Data {
    fn default() -> Self {
        Data {
            ping_window: vec![],
            pong_window: vec![],
            sync_state: SyncState::Syncing,
            is_ping: true,
            method: Method::AUTO,
            repeat: 0,
        }
    }
}

impl Data {
    pub fn reset(&mut self) {
    }
}

pub struct OcvrCtrl {
    srcpad: gst::Pad,
    sinkpad: gst::Pad,
    settings: Mutex<Settings>,
    data: Mutex<Data>,
}

impl OcvrCtrl {
    fn compare_checksum(&self) -> bool {
        true
    }

    fn compare_fuzzy(&self) -> bool {
        true
    }

    fn sink_chain(
        &self,
        pad: &gst::Pad,
        _element: &super::OcvrCtrl,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut drop = true;
        {
            let info = VideoInfo::builder(VideoFormat::I420, 3840, 2160)
                .build()
                .unwrap();
            let frame =
                VideoFrameRef::<&gst::BufferRef>::from_buffer_ref_readable(&buffer, &info).unwrap();
            let frame_data = frame.plane_data(0).unwrap();
            let width = frame.width() as usize;
            let height = frame.height() as usize;

            // let crc = CASTAGNOLI.checksum(&frame_data[0..width / 10 * height / 10]);
            // gst_log!(CAT, obj: pad, "Checksum {:?}", crc);
            // let crc = CASTAGNOLI;
            // let mut digest = crc.digest();
            // let from_to_height = height / 2 - height / 20..height / 2 + height / 20;
            // for height in from_to_height {
            //     let offset = height * width + width / 2;
            //     let from_to_width = offset - width / 20..offset + width / 20;
            //     digest.update(&frame_data[from_to_width]);
            // }
            // gst_log!(CAT, obj: pad, "Checksum {:?}", digest.finalize());

            let mut data = self.data.lock().unwrap();
            let is_ping = data.is_ping;
            let mut win = &mut data.pong_window;
            if is_ping {
                win = &mut data.ping_window;
            }

            win.clear();
            for height in (0..height).step_by(height / 10) {
                let offset = height * width;
                win.extend_from_slice(&frame_data[offset..offset + width]);
            }

            if data.ping_window.len() > 0 && data.pong_window.len() > 0 {
                let it = data.ping_window.iter();
                data.pong_window.iter().zip(it).all(|(a, b)| {
                    let ax = cmp::max(a, b) >> 4;
                    let bx = cmp::min(a, b) >> 4;
                    let d = ax - bx;
                    if d > 1 {
                        drop = false
                    }
                    drop
                });
                gst_log!(CAT, obj: pad, "Drop? {:?}", drop);
            }

            data.is_ping = !data.is_ping;
        }
        if !drop {
            self.srcpad.push(buffer)
        } else {
            Ok(gst::FlowSuccess::Ok)
        }
    }

    fn sink_event(&self, pad: &gst::Pad, _element: &super::OcvrCtrl, event: gst::Event) -> bool {
        gst_log!(CAT, obj: pad, "Handling event {:?}", event);
        self.srcpad.push_event(event)
    }

    fn sink_query(
        &self,
        pad: &gst::Pad,
        _element: &super::OcvrCtrl,
        query: &mut gst::QueryRef,
    ) -> bool {
        gst_log!(CAT, obj: pad, "Handling query {:?}", query);
        self.srcpad.peer_query(query)
    }

    fn src_event(&self, pad: &gst::Pad, _element: &super::OcvrCtrl, event: gst::Event) -> bool {
        gst_log!(CAT, obj: pad, "Handling event {:?}", event);
        self.sinkpad.push_event(event)
    }

    fn src_query(
        &self,
        pad: &gst::Pad,
        _element: &super::OcvrCtrl,
        query: &mut gst::QueryRef,
    ) -> bool {
        gst_log!(CAT, obj: pad, "Handling query {:?}", query);
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
                glib::ParamSpecFlags::new(
                    "method",
                    "Check method",
                    "Method to check for duplicate frames",
                    Method::static_type(),
                    DEFAULT_METHOD.bits() as u32,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_PLAYING,
                ),
                glib::ParamSpecFloat::new(
                    "threshold",
                    "Threshold",
                    "Compare threshold for method 'fuzzy'",
                    0.0,
                    1.0,
                    DEFAULT_THRESHOLD as f32,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_PLAYING,
                ),
                glib::ParamSpecFlags::new(
                    "tolerance",
                    "Check tolerance",
                    "How to handle failed comparisons",
                    Method::static_type(),
                    DEFAULT_TOLERANCE.bits() as u32,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_PLAYING,
                ),
                glib::ParamSpecUInt64::new(
                    "repeat",
                    "Times to repeat check",
                    "Retry times for tolerance 'lazy' or 'strict'",
                    0,
                    100,
                    DEFAULT_REPEAT as u64,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_PLAYING,
                ),
                glib::ParamSpecUInt64::new(
                    "rows",
                    "Rows to consider",
                    "Number of pixel rows considered for 'fuzzy' compare",
                    0,
                    u64::MAX - 1,
                    DEFAULT_ROWS as u64,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_PLAYING,
                ),
                glib::ParamSpecBoolean::new(
                    "drop",
                    "Make 30Hz from 60Hz",
                    "Change 60Hz input rate to 30Hz always",
                    DEFAULT_DROP as bool,
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
            "rates" => {
                let rates = value.get().expect("type checked upstream");
                settings.rates = rates;
            }
            "probe-rates" => {
                let probe_rates = value.get().expect("type checked upstream");
                settings.probe_rates = probe_rates;
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
            "repeat" => {
                let repeat = value.get().expect("type checked upstream");
                settings.repeat = repeat;
            }
            "rows" => {
                let rows = value.get().expect("type checked upstream");
                settings.rows = rows;
            }
            "drop" => {
                let drop = value.get().expect("type checked upstream");
                settings.drop = drop;
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _obj: &Self::Type, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();
        match pspec.name() {
            "rates" => settings.rates.to_value(),
            "probe-rates" => settings.probe_rates.to_value(),
            "method" => settings.method.to_value(),
            "threshold" => settings.threshold.to_value(),
            "tolerance" => settings.tolerance.to_value(),
            "repeat" => settings.repeat.to_value(),
            "rows" => settings.rows.to_value(),
            "drop" => settings.drop.to_value(),
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

    fn change_state(
        &self,
        element: &Self::Type,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst_trace!(CAT, obj: element, "Changing state {:?}", transition);
        self.parent_change_state(element, transition)
    }
}
