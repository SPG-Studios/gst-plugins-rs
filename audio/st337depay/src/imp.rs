// Copyright (C) 2026 Fluendo S.A.
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/> .
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;

use std::sync::{LazyLock, Mutex};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "st337depay",
        gst::DebugColorFlags::empty(),
        Some("ST337 depayloader"),
    )
});

/* Extract the depth-bit value left-justified in a 32-bit LE word */
fn word_extract(word: u32, depth: u32) -> u32 {
    (word >> (32 - depth)) & ((1u32 << depth) - 1)
}

fn st337_format_name(format: &St337DepayFormat) -> &'static str {
    match format {
        St337DepayFormat::St337FormatAc3 => "AC-3",
        St337DepayFormat::St337FormatEac3 => "E-AC-3",
        St337DepayFormat::St337FormatAc4 => "AC-4",
        St337DepayFormat::St337FormatDolbyE => "Dolby-E",
    }
}

const ST337_PA_16: u32 = 0xF872;
const ST337_PA_20: u32 = 0x6F872;
const ST337_PA_24: u32 = 0x96F872;
const ST337_PB_16: u32 = 0x4E1F;
const ST337_PB_20: u32 = 0x54E1F;
const ST337_PB_24: u32 = 0xA54E1F;

struct SyncWord {
    depth: u32,
    pa: u32,
    pb: u32,
}

const SYNC_WORDS: &[SyncWord] = &[
    SyncWord {
        depth: 16,
        pa: ST337_PA_16,
        pb: ST337_PB_16,
    },
    SyncWord {
        depth: 20,
        pa: ST337_PA_20,
        pb: ST337_PB_20,
    },
    SyncWord {
        depth: 24,
        pa: ST337_PA_24,
        pb: ST337_PB_24,
    },
];

enum St337DepayState {
    St337StateSyncing,
    St337StateHeader,
    St337StatePayload,
}

enum St337DepayFormat {
    St337FormatAc3,
    St337FormatEac3,
    St337FormatAc4,
    St337FormatDolbyE,
}

struct St337DepayPreamble {
    found: bool,
    pa: u32,
    pb: u32,
    pc: u32,
    pd: u32,
    depth: u8,
    is_subframe_mode: bool,
}
struct State {
    accumulator: Vec<u8>, // Equivalent to GstAdapter in C
    depay_state: St337DepayState,
    preamble: St337DepayPreamble,
    format: Option<St337DepayFormat>,
    payload_size: usize,
    num_channels: u32,
    fs: u32,
    wordsize: u32,
    is_interleaved: bool,
}

impl Default for St337DepayPreamble {
    fn default() -> Self {
        Self {
            found: false,
            pa: 0,
            pb: 0,
            pc: 0,
            pd: 0,
            depth: 0,
            is_subframe_mode: false,
        }
    }
}

impl Default for State {
    fn default() -> Self {
        Self {
            accumulator: Vec::new(),
            depay_state: St337DepayState::St337StateSyncing,
            preamble: St337DepayPreamble::default(),
            format: None,
            payload_size: 0,
            num_channels: 0,
            fs: 0,
            wordsize: 0,
            is_interleaved: false,
        }
    }
}

pub struct St337Depay {
    sinkpad: gst::Pad,
    srcpad: gst::Pad,
    state: Mutex<State>,
    pending_segment: Mutex<Option<gst::Event>>,
}

impl ObjectImpl for St337Depay {
    fn constructed(&self) {
        self.parent_constructed();
        let obj = self.obj();
        obj.add_pad(&self.sinkpad).unwrap();
        obj.add_pad(&self.srcpad).unwrap();
    }
}

impl GstObjectImpl for St337Depay {}

