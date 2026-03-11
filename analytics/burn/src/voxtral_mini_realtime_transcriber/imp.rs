// Copyright (C) 2026 Mathieu Duponchelle <mathieu@centricular.com>
// Copyright (C) 2026 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

/**
 * SECTION:element-voxtral-mini-realtime-transcriber
 *
 * Since: plugins-rs-0.16.0
 */
use byte_slice_cast::*;
use gst::subclass::prelude::*;
use gst::{glib, prelude::*};

use std::sync::{LazyLock, Mutex, MutexGuard, mpsc};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "voxtral-mini-realtime-transcriber",
        gst::DebugColorFlags::empty(),
        Some("Voxtral Mini Realtime Speech to Text element"),
    )
});

#[derive(Debug, Clone)]
pub(super) struct Settings {
    latency_ms: u32,
    chunk_duration_ms: u32,
    live_edge_offset_ms: u32,
    model_path: Option<String>,
    tokenizer_path: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            latency_ms: 1_000,
            chunk_duration_ms: 4_000,
            live_edge_offset_ms: 1_000,
            model_path: None,
            tokenizer_path: None,
        }
    }
}

struct TokenAccumulator {
    gst_dtw: gst::ClockTime,
    data: Option<String>,
}

impl TokenAccumulator {
    fn drain(&mut self, gst_dtw_end: gst::ClockTime) -> Option<gst::Buffer> {
        self.data.take().map(|old_data| {
            let mut outbuf = gst::Buffer::from_slice(old_data);
            {
                let buf_mut = outbuf.get_mut().unwrap();
                buf_mut.set_pts(self.gst_dtw);
                buf_mut.set_duration(gst_dtw_end.saturating_sub(self.gst_dtw))
            }
            outbuf
        })
    }

    fn push(&mut self, gst_dtw: gst::ClockTime, data: String) -> Option<gst::Buffer> {
        if data.starts_with(' ') {
            let ret = self.drain(gst_dtw);

            self.data = Some(data);
            self.gst_dtw = gst_dtw;
            ret
        } else {
            if let Some(mut old_data) = self.data.take() {
                old_data.push_str(&data);
                self.data = Some(old_data);
                self.gst_dtw = gst_dtw;
            } else {
                self.data = Some(data);
            }
            None
        }
    }
}

struct ModelState {}

#[derive(Default)]
struct State {
    // (live, min, max)
    upstream_latency: Option<(bool, gst::ClockTime, Option<gst::ClockTime>)>,
    model_state: Option<ModelState>,
    // The chunk we are currently accumulating for inference
    current_chunk: Vec<f32>,
    // The previous chunk used for inference
    previous_chunk: Vec<f32>,
    // The first PTS of the oldest chunk (previous or current)
    chunked_pts: Option<gst::ClockTime>,
    out_pts: Option<gst::ClockTime>,
    inference_tx: Option<mpsc::Sender<(ModelState, Result<(), anyhow::Error>)>>,
    model_tx: Option<mpsc::Sender<ModelState>>,
    model_rx: Option<mpsc::Receiver<ModelState>>,
    thread_pool: Option<glib::ThreadPool>,
}

pub struct Transcriber {
    srcpad: gst::Pad,
    sinkpad: gst::Pad,
    settings: Mutex<Settings>,
    state: Mutex<State>,
}

impl Transcriber {
    fn upstream_latency(&self) -> Option<(bool, gst::ClockTime, Option<gst::ClockTime>)> {
        if let Some(latency) = self.state.lock().unwrap().upstream_latency {
            return Some(latency);
        }

        let mut peer_query = gst::query::Latency::new();

        let ret = self.sinkpad.peer_query(&mut peer_query);

        if ret {
            let upstream_latency = peer_query.result();
            gst::info!(
                CAT,
                imp = self,
                "queried upstream latency: {upstream_latency:?}"
            );

            self.state.lock().unwrap().upstream_latency = Some(upstream_latency);

            Some(upstream_latency)
        } else {
            gst::trace!(CAT, imp = self, "could not query upstream latency");

            None
        }
    }

    fn push<'a>(
        &'a self,
        state: MutexGuard<'a, State>,
        mut output: Vec<gst::Buffer>,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let Some(mut out_pts) = state.out_pts else {
            if !output.is_empty() {
                panic!("Trying to push non-empty output but no out pts was set");
            }
            return Ok(gst::FlowSuccess::Ok);
        };

        let chunked_pts = state.chunked_pts;

