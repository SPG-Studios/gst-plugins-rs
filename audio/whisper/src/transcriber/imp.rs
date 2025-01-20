// Copyright (C) 2025 <diego.nieto.m@outlook.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst::ClockTime;
use gst::DebugLevel;

use std::ffi::CStr;
use std::ffi::{c_char, c_void};
use std::sync::{LazyLock, Mutex};
use std::vec::Vec;

use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperState,
};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "whispertranscriber",
        gst::DebugColorFlags::empty(),
        Some("GstWhisperTranscriber"),
    )
});

const DEFAULT_AUDIO_CHUNK_SIZE_IN_MS: u32 = 4000;
const DEFAULT_TRANSCRIBE_LATENCY: gst::ClockTime = gst::ClockTime::from_seconds(5);

struct Settings {
    model_path: String,
    chunk_size: u32,
    transcribe_latency: gst::ClockTime,
}

struct State {
    is_eos: bool,
    adapter: gst_base::UniqueAdapter,
    wp_state: Option<WhisperState>,
    offset: Option<ClockTime>,
}

pub struct Transcriber {
    srcpad: gst::Pad,
    sinkpad: gst::Pad,
    settings: Mutex<Settings>,
    state: Mutex<State>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            is_eos: false,
            adapter: gst_base::UniqueAdapter::new(),
            wp_state: None,
            offset: None,
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            model_path: String::new(),
            chunk_size: DEFAULT_AUDIO_CHUNK_SIZE_IN_MS,
            transcribe_latency: DEFAULT_TRANSCRIBE_LATENCY,
        }
    }
}

extern "C" fn log_callback(level: u32, message: *const c_char, _user_data: *mut c_void) {
    let message = unsafe {
        assert!(!message.is_null());
        CStr::from_ptr(message).to_str().unwrap_or("Invalid UTF-8")
    };

    let gst_whisper_level = match CAT.threshold() {
        DebugLevel::None => 0,
        DebugLevel::Error => 1,
        DebugLevel::Warning => 2,
        DebugLevel::Info => 3,
        DebugLevel::Debug => 4,
        DebugLevel::Trace => 5,
        _ => 0,
    };

    if gst_whisper_level > level {
        gst::log!(CAT, "Level: {}, Message: {}", level, message);
    }
}

impl Transcriber {
    fn high_pass_filter(&self, data: &mut Vec<f32>, cutoff: f32, sample_rate: f32) {
        let rc = 1.0 / (2.0 * std::f32::consts::PI * cutoff);
        let dt = 1.0 / sample_rate;
        let alpha = dt / (rc + dt);

        let mut y = data[0];

        for i in 1..data.len() {
            y = alpha * (y + data[i] - data[i - 1]);
            data[i] = y;
        }
    }

    fn vad_simple(
        &self,
        pcmf32: &mut Vec<f32>,
        sample_rate: i32,
        last_ms: i32,
        vad_thold: f32,
        freq_thold: f32,
    ) -> bool {
        let n_samples = pcmf32.len() as i32;
        let n_samples_last = (sample_rate * last_ms) / 1000;

        if n_samples_last >= n_samples {
            // not enough samples - assume no speech
            gst::error!(CAT, "num samples {}. N samples {}. N samples last {}", pcmf32.len(), n_samples, n_samples_last);
            return false;
        }

        if freq_thold > 0.0 {
            self.high_pass_filter(pcmf32, freq_thold, sample_rate as f32);
        }

        let mut energy_all = 0.0f32;
        let mut energy_last = 0.0f32;

        for i in 0..n_samples as usize {
            energy_all += pcmf32[i].abs();

            if i >= (n_samples - n_samples_last) as usize {
                energy_last += pcmf32[i].abs();
            }
        }

        energy_all /= n_samples as f32;
        energy_last /= n_samples_last as f32;

        gst::debug!(CAT,
            "{}: energy_all: {}, energy_last: {}, vad_thold: {}, freq_thold: {}",
            "vad_simple", energy_all, energy_last, vad_thold, freq_thold
        );

        if energy_last > vad_thold * energy_all {
            return false;
        }

        true
    }

