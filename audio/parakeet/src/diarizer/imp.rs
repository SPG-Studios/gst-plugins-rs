// Copyright (C) 2026 Mathieu Duponchelle <mathieu@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

/**
 * SECTION:element-parakeetdiarizer
 *
 * Speaker diarization element using [parakeet-rs]
 *
 * The element is a very thin wrapper around parakeet's streaming API, for proper
 * alignment of the results it should be preceded by an `audiobuffersplit` element
 * grouping samples into 80 millisecond chunks.
 *
 * The element will set one custom meta on output audio buffers per overlapping
 * speaker segment, with a `speaker` field identifying the speaker.
 *
 * It is important to note that NVidia's sortformer model only supports up to four
 * speakers for now, further speakers will get misidentified.
 *
 * ```
 * gst-launch-1.0 -v filesrc location=/path/to/audio.wav ! wavparse ! audiobuffersplit output-buffer-size=5120 ! \
 * parakeetdiarizer model-path=/home/meh/devel/parakeet-rs/diar_streaming_sortformer_4spk-v2.1.onnx ! fakesink silent=false
 * ```
 *
 * [parakeet-rs]: https://github.com/altunenes/parakeet-rs/
 *
 * Since: plugins-rs-0.16.0
 */
use byte_slice_cast::*;
use gst::subclass::prelude::*;
use gst::{glib, prelude::*};

use parakeet_rs::sortformer::{DiarizationConfig, Sortformer, SpeakerSegment};
use std::collections::VecDeque;
use std::sync::{LazyLock, Mutex, mpsc};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "parakeetdiarizer",
        gst::DebugColorFlags::empty(),
        Some("Parakeet Diarization element"),
    )
});

#[derive(Debug, Clone)]
pub(super) struct Settings {
    model_path: Option<String>,
    latency_ms: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            model_path: None,
            latency_ms: 1_000,
        }
    }
}

#[derive(Default)]
struct State {
    // (live, min, max)
    upstream_latency: Option<(bool, gst::ClockTime, Option<gst::ClockTime>)>,
    sortformer: Option<Sortformer>,
    sortformer_latency: gst::ClockTime,
    thread_pool: Option<glib::ThreadPool>,
    inference_tx:
        Option<mpsc::Sender<(Sortformer, Result<Vec<SpeakerSegment>, parakeet_rs::Error>)>>,
    buffers: VecDeque<gst::Buffer>,
    segments: VecDeque<SpeakerSegment>,
    offset_start: u64,
    offset_end: u64,
}

// Locking order: state, settings
pub struct Diarizer {
    srcpad: gst::Pad,
    sinkpad: gst::Pad,
    state: Mutex<State>,
    settings: Mutex<Settings>,
}

impl Diarizer {
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