        drop(state);

        let (chunk_duration_ms, live_edge_offset_ms) = {
            let settings = self.settings.lock().unwrap();

            (
                settings.chunk_duration_ms as u64,
                settings.live_edge_offset_ms as u64,
            )
        };

        if let Some(chunked_pts) = chunked_pts {
            let sliding_window_start_edge = chunked_pts
                + gst::ClockTime::from_mseconds(
                    chunk_duration_ms.saturating_sub(live_edge_offset_ms),
                );
            if out_pts < sliding_window_start_edge {
                let _ = self.srcpad.push_event(
                    gst::event::Gap::builder(out_pts)
                        .duration(sliding_window_start_edge - out_pts)
                        .build(),
                );
                out_pts = sliding_window_start_edge;
            }
        }

        for buffer in output.drain(..) {
            let buf_pts = buffer.pts().unwrap();
            let buf_end_pts = buf_pts + buffer.duration().unwrap();

            if buf_pts > out_pts {
                let _ = self.srcpad.push_event(
                    gst::event::Gap::builder(out_pts)
                        .duration(buf_pts - out_pts)
                        .build(),
                );
            }

            self.srcpad.push(buffer)?;

            out_pts = buf_end_pts;
        }

        self.state.lock().unwrap().out_pts = Some(out_pts);