    fn create_state(&self) -> Result<(), gst::ErrorMessage> {
        let wp_ctx = WhisperContext::new_with_params(
            &self.settings.lock().unwrap().model_path,
            WhisperContextParameters::default(),
        );
        let wp_state = wp_ctx.as_ref().unwrap().create_state();

        self.state.lock().unwrap().wp_state = Some(wp_state.unwrap());

        Ok(())
    }

    fn try_decode(&self) {
        let mut state = self.state.lock().unwrap();
        let min_ms = self.settings.lock().unwrap().chunk_size;
        let data_in_ms: i32 = (state.adapter.available() / 64).try_into().unwrap();

        let min_data_reached: bool = data_in_ms >= min_ms.try_into().unwrap();
        let process = min_data_reached || state.is_eos;
        if !process {
            gst::debug!(
                CAT,
                imp = self,
                "Data len not reached: {:?} and not in EOS.",
                state.adapter.available()
            );
            return;
        }

        gst::debug!(CAT, imp = self, "Processing data");

        let adapter = state
            .adapter
            .buffer(state.adapter.available())
            .ok()
            .unwrap();
        let data: gst::MappedBuffer<gst::buffer::Readable> = adapter
            .into_mapped_buffer_readable()
            .map_err(|_| gst::FlowError::Error)
            .unwrap();

        let num_f32 = data.len() / std::mem::size_of::<f32>();
        let f32_slice: &[f32] =
            unsafe { std::slice::from_raw_parts(data.as_ptr() as *const f32, num_f32) };

        let samples: Vec<f32> = f32_slice.to_vec();

        let mut vad_chunk = samples.iter().take(32000).cloned().collect();

        let voice_activity_detected = self.vad_simple(&mut vad_chunk, 16000, 1000, 0.6, 100.0);
        gst::debug!(CAT, "Voice activity detected {}. Total samples {}", voice_activity_detected, samples.len());

        let start_time_metrics = std::time::Instant::now();

        let offset = if let Some(offset) = state.offset {
            offset
        } else {
            gst::ClockTime::from_mseconds(0)
        };
        if let Some(wp_state) = &mut state.wp_state {
            let mut wp_params = FullParams::new(SamplingStrategy::default());
            wp_params.set_print_progress(false);
            wp_params.set_print_special(false);
            wp_params.set_print_progress(false);
            wp_params.set_print_realtime(false);
            wp_params.set_print_timestamps(false);

            wp_state
                .full(wp_params, &samples)
                .expect("failed to convert samples");
            let end_time_metrics = std::time::Instant::now();

            let num_segments = wp_state
                .full_n_segments()
                .expect("failed to get number of segments");

            let mut last_end = gst::ClockTime::NONE;

            for i in 0..num_segments {
                let segment = wp_state
                    .full_get_segment_text(i)
                    .expect("failed to get segment");
                let _start_timestamp = wp_state
                    .full_get_segment_t0(i)
                    .expect("failed to get start timestamp");
                let end_timestamp = wp_state
                    .full_get_segment_t1(i)
                    .expect("failed to get end timestamp");

                let n_tokens = wp_state.full_n_tokens(i).unwrap();
                for j in 0..n_tokens {
                    let prob = wp_state.full_get_token_prob(i, j);
                    println!("prob {}={:?}", j, prob);
                }

                let start_time =
                    gst::ClockTime::from_mseconds((offset.mseconds() * 10).try_into().unwrap());
                let end_time = gst::ClockTime::from_mseconds(
                    ((offset.mseconds() + (end_timestamp as u64)) * 10)
                        .try_into()
                        .unwrap(),
                );
                let mut buffer = gst::Buffer::from_mut_slice(segment.into_bytes());
                gst::info!(CAT, imp = self, "GST TIME [{} -> {}]", start_time, end_time);

                {
                    let buf = buffer.get_mut().unwrap();
                    buf.set_pts(start_time);
                    buf.set_duration(end_time - start_time);
                }

                last_end = Some((end_time / 10).try_into().unwrap());

                let _ = self.srcpad.push(buffer);
            }
            state.offset = last_end;
            gst::trace!(
                CAT,
                imp = self,
                "Took {}ms. Number of bytes: caps {}, samples {}, data {}, segments {}",
                (end_time_metrics - start_time_metrics).as_millis(),
                state.adapter.available(),
                samples.len(),
                data.len(),
                num_segments
            );

            if num_segments > 0 {
                gst::info!(
                    CAT,
                    imp = self,
                    "Number of segments produced {num_segments}"
                );
                state.adapter.clear();
            }
        }
    }