    fn sink_event(&self, pad: &gst::Pad, event: gst::Event) -> bool {
        gst::log!(CAT, obj = pad, "Handling event {event:?}");

        use gst::EventView::*;
        match event.view() {
            FlushStart(_) => {
                *self.state.lock().unwrap() = State::default();

                gst::Pad::event_default(pad, Some(&*self.obj()), event)
            }
            FlushStop(_) => gst::Pad::event_default(pad, Some(&*self.obj()), event),
            Caps(_) => self.srcpad.push_event(
                gst::event::Caps::builder(self.srcpad.pad_template().unwrap().caps())
                    .seqnum(event.seqnum())
                    .build(),
            ),
            Eos(_) | Segment(_) | Gap(_) | SegmentDone(_) => {
                if let Err(err) = self.infer(None) {
                    gst::warning!(
                        CAT,
                        imp = self,
                        "Inference failed on event {event:?}: {err:?}"
                    );
                }

                *self.state.lock().unwrap() = State::default();

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
                        let our_latency = self.state.lock().unwrap().sortformer_latency
                            + gst::ClockTime::from_mseconds(
                                self.settings.lock().unwrap().latency_ms as u64,
                            );
                        q.set(live, min + our_latency, max.opt_add(our_latency));
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

    fn infer(&self, buffer: Option<gst::Buffer>) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst::log!(CAT, "Handling {buffer:?}");

        let mut state = self.state.lock().unwrap();

        if buffer.is_none() && state.buffers.is_empty() {
            gst::debug!(CAT, imp = self, "Nothing to drain");
            return Ok(gst::FlowSuccess::Ok);
        }

        let (inference_tx, inference_rx) = mpsc::channel();
        state.inference_tx = Some(inference_tx);
        if let Some(ref buffer) = buffer {
            state.buffers.push_back(buffer.clone());
            state.offset_end += buffer.size() as u64;
        }

        if state.thread_pool.is_none() {
            let Ok(threadpool) = glib::ThreadPool::shared(None) else {
                gst::element_imp_error!(
                    self,
                    gst::StreamError::Failed,
                    ["Failed to create threadpool"]
                );
                return Err(gst::FlowError::Error);
            };

            state.thread_pool = Some(threadpool);
        }

        let thread_pool = state.thread_pool.take().unwrap();

        let mut sortformer = match state.sortformer.take() {
            Some(sortformer) => sortformer,
            None => {
                let Some(model_path) = self.settings.lock().unwrap().model_path.clone() else {
                    gst::element_imp_error!(
                        self,
                        gst::StreamError::Failed,
                        ["model-path property was not set"]
                    );
                    return Err(gst::FlowError::Error);
                };

                gst::info!(
                    CAT,
                    imp = self,
                    "Instantiating sortformer from path {model_path}"
                );

                match Sortformer::with_config(
                    model_path,
                    None,
                    // TODO: expose
                    DiarizationConfig::callhome(),
                ) {
                    Err(err) => {
                        gst::element_imp_error!(
                            self,
                            gst::StreamError::Failed,
                            ["failed to instantiate sortformer: {err:?}"]
                        );
                        return Err(gst::FlowError::Error);
                    }
                    Ok(sortformer) => {
                        gst::info!(
                            CAT,
                            imp = self,
                            "Successfully instantiated sortformer with {} samples latency",
                            (sortformer.chunk_len + sortformer.right_context) * 80 * 16
                        );

                        state.sortformer_latency = gst::ClockTime::from_mseconds(
                            ((sortformer.chunk_len + sortformer.right_context) * 80) as u64,
                        );

                        sortformer
                    }
                }
            }
        };

        if state.sortformer.is_none() {}

        let this_weak = self.downgrade();
        if let Err(err) = thread_pool.push(move || {
            let result = if let Some(buffer) = buffer {
                let Ok(data) = buffer.map_readable() else {
                    if let Some(this) = this_weak.upgrade() {
                        gst::element_imp_error!(
                            this,
                            gst::StreamError::Failed,
                            ["failed to map buffer readable"]
                        );
                    }
                    return;
                };

                sortformer.feed(data.as_slice_of().unwrap())
            } else {
                sortformer.flush()
            };

            if let Some(this) = this_weak.upgrade() {
                gst::debug!(CAT, imp = this, "Ran inference: {result:?}");
                if let Some(tx) = this.state.lock().unwrap().inference_tx.take() {
                    let _ = tx.send((sortformer, result));
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

        let (sortformer, result) = match inference_rx.recv() {
            Ok(res) => res,
            Err(_) => {
                return Err(gst::FlowError::Flushing);
            }
        };

        let segments = match result {
            Ok(segments) => segments,
            Err(err) => {
                gst::element_imp_error!(
                    self,
                    gst::StreamError::Failed,
                    ["Inference failed: {err}"]
                );
                return Err(gst::FlowError::Error);
            }
        };

        let mut output: Vec<gst::Buffer> = vec![];

        state = self.state.lock().unwrap();

        let edge_offset = state
            .offset_end
            .saturating_sub(((sortformer.chunk_len + sortformer.right_context) * 80 * 64) as u64);

        state.segments.extend(segments);

        while let Some(mut buffer) = state.buffers.pop_front() {
            // We do not perform any kind of complicated splitting logic, instead
            // users should place an audiobuffersplit element upstream of the
            // diarizer, with output-buffer-size = 5120 (80 milliseconds)
            if state.offset_start < edge_offset {
                state.offset_start += buffer.size() as u64;

                let buf_mut = buffer.make_mut();

                for segment in state.segments.iter() {
                    if segment.start * 4 <= state.offset_start && segment.end * 4 > edge_offset {
                        if let Ok(mut m) =
                            gst::meta::CustomMeta::add(buf_mut, "ParakeetSpeakerMeta")
                        {
                            m.mut_structure()
                                .set("speaker", segment.speaker_id.to_string());
                        }
                    }
                }

                gst::info!(CAT, "Will push buffer {buffer:?}");

                output.push(buffer);
            } else {
                state.buffers.push_front(buffer);
                break;
            }
        }

        // Now trim segments
        while let Some(segment) = state.segments.pop_front() {
            if segment.end * 4 <= edge_offset {
                continue;
            } else {
                state.segments.push_front(segment);
                break;
            }
        }

        state.sortformer = Some(sortformer);

        state.thread_pool = Some(thread_pool);

        drop(state);

        for buffer in output {
            gst::log!(CAT, imp = self, "Pushing buffer {buffer:?}");
            self.srcpad.push(buffer)?;
        }

        Ok(gst::FlowSuccess::Ok)
    }

    fn sink_chain(
        &self,
        _pad: &gst::Pad,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        if buffer.flags().contains(gst::BufferFlags::DISCONT) {
            self.infer(None)?;
            *self.state.lock().unwrap() = State::default();
        }

        self.infer(Some(buffer))
    }
}

#[glib::object_subclass]
impl ObjectSubclass for Diarizer {
    const NAME: &'static str = "GstParakeetDiarizer";
    type Type = super::Diarizer;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.pad_template("sink").unwrap();
        let sinkpad = gst::Pad::builder_from_template(&templ)
            .chain_function(|pad, parent, buffer| {
                Diarizer::catch_panic_pad_function(
                    parent,
                    || Err(gst::FlowError::Error),
                    |imp| imp.sink_chain(pad, buffer),
                )
            })
            .event_function(|pad, parent, event| {
                Diarizer::catch_panic_pad_function(
                    parent,
                    || false,
                    |imp| imp.sink_event(pad, event),
                )
            })
            .build();

        let templ = klass.pad_template("src").unwrap();
        let srcpad = gst::PadBuilder::<gst::Pad>::from_template(&templ)
            .query_function(|pad, parent, query| {
                Diarizer::catch_panic_pad_function(
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

impl ObjectImpl for Diarizer {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
            glib::ParamSpecUInt::builder("latency")
                .nick("Latency")
                .blurb("The expected processing latency. Will count towards total latency.")
                .default_value(Settings::default().latency_ms)
                .build(),
            glib::ParamSpecString::builder("model-path")
                .nick("Model Path")
                .blurb("Path to onnx export of diarizer model (https://github.com/altunenes/parakeet-rs/blob/master/scripts/export_diar_sortformer.py)")
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
            "model-path" => {
                self.settings.lock().unwrap().model_path = value.get().unwrap();
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "latency" => self.settings.lock().unwrap().latency_ms.to_value(),
            "model-path" => self.settings.lock().unwrap().model_path.to_value(),
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for Diarizer {}

impl ElementImpl for Diarizer {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Diarizer",
                "Audio/Filter",
                "Diarization filter, using Parakeet",
                "Mathieu Duponchelle <mathieu@centricular.com>",
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

            let caps = gst_audio::AudioCapsBuilder::new()
                .format(format)
                .rate(16_000)
                .channels(1)
                .layout(gst_audio::AudioLayout::Interleaved)
                .build();
            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
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
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst::info!(CAT, imp = self, "Changing state {transition:?}");

        match transition {
            gst::StateChange::PausedToReady => {
                gst::info!(CAT, "paused to ready");
                *self.state.lock().unwrap() = State::default();
                gst::info!(CAT, "done");
            }
            _ => (),
        }

        self.parent_change_state(transition)
    }
}