        Ok(gst::FlowSuccess::Ok)
    }

    fn sink_event(&self, pad: &gst::Pad, event: gst::Event) -> bool {
        gst::log!(CAT, obj = pad, "Handling event {event:?}");

        use gst::EventView::*;
        match event.view() {
            FlushStart(_) => {
                {
                    let mut state = self.state.lock().unwrap();
                    let _ = state.inference_tx.take();
                    let _ = state.model_tx.take();
                }

                gst::Pad::event_default(pad, Some(&*self.obj()), event)
            }
            FlushStop(_) => {
                // Make sure pad.push returned
                let _ = self.sinkpad.stream_lock();

                {
                    let mut state = self.state.lock().unwrap();

                    *state = State::default();
                }
                gst::Pad::event_default(pad, Some(&*self.obj()), event)
            }
            Caps(_) => self.srcpad.push_event(
                gst::event::Caps::builder(self.srcpad.pad_template().unwrap().caps())
                    .seqnum(event.seqnum())
                    .build(),
            ),
            Eos(_) | Segment(_) | Gap(_) | SegmentDone(_) => {
                let mut output: Vec<gst::Buffer> = vec![];

                let mut state = self.state.lock().unwrap();
                state = match self.run_inference(state, true, &mut output) {
                    Ok(state) => state,
                    Err(_) => self.state.lock().unwrap(),
                };

                let _ = self.push(state, output);

                if let Gap(gap) = event.view() {
                    let (pts, duration) = gap.get();
                    self.state.lock().unwrap().out_pts =
                        Some(pts + duration.unwrap_or(gst::ClockTime::ZERO));
                }

                gst::Pad::event_default(pad, Some(&*self.obj()), event)
            }
            _ => gst::Pad::event_default(pad, Some(&*self.obj()), event),
        }
    }

    fn src_query(&self, pad: &gst::Pad, query: &mut gst::QueryRef) -> bool {
        gst::log!(CAT, obj = pad, "Handling query {:?}", query);

        match query.view_mut() {
            gst::QueryViewMut::Latency(ref mut q) => {
                if let Some((live, min, max)) = self.upstream_latency() {
                    if live {
                        let settings = self.settings.lock().unwrap();
                        let our_min_latency = gst::ClockTime::from_mseconds(
                            (settings.latency_ms
                                + settings.chunk_duration_ms
                                + settings.live_edge_offset_ms) as u64,
                        );
                        q.set(live, our_min_latency + min, max.opt_add(our_min_latency));
                    } else {
                        q.set(false, gst::ClockTime::ZERO, gst::ClockTime::NONE);
                    }
                    true
                } else {
                    false
                }
            }
            _ => gst::Pad::query_default(pad, Some(&*self.obj()), query),
        }
    }

    fn run_inference<'a>(
        &'a self,
        mut state: MutexGuard<'a, State>,
        drain: bool,
        output: &mut Vec<gst::Buffer>,
    ) -> Result<MutexGuard<'a, State>, gst::FlowError> {
        let Some(chunked_pts) = state.chunked_pts else {
            return Ok(state);
        };

        let mut model_state = match state.model_state.take() {
            None => {
                let Some(model_rx) = state.model_rx.take() else {
                    return Err(gst::FlowError::Flushing);
                };

                drop(state);

                let model_state = match model_rx.recv() {
                    Err(_) => {
                        return Err(gst::FlowError::Flushing);
                    }
                    Ok(model_state) => model_state,
                };

                state = self.state.lock().unwrap();

                model_state
            }
            Some(model_state) => model_state,
        };

        let is_live = state
            .upstream_latency
            .map(|(live, _, _)| live)
            .unwrap_or(false);

        gst::debug!(
            CAT,
            imp = self,
            "Running inference from chunk PTS {chunked_pts}, drain: {drain}"
        );

        let settings_clone = self.settings.lock().unwrap().clone();

        let (chunk_duration_ms, live_edge_offset_ms, our_latency) = {
            (
                settings_clone.chunk_duration_ms as u64,
                settings_clone.live_edge_offset_ms as u64,
                gst::ClockTime::from_mseconds(settings_clone.latency_ms as u64),
            )
        };

        let mut chunk_to_process = state.previous_chunk.clone();
        chunk_to_process.extend(&state.current_chunk);

        let processed_duration = gst::ClockTime::SECOND
            .mul_div_floor(chunk_to_process.len() as u64, 16_000)
            .unwrap();

        let clock = self.obj().clock();

        let now = clock.as_ref().map(|clock| clock.time());

        let (inference_tx, inference_rx) = mpsc::channel();

        state.inference_tx = Some(inference_tx);

        let this_weak = self.downgrade();

        if let Err(err) = state.thread_pool.as_ref().unwrap().push(move || {
            // TODO
            let result = todo!();

            if let Some(this) = this_weak.upgrade() {
                gst::debug!(CAT, imp = this, "Ran inference: {result:?}");
                if let Some(tx) = this.state.lock().unwrap().inference_tx.take() {
                    let _ = tx.send((model_state, result));
                }
            }
        }) {
            drop(state);
            gst::element_imp_error!(
                self,
                gst::StreamError::Failed,
                ["Failed to spawn inference thread: {err}"]
            );
            return Err(gst::FlowError::Error);
        }

        drop(state);

        let (model_state, result) = match inference_rx.recv() {
            Ok(res) => res,
            Err(_) => {
                return Err(gst::FlowError::Flushing);
            }
        };

        let mut state = self.state.lock().unwrap();

        state.model_state = Some(model_state);

        if let Err(err) = result {
            gst::element_imp_error!(self, gst::StreamError::Failed, ["inference failed: {err}"]);
            return Err(gst::FlowError::Error);
        }

        if let Some(now) = now {
            let actual_processing_time = clock.unwrap().time().saturating_sub(now);
            gst::log!(
                CAT,
                imp = self,
                "Actual processing time: {}",
                actual_processing_time
            );
            if is_live && actual_processing_time > our_latency {
                gst::warning!(
                    CAT,
                    imp = self,
                    "actual processing time {actual_processing_time} greater than configured latency {our_latency}"
                );
            }
        }

        let had_previous_chunk = !state.previous_chunk.is_empty();

        output.push(gst::Buffer::new());

        //let mut token_accumulator = TokenAccumulator {
        //    gst_dtw: gst::ClockTime::ZERO,
        //    data: None,
        //};
        //for segment in state.model_state.as_ref().unwrap().as_iter() {
        //    for idx in 0..segment.n_tokens() {
        //        let token = segment.get_token(idx).unwrap();
        //        let Ok(token_str) = token.to_str() else {
        //            continue;
        //        };

        //        let t_dtw_ms = token.token_data().t_dtw as u64 * 10;

        //        let is_continuation_token = !token_str.starts_with(' ');

        //        let out_of_bounds = if !drain {
        //            if had_previous_chunk {
        //                t_dtw_ms < chunk_duration_ms - live_edge_offset_ms
        //                    || t_dtw_ms >= chunk_duration_ms * 2 - live_edge_offset_ms
        //            } else {
        //                t_dtw_ms >= chunk_duration_ms - live_edge_offset_ms
        //            }
        //        } else if had_previous_chunk {
        //            t_dtw_ms < chunk_duration_ms - live_edge_offset_ms
        //        } else {
        //            false
        //        };

        //        let gst_dtw = gst::ClockTime::from_mseconds(t_dtw_ms) + chunked_pts;

        //        gst::log!(
        //            CAT,
        //            imp = self,
        //            "considering one token {token_str} with t_dtw_ms {t_dtw_ms}, out of bounds: {out_of_bounds}"
        //        );

        //        if !out_of_bounds || (token_accumulator.data.is_some() && is_continuation_token) {
        //            if token_accumulator.data.is_none() && is_continuation_token {
        //                continue;
        //            }

        //            if let Some(buffer) = token_accumulator.push(gst_dtw, token_str.to_string()) {
        //                output.push(buffer);
        //            }
        //        }

        //        if out_of_bounds
        //            && !is_continuation_token
        //            && let Some(buffer) = token_accumulator.drain(gst_dtw)
        //        {
        //            output.push(buffer);
        //        }
        //    }
        //}

        //if let Some(buffer) = token_accumulator.drain(chunked_pts + processed_duration) {
        //    output.push(buffer);
        //}

        if drain {
            state.current_chunk.clear();
            state.previous_chunk.clear();
            let _ = state.chunked_pts.take();
        }

        Ok(state)
    }

    fn sink_chain(
        &self,
        pad: &gst::Pad,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst::log!(CAT, obj = pad, "Handling {buffer:?}");

        if buffer.pts().is_none() {
            gst::element_imp_error!(self, gst::StreamError::Failed, ["need timestamped buffers"]);
            return Err(gst::FlowError::Error);
        }

        let Ok(data) = buffer.map_readable() else {
            gst::element_imp_error!(
                self,
                gst::StreamError::Failed,
                ["failed to map buffer readable"]
            );
            return Err(gst::FlowError::Error);
        };

        let upstream_latency = self.upstream_latency();

        let (chunk_duration_ms, our_latency_ms) = {
            let settings = self.settings.lock().unwrap();
            (settings.chunk_duration_ms, settings.latency_ms)
        };

        if let Some((live, _, _)) = upstream_latency
            && live
            && our_latency_ms > chunk_duration_ms
        {
            gst::element_imp_error!(
                self,
                gst::StreamError::Failed,
                ["Upstream is live and configured latency larger than chunk-duration"]
            );
            return Err(gst::FlowError::Error);
        }

        let mut state = self.state.lock().unwrap();

        if state.chunked_pts.is_none() {
            state.chunked_pts = Some(buffer.pts().unwrap());
            state.out_pts = state.chunked_pts;
        }

        let mut output: Vec<gst::Buffer> = vec![];

        let mut offset = 0;
        // 16 samples per millisecond, 4 bytes per sample
        let chunk_size = chunk_duration_ms as usize * 16 * 4;
        while offset < data.len() {
            let end_offset =
                offset + (data.len() - offset).min(chunk_size - (state.current_chunk.len() * 4));
            state
                .current_chunk
                .extend(data[offset..end_offset].as_slice_of::<f32>().unwrap());

            assert!(state.current_chunk.len() * 4 <= chunk_size);

            if state.current_chunk.len() * 4 == chunk_size {
                state = self.run_inference(state, false, &mut output)?;

                state.chunked_pts = Some(
                    state.chunked_pts.unwrap()
                        + gst::ClockTime::SECOND
                            .mul_div_floor(state.previous_chunk.len() as u64, 16_000)
                            .unwrap(),
                );

                state.previous_chunk = state.current_chunk.clone();
                state.current_chunk = vec![];
            }

            offset = end_offset;
        }

        self.push(state, output)
    }

    fn prepare(&self) -> Result<(), gst::ErrorMessage> {
        gst::debug!(CAT, imp = self, "Preparing");

        let (model_path, tokenizer_path) = {
            let settings = self.settings.lock().unwrap();

            let Some(model_path) = settings.model_path.clone() else {
                return Err(gst::error_msg!(
                    gst::CoreError::Failed,
                    ["model-path property must be set"]
                ));
            };

            let Some(tokenizer_path) = settings.tokenizer_path.clone() else {
                return Err(gst::error_msg!(
                    gst::CoreError::Failed,
                    ["tokenizer-path property must be set"]
                ));
            };

            if settings.live_edge_offset_ms >= settings.chunk_duration_ms {
                return Err(gst::error_msg!(
                    gst::CoreError::Failed,
                    ["chunk-duration must be greater than live-edge-offset"]
                ));
            }

            (model_path, tokenizer_path)
        };

        let settings = self.settings.lock().unwrap().clone();

        let (model_tx, model_rx) = mpsc::channel();

        let Ok(threadpool) = glib::ThreadPool::shared(None) else {
            return Err(gst::error_msg!(
                gst::CoreError::Failed,
                ["Failed to create whisper thread pool"]
            ));
        };

        self.post_start("loading", "loading model");

        let mut state = self.state.lock().unwrap();
        state.model_tx = Some(model_tx);
        state.model_rx = Some(model_rx);
        state.thread_pool = Some(threadpool);

        let this = self.ref_counted();
        if let Err(err) = state.thread_pool.as_ref().unwrap().push(move || {
            use super::voxtral_mini_realtime::audio::{
                ChunkConfig, MelConfig, MelSpectrogram, PadConfig,
            };
            use super::voxtral_mini_realtime::gguf::loader::Q4ModelLoader;
            use super::voxtral_mini_realtime::models::time_embedding::TimeEmbedding;
            use super::voxtral_mini_realtime::tokenizer::VoxtralTokenizer;
            use std::path::PathBuf;

            gst::debug!(CAT, imp = this, "Loading model");
            let device = cubecl::cpu::CpuDevice::new();

            let tokenizer = VoxtralTokenizer::from_file(&tokenizer_path).unwrap();

            let mel_extractor = MelSpectrogram::new(MelConfig::voxtral());
            let pad_config = PadConfig::voxtral();
            let time_embed = TimeEmbedding::new(3072);
            let t_embed = time_embed.embed::<burn::backend::Cpu>(1.0 as f32, &device);

            let path = PathBuf::from(model_path);
            if !path.exists() {
                unreachable!();
            }
            let mut loader = Q4ModelLoader::from_file(&path).unwrap();
            let model = loader.load::<burn::backend::Cpu>(&device).unwrap();

            let chunk_config =
                ChunkConfig::voxtral().with_max_frames(1200 /* max_mel_frames */);
            gst::debug!(CAT, imp = this, "Loaded model");

            //let ctx = match todo!() {
            //    Ok(ctx) => ctx,
            //    Err(err) => {
            //        if let Some(this) = this_weak.upgrade() {
            //            this.post_cancelled("loading", &format!("Failed to load model!: {}", err));
            //        }

            //        return;
            //    }
            //};

            let model_state = todo!();

            let mut state = this.state.lock().unwrap();

            let post_complete = if let Some(tx) = state.model_tx.take() {
                let _ = tx.send(model_state);
                true
            } else {
                false
            };

            drop(state);

            if post_complete {
                this.post_complete("loading", "loaded model");
            } else {
                this.post_cancelled("loading", "model loading interrupted");
            }
        }) {
            return Err(gst::error_msg!(
                gst::CoreError::Failed,
                ["Failed to spawn load_model thread: {err}"]
            ));
        }

        Ok(())
    }

    fn post_start(&self, code: &str, text: &str) {
        let obj = self.obj();
        let msg = gst::message::Progress::builder(gst::ProgressType::Start, code, text)
            .src(&*obj)
            .build();
        let _ = obj.post_message(msg);
    }

    fn post_complete(&self, code: &str, text: &str) {
        let obj = self.obj();
        let msg = gst::message::Progress::builder(gst::ProgressType::Complete, code, text)
            .src(&*obj)
            .build();
        let _ = obj.post_message(msg);
    }

    fn post_cancelled(&self, code: &str, text: &str) {
        let obj = self.obj();
        let msg = gst::message::Progress::builder(gst::ProgressType::Canceled, code, text)
            .src(&*obj)
            .build();
        let _ = obj.post_message(msg);
    }
}