    fn sink_chain(
        &self,
        pad: &gst::Pad,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst::log!(CAT, obj = pad, "Handling buffer {:?}", buffer);

        if buffer.pts().is_none() {
            gst::element_imp_error!(
                self,
                gst::StreamError::Format,
                ["Stream with timestamped buffers required"]
            );

            return Err(gst::FlowError::Error);
        }

        self.state.lock().unwrap().adapter.push(buffer);
        self.try_decode();

        Ok(gst::FlowSuccess::Ok)
    }

    fn sink_event(&self, pad: &gst::Pad, event: gst::Event) -> bool {
        gst::log!(CAT, obj = pad, "Handling sink event {:?}", event);

        use gst::EventView::*;
        match event.view() {
            Eos(_) => {
                gst::info!(CAT, imp = self, "Received EOS. Sending InputEvent::Eos");
                self.state.lock().unwrap().is_eos = true;
                self.try_decode();
                gst::Pad::event_default(pad, Some(&*self.obj()), event);
            }
            FlushStart(_) => {
                gst::info!(CAT, imp = self, "Received flush start");
                self.state.lock().unwrap().adapter.clear();
            }
            FlushStop(_) => {
                gst::info!(CAT, imp = self, "Received flush stop");
            }
            Segment(e) => {
                gst::info!(CAT, imp = self, "Received segment {e:?}",);
                gst::Pad::event_default(pad, Some(&*self.obj()), event);
            }
            Tag(t) => {
                gst::info!(CAT, imp = self, "Received tag {t:?}",);
            }
            Caps(c) => {
                gst::info!(CAT, "Received caps {c:?}");
                gst::Pad::event_default(pad, Some(&*self.obj()), event);
            }
            StreamStart(_) => {
                gst::info!(CAT, "Received stream start");
                gst::Pad::event_default(pad, Some(&*self.obj()), event);
            }
            _ => {
                gst::info!(CAT, "Other event");
            }
        }
        true
    }

    fn sink_query(&self, pad: &gst::Pad, query: &mut gst::QueryRef) -> bool {
        gst::log!(CAT, obj = pad, "Handling sink query {:?}", query);
        gst::Pad::query_default(pad, Some(pad), query)
    }

    fn src_event(&self, pad: &gst::Pad, event: gst::Event) -> bool {
        gst::log!(CAT, obj = pad, "Handling src event {:?}", event);
        self.sinkpad.push_event(event)
    }

    fn src_query(&self, pad: &gst::Pad, query: &mut gst::QueryRef) -> bool {
        gst::log!(CAT, obj = pad, "Handling src query {:?}", query);

        use gst::QueryViewMut::*;
        match query.view_mut() {
            Latency(q) => {
                let mut peer_query = gst::query::Latency::new();

                let ret = self.sinkpad.peer_query(&mut peer_query);

                if ret {
                    let (_, min, _) = peer_query.result();

                    let our_latency = self.settings.lock().unwrap().transcribe_latency;

                    gst::info!(CAT, obj = pad, "Our latency {our_latency}");
                    q.set(true, our_latency + min, gst::ClockTime::NONE);
                }
                ret
            }
            _ => gst::Pad::query_default(pad, Some(pad), query),
        }
    }

    fn prepare(&self) -> Result<(), gst::ErrorMessage> {
        gst::debug!(CAT, imp = self, "Preparing");

        unsafe {
            whisper_rs::set_log_callback(Some(log_callback), std::ptr::null_mut());
        }

        self.create_state()
    }
}