impl ElementImpl for St337Depay {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "STMPE ST-337 depayloader",
                "Codec/Depayloader/Audio",
                "Depays ST-337 streams",
                "Pablo García Sancho <pgarcia@fluendo.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let src_caps = gst::Caps::builder_full()
                .structure(gst::Structure::new_empty("audio/x-ac3"))
                .structure(gst::Structure::new_empty("audio/x-eac3"))
                .structure(gst::Structure::new_empty("audio/x-ac4"))
                .structure(gst::Structure::new_empty("audio/x-dolby-e"))
                .build();
            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &src_caps,
            )
            .unwrap();

            let sink_caps = gst::Caps::builder("audio/x-raw")
                .field("format", "S32LE")
                .field("layout", "interleaved")
                .build();
            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &sink_caps,
            )
            .unwrap();

            vec![src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

// Registers struct as a GstObject subclass
#[glib::object_subclass]
impl ObjectSubclass for St337Depay {
    const NAME: &'static str = "GstSt337Depay";
    type Type = super::St337Depay;
    type ParentType = gst::Element;

    // Equivalent to _init() in C
    fn with_class(class: &Self::Class) -> Self {
        let src_templ = class.pad_template("src").unwrap();
        let srcpad = gst::Pad::builder_from_template(&src_templ).build();
        let sink_templ = class.pad_template("sink").unwrap();
        let sinkpad = gst::Pad::builder_from_template(&sink_templ)
            .chain_function(|_pad, parent, buffer| {
                St337Depay::catch_panic_pad_function(
                    parent,
                    || Err(gst::FlowError::Error),
                    |this| this.chain(buffer),
                )
            })
            .event_function(|_pad, parent, event| {
                St337Depay::catch_panic_pad_function(
                    parent,
                    || false,
                    |this| this.sink_event(event),
                )
            })
            .build();

        Self {
            srcpad,
            sinkpad,
            state: Mutex::new(State::default()),
            pending_segment: Mutex::new(None),
        }
    }
}

impl St337Depay {
    fn chain(&self, buffer: gst::Buffer) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut state = self.state.lock().unwrap();

        {
            gst::debug!(CAT, "Chain: {} bytes", buffer.size());
            let map = buffer.map_readable().map_err(|_| {
                gst::error!(CAT, "Failed to map input buffer");
                gst::FlowError::Error
            })?;
            state.accumulator.extend_from_slice(map.as_slice());
        }

        if state.wordsize == 0 || state.num_channels == 0 {
            return Ok(gst::FlowSuccess::Ok);
        }

        /* Pa+Pb in subframe mode */
        let min_bytes = 4 * state.wordsize as usize;

        while state.accumulator.len() >= min_bytes {
            match state.depay_state {
                St337DepayState::St337StateSyncing => {
                    if !Self::parse_pa_pb(&mut state) {
                        return Err(gst::FlowError::Error);
                    }
                    if state.preamble.found {
                        state.depay_state = St337DepayState::St337StateHeader;
                    } else {
                        let ws = state.wordsize as usize;
                        state.accumulator.drain(..ws);
                    }
                }
                St337DepayState::St337StateHeader => {
                    /* Pa/Pb/Pc/Pd span 4 words (frame) or 8 words (subframe) */
                    let to_map = if state.preamble.is_subframe_mode { 8 } else { 4 }
                        * state.wordsize as usize;

                    if state.accumulator.len() < to_map {
                        return Ok(gst::FlowSuccess::Ok);
                    }

                    Self::parse_pc_pd(&mut state);

                    if !Self::parse_data_type(&mut state) {
                        state.depay_state = St337DepayState::St337StateSyncing;
                        state.accumulator.drain(..to_map);
                        continue;
                    }

                    if let Some(ref format) = state.format {
                        if let Err(e) = self.set_caps(format) {
                            gst::error!(CAT, "failed to set src caps: {e:?}");
                        }
                    }

                    state.accumulator.drain(..to_map);
                    state.depay_state = St337DepayState::St337StatePayload;
                }
                St337DepayState::St337StatePayload => {
                    if state.accumulator.len() < state.payload_size {
                        return Ok(gst::FlowSuccess::Ok);
                    }

                    self.push_payload(&mut state)?;
                }
            }
        }

        Ok(gst::FlowSuccess::Ok)
    }

    fn sink_event(&self, event: gst::Event) -> bool {
        gst::debug!(CAT, "Sink event: {event:?}");

        match event.type_() {
            gst::EventType::Caps => {
                let caps = if let gst::EventView::Caps(c) = event.view() {
                    c.caps_owned()
                } else {
                    gst::Caps::new_empty()
                };

                let mut state = self.state.lock().unwrap();

                if !Self::parse_caps(&mut state, &caps) {
                    gst::warning!(CAT, "failed to parse input caps");
                }

                if state.num_channels != 2 {
                    gst::warning!(
                        CAT,
                        "expected 2 channels (AES3), got {}",
                        state.num_channels
                    );
                }
                true
            }
            /* Store segment locally without auto-forwarding to srcpad.
             * It will be forwarded after caps are negotiated in push_payload(). */
            gst::EventType::Segment => {
                *self.pending_segment.lock().unwrap() = Some(event);
                true
            }
            _ => gst::Pad::event_default(&self.sinkpad, Some(&*self.obj()), event),
        }
    }

    fn set_caps(&self, format: &St337DepayFormat) -> Result<(), gst::FlowError> {
        let mime = Self::format_to_mime(format);

        let caps = gst::Caps::builder(mime).build();

        if self.srcpad.has_current_caps() {
            if let Some(current) = self.srcpad.current_caps() {
                if current == caps {
                    return Ok(());
                }
            }
        }

        if !self.srcpad.push_event(gst::event::Caps::new(&caps)) {
            gst::error!(CAT, "failed to set src caps");
            return Err(gst::FlowError::NotNegotiated);
        }

        Ok(())
    }

    fn push_payload(&self, state: &mut State) -> Result<gst::FlowSuccess, gst::FlowError> {
        let depth = state.preamble.depth as u32;
        let format = state.format.as_ref().unwrap();
        let is_dolby_e = matches!(format, St337DepayFormat::St337FormatDolbyE);

        /* Forward any pending segment event now that caps are negotiated.
         * This preserves correct sticky event ordering: Caps -> Segment. */
        if let Some(segment) = self.pending_segment.lock().unwrap().take() {
            self.srcpad.push_event(segment);
        }

        /* Dolby E ES: [Pa][Pb][Pc][Pd] + payload as 32-bit LE words (depth bits
         * left-justified). Other formats: bit-packed bytes, no preamble. */
        let out_buf = if is_dolby_e {
            let shift = 32 - depth;
            let out_size = (4 + state.preamble.pd / depth) as usize * 4;
            let mut out = Vec::with_capacity(out_size);
            out.extend_from_slice(&state.preamble.pa.to_le_bytes());
            out.extend_from_slice(&state.preamble.pb.to_le_bytes());
            out.extend_from_slice(&(state.preamble.pc << shift).to_le_bytes());
            out.extend_from_slice(&(state.preamble.pd << shift).to_le_bytes());
            out.extend_from_slice(&state.accumulator[..state.payload_size]);
            out
        } else {
            let out_size = ((state.preamble.pd + 7) / 8) as usize;
            let mut out = Vec::with_capacity(out_size);
            Self::extract_payload(
                &state.accumulator[..state.payload_size],
                depth,
                state.preamble.is_subframe_mode,
                state.num_channels,
                &mut out,
            );
            out
        };

        let push_size = out_buf.len();
        let format_name = st337_format_name(format);

        state.accumulator.drain(..state.payload_size);
        state.depay_state = St337DepayState::St337StateSyncing;

        let buffer = gst::Buffer::from_mut_slice(out_buf);
        gst::info!(CAT, "Pushing {} bytes ({})", push_size, format_name);

        self.srcpad.push(buffer).inspect_err(|err| {
            if *err != gst::FlowError::Eos {
                gst::error!(CAT, "failed to push output buffer: {err:?}");
            }
        })
    }

    /* Extract the depth-bit value left-justified in a 32-bit LE word */
    fn format_to_mime(format: &St337DepayFormat) -> &'static str {
        match format {
            St337DepayFormat::St337FormatAc3 => "audio/x-ac3",
            St337DepayFormat::St337FormatEac3 => "audio/x-eac3",
            St337DepayFormat::St337FormatAc4 => "audio/x-ac4",
            St337DepayFormat::St337FormatDolbyE => "audio/x-dolby-e",
        }
    }

    fn parse_caps(state: &mut State, caps: &gst::Caps) -> bool {
        let info = match gst_audio::AudioInfo::from_caps(caps) {
            Ok(info) => info,
            Err(_) => return false,
        };

        state.num_channels = info.channels();
        state.wordsize = info.bps() as u32;
        state.fs = info.rate();
        state.is_interleaved = info.layout() == gst_audio::AudioLayout::Interleaved;

        gst::info!(
            CAT,
            "Caps: {} ch, {} bytes/word, {} Hz, interleaved: {}",
            state.num_channels,
            state.wordsize,
            state.fs,
            if state.is_interleaved { "yes" } else { "no" }
        );

        true
    }

    fn parse_pa_pb(state: &mut State) -> bool {
        if !state.is_interleaved {
            gst::error!(
                CAT,
                "non-interleaved input is not supported in this depayloader"
            );
            return false;
            /* TODO: implement non-interleaved layout */
        }

        state.preamble.found = false;

        let ws = state.wordsize as usize;
        let word_pa = u32::from_le_bytes(state.accumulator[0..4].try_into().unwrap());
        let word_pb_frame =
            u32::from_le_bytes(state.accumulator[ws..ws + 4].try_into().unwrap());
        let has_third_word = state.accumulator.len() >= 3 * ws;
        let word_pb_subframe = if has_third_word {
            u32::from_le_bytes(state.accumulator[2 * ws..2 * ws + 4].try_into().unwrap())
        } else {
            0
        };

        for sw in SYNC_WORDS {
            if word_extract(word_pa, sw.depth) != sw.pa {
                continue;
            }

            if word_extract(word_pb_frame, sw.depth) == sw.pb {
                state.preamble.depth = sw.depth as u8;
                state.preamble.is_subframe_mode = false;
                state.preamble.found = true;
            } else if has_third_word
                && word_extract(word_pb_subframe, sw.depth) == sw.pb
            {
                state.preamble.depth = sw.depth as u8;
                state.preamble.is_subframe_mode = true;
                state.preamble.found = true;
            }
            break;
        }

        if state.preamble.found {
            state.preamble.pa = word_pa;
            state.preamble.pb = if state.preamble.is_subframe_mode {
                word_pb_subframe
            } else {
                word_pb_frame
            };
            gst::info!(
                CAT,
                "{}-bit preamble found in {} mode",
                state.preamble.depth,
                if state.preamble.is_subframe_mode {
                    "subframe"
                } else {
                    "frame"
                }
            );
        }

        true
    }

    fn parse_pc_pd(state: &mut State) {
        let depth = state.preamble.depth as u32;
        let shift = 32 - depth;
        let mask = (1u32 << depth) - 1;
        let ws = state.wordsize as usize;

        let (word_pc, word_pd) = if state.preamble.is_subframe_mode {
            let pc = u32::from_le_bytes(state.accumulator[4 * ws..4 * ws + 4].try_into().unwrap());
            let pd = u32::from_le_bytes(state.accumulator[6 * ws..6 * ws + 4].try_into().unwrap());
            (pc, pd)
        } else {
            let pc = u32::from_le_bytes(state.accumulator[2 * ws..2 * ws + 4].try_into().unwrap());
            let pd = u32::from_le_bytes(state.accumulator[3 * ws..3 * ws + 4].try_into().unwrap());
            (pc, pd)
        };

        state.preamble.pc = (word_pc >> shift) & mask;
        state.preamble.pd = (word_pd >> shift) & mask;

        let logical_words = (state.preamble.pd + depth - 1) / depth;

        if state.preamble.is_subframe_mode {
            state.payload_size =
                logical_words as usize * state.num_channels as usize * ws;
        } else {
            state.payload_size =
                ((logical_words as usize + 1) / 2) * state.num_channels as usize * ws;
        }

        gst::info!(
            CAT,
            "Pc=0x{:X} Pd=0x{:X} ({} bits, {} physical bytes)",
            state.preamble.pc,
            state.preamble.pd,
            state.preamble.pd,
            state.payload_size
        );
    }

    fn parse_data_type(state: &mut State) -> bool {
        let depth = state.preamble.depth as u32;

        /* data_type is a 5-bit field in Pc; its LSB is at bit (depth-16).
         * error_flag is the MSB of Pc. (SMPTE ST 337, Table 7) */
        let data_type = (state.preamble.pc >> (depth - 16)) & 0x1F;
        let error_flag = (state.preamble.pc >> (depth - 1)) & 0x1;

        state.format = match data_type {
            1 => Some(St337DepayFormat::St337FormatAc3),
            16 => Some(St337DepayFormat::St337FormatEac3),
            24 => Some(St337DepayFormat::St337FormatAc4),
            28 => Some(St337DepayFormat::St337FormatDolbyE),
            _ => {
                gst::warning!(CAT, "unknown data_type {}, ignoring burst", data_type);
                return false;
            }
        };

        gst::info!(
            CAT,
            "data_type={} ({}) error_flag={}",
            data_type,
            st337_format_name(state.format.as_ref().unwrap()),
            if error_flag != 0 { "true" } else { "false" }
        );

        if error_flag != 0 {
            gst::warning!(CAT, "error_flag set in burst");
        }

        true
    }

    fn extract_payload(
        data: &[u8],
        depth: u32,
        is_subframe: bool,
        num_channels: u32,
        out: &mut Vec<u8>,
    ) {
        let shift = 32 - depth;
        let mask = (1u32 << depth) - 1;
        /* In subframe mode only one channel carries payload; skip the other. */
        let word_stride = if is_subframe { 4 * num_channels as usize } else { 4 };
        let mut bit_accum: u64 = 0;
        let mut bits_in_accum: u32 = 0;
        let mut in_offset = 0;

        while in_offset + 4 <= data.len() {
            let word = u32::from_le_bytes(data[in_offset..in_offset + 4].try_into().unwrap());
            bit_accum = (bit_accum << depth) | ((word >> shift) & mask) as u64;
            bits_in_accum += depth;

            while bits_in_accum >= 8 {
                bits_in_accum -= 8;
                out.push((bit_accum >> bits_in_accum) as u8 & 0xFF);
            }

            in_offset += word_stride;
        }
    }
}