#[glib::object_subclass]
impl ObjectSubclass for Transcriber {
    const NAME: &'static str = "GstVoxtralMiniRealtimeTranscriber";
    type Type = super::Transcriber;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.pad_template("sink").unwrap();
        let sinkpad = gst::Pad::builder_from_template(&templ)
            .chain_function(|pad, parent, buffer| {
                Transcriber::catch_panic_pad_function(
                    parent,
                    || Err(gst::FlowError::Error),
                    |imp| imp.sink_chain(pad, buffer),
                )
            })
            .event_function(|pad, parent, event| {
                Transcriber::catch_panic_pad_function(
                    parent,
                    || false,
                    |imp| imp.sink_event(pad, event),
                )
            })
            .build();

        let templ = klass.pad_template("src").unwrap();
        let srcpad = gst::PadBuilder::<gst::Pad>::from_template(&templ)
            .query_function(|pad, parent, query| {
                Transcriber::catch_panic_pad_function(
                    parent,
                    || false,
                    |imp| imp.src_query(pad, query),
                )
            })
            .flags(gst::PadFlags::FIXED_CAPS)
            .build();

        Self {
            srcpad,
            sinkpad,
            settings: Default::default(),
            state: Default::default(),
        }
    }
}

impl ObjectImpl for Transcriber {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
            glib::ParamSpecUInt::builder("latency")
                .nick("Latency")
                .blurb("The expected processing latency. Will count towards total latency.")
                .default_value(Settings::default().latency_ms)
                .build(),
            glib::ParamSpecUInt::builder("chunk-duration")
                .nick("Chunk Duration")
                .blurb("The duration of chunks to accumulate for inference, in milliseconds. \
                    Will count towards total latency.")
                .default_value(Settings::default().chunk_duration_ms)
                .build(),
            glib::ParamSpecUInt::builder("live-edge-offset")
                .nick("Live Edge Offset")
                .blurb("The element will feed in the previous chunk when running inference, \
                    and output tokens that are contained within a sliding window that may overlap both chunks. \
                    This controls the duration (in milliseconds) of the overlap, and will leave time for tokens \
                    near the end of the current chunk to stabilize. Will count towards total latency.")
                .default_value(Settings::default().live_edge_offset_ms)
                .build(),
            glib::ParamSpecString::builder("model-path")
                .nick("Model Path")
                .blurb("Path to ggml-formatted Voxtral Mini 4B Realtime model in Q4_0 quantization")
                .default_value(None)
                .build(),
            glib::ParamSpecString::builder("tokenizer-path")
                .nick("Tokenizer Path")
                .blurb("Path to JSON tokenizer (tekken.json)")
                .default_value(None)
                .build(),
        ]
        });

        PROPERTIES.as_ref()
    }

    fn constructed(&self) {
        self.parent_constructed();

        let obj = self.obj();
        obj.add_pad(&self.sinkpad).unwrap();
        obj.add_pad(&self.srcpad).unwrap();
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "latency" => {
                self.settings.lock().unwrap().latency_ms = value.get().unwrap();
            }
            "chunk-duration" => {
                self.settings.lock().unwrap().chunk_duration_ms = value.get().unwrap();
            }
            "live-edge-offset" => {
                self.settings.lock().unwrap().live_edge_offset_ms = value.get().unwrap();
            }
            "model-path" => {
                self.settings.lock().unwrap().model_path = value.get().unwrap();
            }
            "tokenizer-path" => {
                self.settings.lock().unwrap().tokenizer_path = value.get().unwrap();
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "latency" => self.settings.lock().unwrap().latency_ms.to_value(),
            "chunk-duration" => self.settings.lock().unwrap().chunk_duration_ms.to_value(),
            "live-edge-offset" => self.settings.lock().unwrap().live_edge_offset_ms.to_value(),
            "model-path" => self.settings.lock().unwrap().model_path.to_value(),
            "tokenizer-path" => self.settings.lock().unwrap().tokenizer_path.to_value(),
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for Transcriber {}

impl ElementImpl for Transcriber {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Transcriber",
                "Text/Audio/Filter",
                "Speech to Text filter, using Voxtral Mini Realtime",
                "Sebastian Dröge <sebastian@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            #[cfg(target_endian = "little")]
            let format = gst_audio::AudioFormat::F32le;
            #[cfg(target_endian = "big")]
            let format = gst_audio::AudioFormat::F32be;

            let sink_caps = gst_audio::AudioCapsBuilder::new()
                .format(format)
                .rate(16_000)
                .channels(1)
                .layout(gst_audio::AudioLayout::Interleaved)
                .build();
            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &sink_caps,
            )
            .unwrap();

            let src_caps = gst::Caps::builder_full()
                .structure(
                    gst::Structure::builder("text/x-raw")
                        .field("format", "utf8")
                        .build(),
                )
                .build();
            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &src_caps,
            )
            .unwrap();

            vec![src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }

    fn change_state(
        &self,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst::info!(CAT, imp = self, "Changing state {transition:?}");

        match transition {
            gst::StateChange::NullToReady => {
                self.prepare().map_err(|err| {
                    self.post_error_message(err);
                    gst::StateChangeError
                })?;
            }
            gst::StateChange::ReadyToPaused => {}
            gst::StateChange::PausedToReady => {
                let mut state = self.state.lock().unwrap();
                let _ = state.inference_tx.take();
                let _ = state.model_tx.take();
            }
            _ => (),
        }

        self.parent_change_state(transition)
    }
}