#[glib::object_subclass]
impl ObjectSubclass for Transcriber {
    const NAME: &'static str = "GstWhisperTranscriber";
    type Type = super::Transcriber;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.pad_template("sink").unwrap();
        let sinkpad = gst::Pad::builder_from_template(&templ)
            .chain_function(|pad, parent, buffer| {
                Transcriber::catch_panic_pad_function(
                    parent,
                    || Err(gst::FlowError::Error),
                    |transcriber| transcriber.sink_chain(pad, buffer),
                )
            })
            .event_function(|pad, parent, event| {
                Transcriber::catch_panic_pad_function(
                    parent,
                    || false,
                    |transcriber| transcriber.sink_event(pad, event),
                )
            })
            .query_function(|pad, parent, query| {
                Transcriber::catch_panic_pad_function(
                    parent,
                    || false,
                    |transcriber| transcriber.sink_query(pad, query),
                )
            })
            .build();

        let templ = klass.pad_template("src").unwrap();
        let srcpad = gst::Pad::builder_from_template(&templ)
            .event_function(|pad, parent, event| {
                Transcriber::catch_panic_pad_function(
                    parent,
                    || false,
                    |identity| identity.src_event(pad, event),
                )
            })
            .query_function(|pad, parent, query| {
                Transcriber::catch_panic_pad_function(
                    parent,
                    || false,
                    |identity| identity.src_query(pad, query),
                )
            })
            .build();

        Self {
            srcpad,
            sinkpad,
            settings: Mutex::new(Settings::default()),
            state: Mutex::new(State::default()),
        }
    }
}

// Implementation of glib::Object virtual methods
impl ObjectImpl for Transcriber {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecString::builder("model-path")
                    .nick("The model path")
                    .blurb("The path to the model to use")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt::builder("chunk-size")
                    .nick("Chunk-size")
                    .blurb("The size in ms of each chunk to process")
                    .minimum(500)
                    .maximum(10000)
                    .default_value(DEFAULT_AUDIO_CHUNK_SIZE_IN_MS)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt::builder("transcribe-latency")
                    .nick("Whisper Transcribe Latency")
                    .blurb("Amount of milliseconds to allow Whisper transcribe")
                    .default_value(DEFAULT_TRANSCRIBE_LATENCY.mseconds() as u32)
                    .mutable_ready()
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let mut settings = self.settings.lock().unwrap();
        match pspec.name() {
            "model-path" => {
                settings.model_path = value.get().expect("type checked upstream");
            }
            "chunk-size" => {
                settings.chunk_size = value.get().expect("type checked upstream");
            }
            "transcribe-latency" => {
                settings.transcribe_latency = value.get().expect("type checked upstream");
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();
        match pspec.name() {
            "model-path" => settings.model_path.to_value(),
            "chunk-size" => settings.chunk_size.to_value(),
            "transcribe-latency" => settings.transcribe_latency.to_value(),
            _ => unimplemented!(),
        }
    }

    fn constructed(&self) {
        self.parent_constructed();

        let obj = self.obj();
        obj.add_pad(&self.sinkpad).unwrap();
        obj.add_pad(&self.srcpad).unwrap();
    }
}

impl GstObjectImpl for Transcriber {}

// Implementation of gst::Element virtual methods
impl ElementImpl for Transcriber {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Transcriber",
                "Audio/Text/Filter",
                "Speech to Text filter, using Whisper transcriber",
                "Diego Nieto <diego.nieto.m@outlook.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn change_state(
        &self,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst::info!(CAT, imp = self, "Changing state {transition:?}");

        if let gst::StateChange::NullToReady = transition {
            self.prepare().map_err(|err| {
                self.post_error_message(err);
                gst::StateChangeError
            })?;
        }

        let mut success = self.parent_change_state(transition)?;

        match transition {
            gst::StateChange::PausedToReady => {
                success = gst::StateChangeSuccess::NoPreroll;
            }
            gst::StateChange::ReadyToPaused => {
                success = gst::StateChangeSuccess::NoPreroll;
            }
            gst::StateChange::PlayingToPaused => {
                success = gst::StateChangeSuccess::NoPreroll;
            }
            _ => (),
        }

        Ok(success)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let sink_caps = gst_audio::AudioCapsBuilder::new()
                .format(gst_audio::AudioFormat::F32le)
                .rate(16000)
                .channels(1)
                .build();
            let src_caps = gst::Caps::builder("text/x-raw")
                .field("format", "utf8")
                .build();
            let audio_src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &src_caps,
            )
            .unwrap();
            let audio_sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &sink_caps,
            )
            .unwrap();

            vec![audio_src_pad_template, audio_sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}
