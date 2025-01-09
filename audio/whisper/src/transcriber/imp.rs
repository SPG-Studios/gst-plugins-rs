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
use std::sync::{mpsc, Arc, LazyLock, Mutex};
use std::thread;
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
    buffer_tx: Option<mpsc::Sender<InputEvent>>,
    is_eos: bool,
    wp_processor: Option<thread::JoinHandle<()>>,
}

enum InputEvent {
    InputChunk { buffer: gst::Buffer },
    Eos,
}

pub struct TranscriberStream {
    imp: glib::subclass::ObjectImplRef<Transcriber>,
    adapter: gst_base::UniqueAdapter,
    wp_state: Option<WhisperState>,
    last_sample: bool,
    in_eos: bool,
    offset: Option<ClockTime>,
}

pub struct Transcriber {
    srcpad: gst::Pad,
    sinkpad: gst::Pad,
    settings: Arc<Mutex<Settings>>,
    state: Arc<Mutex<State>>,
}

impl Default for State {
    fn default() -> Self {
        State {
            buffer_tx: None,
            is_eos: false,
            wp_processor: None,
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

impl TranscriberStream {
    fn try_new(imp: &Transcriber) -> Result<Self, gst::ErrorMessage> {
        let wp_ctx = WhisperContext::new_with_params(
            &imp.settings.lock().unwrap().model_path,
            WhisperContextParameters::default(),
        );
        let wp_state = wp_ctx.as_ref().unwrap().create_state();

        Ok(TranscriberStream {
            imp: imp.ref_counted(),
            adapter: gst_base::UniqueAdapter::new(),
            wp_state: Some(wp_state.unwrap()),
            last_sample: false,
            in_eos: false,
            offset: None,
        })
    }

    fn try_decode(&mut self) {
        let min_ms = self.imp.settings.lock().unwrap().chunk_size;
        let data_in_ms: i32 = (self.adapter.available() / 64).try_into().unwrap();

        let min_data_reached: bool = data_in_ms >= min_ms.try_into().unwrap();
        let process = min_data_reached || (self.in_eos && self.last_sample);
        if !process {
            gst::debug!(
                CAT,
                imp = self.imp,
                "Data len not reached: {:?} and not in EOS.",
                self.adapter.available()
            );
            return;
        }

        gst::debug!(CAT, imp = self.imp, "Processing data");

        let adapter = self.adapter.buffer(self.adapter.available()).ok().unwrap();
        let data: gst::MappedBuffer<gst::buffer::Readable> = adapter
            .into_mapped_buffer_readable()
            .map_err(|_| gst::FlowError::Error)
            .unwrap();

        let num_f32 = data.len() / std::mem::size_of::<f32>();
        let f32_slice: &[f32] =
            unsafe { std::slice::from_raw_parts(data.as_ptr() as *const f32, num_f32) };

        let samples: Vec<f32> = f32_slice.to_vec();

        let start_time_metrics = std::time::Instant::now();

        if let Some(wp_state) = &mut self.wp_state {
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
            let offset = if let Some(offset) = self.offset {
                offset
            } else {
                gst::ClockTime::from_mseconds(0)
            };

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

                let start_time =
                    gst::ClockTime::from_mseconds((offset.mseconds() * 10).try_into().unwrap());
                let end_time = gst::ClockTime::from_mseconds(
                    ((offset.mseconds() + (end_timestamp as u64)) * 10)
                        .try_into()
                        .unwrap(),
                );
                let mut buffer = gst::Buffer::from_mut_slice(segment.into_bytes());
                gst::info!(
                    CAT,
                    imp = self.imp,
                    "GST TIME [{} -> {}]",
                    start_time,
                    end_time
                );

                {
                    let buf = buffer.get_mut().unwrap();
                    buf.set_pts(start_time);
                    buf.set_duration(end_time - start_time);
                }

                last_end = Some((end_time / 10).try_into().unwrap());

                let _ = self.imp.srcpad.push(buffer);
            }
            self.offset = last_end;
            gst::trace!(
                CAT,
                imp = self.imp,
                "Took {}ms. Number of bytes: caps {}, samples {}, data {}, segments {}",
                (end_time_metrics - start_time_metrics).as_millis(),
                self.adapter.available(),
                samples.len(),
                data.len(),
                num_segments
            );

            if num_segments > 0 {
                gst::info!(
                    CAT,
                    imp = self.imp,
                    "Number of segments produced {num_segments}"
                );
                self.adapter.clear();
            }
        }
    }
}

impl Transcriber {
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

        let Some(buffer_tx) = self.state.lock().unwrap().buffer_tx.take() else {
            gst::log!(CAT, obj = pad, "Flushing");
            return Err(gst::FlowError::Flushing);
        };

        buffer_tx
            .send(InputEvent::InputChunk { buffer: buffer })
            .unwrap();

        self.state.lock().unwrap().buffer_tx = Some(buffer_tx);

        Ok(gst::FlowSuccess::Ok)
    }

    fn sink_event(&self, pad: &gst::Pad, event: gst::Event) -> bool {
        gst::log!(CAT, obj = pad, "Handling sink event {:?}", event);

        use gst::EventView::*;
        match event.view() {
            Eos(_) => {
                gst::info!(CAT, imp = self, "Received EOS. Sending InputEvent::Eos");

                let mut state = self.state.lock().unwrap();
                state.is_eos = true;
                let _ = state.buffer_tx.as_mut().unwrap().send(InputEvent::Eos);
            }
            FlushStart(_) => {
                gst::info!(CAT, imp = self, "Received flush start");
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

        let mut transcriber = TranscriberStream::try_new(self)?;

        let (buffer_tx, buffer_rx) = mpsc::channel::<InputEvent>();

        let handle = thread::spawn(move || {
            gst::info!(CAT, imp = transcriber.imp, "Transcriber loop started");
            loop {
                let buffer = buffer_rx.recv().unwrap();

                transcriber.in_eos = transcriber.imp.state.lock().unwrap().is_eos;
                match buffer {
                    InputEvent::InputChunk { buffer } => {
                        gst::debug!(
                            CAT,
                            imp = transcriber.imp,
                            "Received buffer with pts {:?}",
                            buffer.pts().unwrap()
                        );
                        transcriber.adapter.push(buffer);
                        transcriber.try_decode();
                    }
                    InputEvent::Eos => {
                        gst::info!(
                            CAT,
                            imp = transcriber.imp,
                            "Received InputEvent::Eos. Last processing"
                        );
                        transcriber.last_sample = true;
                        transcriber.try_decode();
                        break;
                    }
                }
            }
            gst::info!(CAT, imp = transcriber.imp, "Finishing transcriber loop");

            gst::Pad::event_default(
                &transcriber.imp.sinkpad,
                Some(&*transcriber.imp.obj()),
                gst::event::Eos::new(),
            );
        });

        let mut state = self.state.lock().unwrap();

        state.buffer_tx = Some(buffer_tx);
        state.wp_processor = Some(handle);

        gst::debug!(CAT, imp = self, "Prepared");

        Ok(())
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
            settings: Arc::new(Mutex::new(Settings::default())),
            state: Arc::new(Mutex::new(State::default())),
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
                let mut state = self.state.lock().unwrap();
                let thread = state.wp_processor.take().unwrap();
                match thread.join() {
                    Ok(_) => {
                        gst::log!(CAT, imp = self, "Thread finished");
                    }
                    Err(_e) => {
                        gst::error!(CAT, imp = self, "Error finishing thread");
                    }
                }
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
