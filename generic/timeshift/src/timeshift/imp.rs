// Copyright (C) 2026 Lluc Simó Margalef <lsimmar@upv.es>, Immersive
//    Interactive Media (IIM) R&D group at Universitat Politècnica de València.
//
// This plugin has been developed with support by the following projects:
// CIAICO/2022/025, from Conselleria de Innovación, Universidades, Ciencia y
// Sociedad Digital of the GVA (DOGV 8919/05.10.2020); and grant
// PID2021-126645OB-I00, funded by MICIU/AEI/10.13039/501100011033/ and by "ERDF
// A way of making Europe".
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

/**
 * SECTION:element-timeshift
 *
 * Timeshift
 *
 * Element that provides timeshifting capabilities by storing incoming data in
 * a ring buffer. It enables seeking when used in a pipeline with non-seekable
 * live sources.
 *
 * # Example launch line
 * gst-launch-1.0 udpsrc ! decodebin ! timeshift ! autovideosink
 *
 *
 */
use gst::glib;
use gst::glib::prelude::*;
use gst::prelude::*;
use gst::subclass::prelude::*;
use ringbuf::{
    HeapRb, SharedRb,
    storage::Storage,
    traits::{Consumer, Observer, RingBuffer},
};
use std::sync::{Condvar, LazyLock, Mutex};

const DEFAULT_BUFFER_SIZE: u32 = 512;

struct Settings {
    /// Size of the ring buffer
    buffer_size: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            buffer_size: DEFAULT_BUFFER_SIZE,
        }
    }
}

/// Wrapper for queries stored in the ring buffer that allows tracking results
struct PendingQuery {
    query: gst::Query,
    result: Option<bool>,
}

struct State {
    /// Ring buffer storing buffers and serialized events and queries
    buffer: Option<Box<HeapRb<gst::MiniObject>>>,
    /// Total number of items pushed to the ring buffer (can be larger than buffer size)
    total_written: u64,
    /// Current playback position in terms of total items written
    playback_pos: u64,

    flushing: bool,

    sent_stream_start: bool,

    sent_caps: bool,

    sent_segment: bool,
    /// Current segment
    segment: gst::Segment,
    /// Pending seek segment to be pushed
    pending_segment: Option<gst::Segment>,
    /// Whether a discontinuity is pending to be applied to the next buffer
    discont_pending: bool,
    /// Currently pending serialized query being processed (if any)
    pending_query: Option<PendingQuery>,
}

impl Default for State {
    fn default() -> Self {
        let mut segment = gst::Segment::new();
        segment.set_format(gst::Format::Time);

        State {
            buffer: None,
            total_written: 0,
            playback_pos: 0,
            flushing: false,
            sent_stream_start: false,
            sent_caps: false,
            sent_segment: false,
            segment,
            pending_segment: None,
            discont_pending: false,
            pending_query: None,
        }
    }
}

/// Result of processing an item from the ring buffer
enum ItemResult {
    Ok,
    Continue,
    Error,
}

pub struct Timeshift {
    sinkpad: gst::Pad,
    srcpad: gst::Pad,
    state: Mutex<State>,
    cond: Condvar,
    settings: Mutex<Settings>,
}

impl Timeshift {
    fn src_activatemode(
        &self,
        pad: &gst::Pad,
        mode: gst::PadMode,
        active: bool,
    ) -> Result<(), gst::LoggableError> {
        if active {
            match mode {
                gst::PadMode::Push => {
                    let weak_element = self.obj().downgrade();
                    let pad_weak = pad.downgrade();
                    pad.start_task(move || {
                        if let (Some(element), Some(pad)) =
                            (weak_element.upgrade(), pad_weak.upgrade())
                        {
                            let imp = element.imp();
                            imp.loop_fn(&pad);
                        }
                    })
                    .map_err(|e| gst::loggable_error!(CAT, "Failed to start task: {:?}", e))?;
                }
                _ => {
                    return Err(gst::loggable_error!(
                        CAT,
                        "Only Push mode supported for now"
                    ));
                }
            }
        } else {
            let mut state = self.state.lock().unwrap();
            state.flushing = true;
            self.cond.notify_all();
            drop(state);
            let _ = pad.stop_task();
        }
        Ok(())
    }

    fn loop_fn(&self, pad: &gst::Pad) {
        let mut state = self.state.lock().unwrap();

        if !state.sent_stream_start {
            state.sent_stream_start = true;
            drop(state);

            let stream_id = pad.create_stream_id(&*self.obj(), Some("src"));
            pad.push_event(gst::event::StreamStart::new(&stream_id));

            state = self.state.lock().unwrap();
        }

        loop {
            if state.flushing {
                break;
            }

            if state.playback_pos >= state.total_written {
                state = self.cond.wait(state).unwrap();
                continue;
            }

            if let Some(segment) = state.pending_segment.take() {
                drop(state);
                pad.push_event(gst::event::FlushStop::new(false));
                pad.push_event(gst::event::Segment::new(&segment));
                state = self.state.lock().unwrap();
            }

            let Some((item, is_discont)) = self.fetch_next_item(&mut state) else {
                break;
            };
            drop(state);

            match self.process_item(pad, item, is_discont) {
                ItemResult::Continue => {
                    state = self.state.lock().unwrap();
                    continue;
                }
                ItemResult::Error => return,
                ItemResult::Ok => {}
            }

            state = self.state.lock().unwrap();
        }
    }

    fn fetch_next_item(&self, state: &mut State) -> Option<(gst::MiniObject, bool)> {
        let rb = state.buffer.as_ref()?;
        let count = rb.occupied_len() as u64;
        let total_written = state.total_written;
        let start_valid = total_written.saturating_sub(count);

        if state.playback_pos < start_valid {
            gst::warning!(
                CAT,
                imp = self,
                "Discontinuity: playback_pos {} < start_valid {}",
                state.playback_pos,
                start_valid
            );
            state.playback_pos = start_valid;
        }

        let offset = state.playback_pos - start_valid;
        if offset > usize::MAX as u64 {
            gst::error!(CAT, imp = self, "Offset {} exceeds usize::MAX", offset);
            return None;
        }

        let item = rb.get(offset as usize).cloned();

        if item.is_some() {
            state.playback_pos += 1;
        } else {
            gst::warning!(CAT, imp = self, "Failed to get item at offset {}", offset);
            state.playback_pos = total_written;
        }

        let is_discont = state.discont_pending;
        if is_discont {
            state.discont_pending = false;
        }

        item.map(|i| (i, is_discont))
    }

    fn process_item(&self, pad: &gst::Pad, item: gst::MiniObject, is_discont: bool) -> ItemResult {
        if let Ok(buffer) = item.clone().downcast::<gst::Buffer>() {
            return self.process_buffer(pad, buffer, is_discont);
        }

        if let Ok(event) = item.clone().downcast::<gst::Event>() {
            return self.process_event(pad, event);
        }

        if item.downcast::<gst::Query>().is_ok() {
            return self.process_query(pad);
        }

        ItemResult::Ok
    }

    fn process_buffer(
        &self,
        pad: &gst::Pad,
        mut buffer: gst::Buffer,
        is_discont: bool,
    ) -> ItemResult {
        if is_discont {
            let buffer_ref = buffer.make_mut();
            buffer_ref.set_flags(gst::BufferFlags::DISCONT);
        }

        if let Err(err) = pad.push(buffer) {
            gst::error!(CAT, imp = self, "Failed to push buffer: {:?}", err);
            return ItemResult::Error;
        }

        ItemResult::Ok
    }

    fn process_event(&self, pad: &gst::Pad, event: gst::Event) -> ItemResult {
        let is_caps = event.type_() == gst::EventType::Caps;
        pad.push_event(event);

        if is_caps {
            let mut state = self.state.lock().unwrap();
            if !state.sent_caps {
                state.sent_caps = true;
                if !state.sent_segment {
                    state.sent_segment = true;
                    drop(state);

                    let mut segment = gst::Segment::new();
                    segment.set_format(gst::Format::Time);
                    pad.push_event(gst::event::Segment::new(&segment));
                }
            }
            return ItemResult::Continue;
        }

        ItemResult::Ok
    }

    fn process_query(&self, pad: &gst::Pad) -> ItemResult {
        let mut state = self.state.lock().unwrap();
        let pending_query = state.pending_query.take();
        drop(state);

        if let Some(mut pending) = pending_query {
            let query = pending.query.make_mut();
            let result = pad.peer_query(query);
            pending.result = Some(result);
            gst::debug!(
                CAT,
                imp = self,
                "Processed serialized query, result: {}",
                result
            );

            let mut state = self.state.lock().unwrap();
            state.pending_query = Some(pending);
        }

        self.cond.notify_all();
        ItemResult::Continue
    }

    fn src_query(&self, pad: &gst::Pad, query: &mut gst::QueryRef) -> bool {
        use gst::QueryViewMut;

        match query.view_mut() {
            QueryViewMut::Seeking(q) => {
                if q.format() != gst::Format::Time {
                    return false;
                }

                let state = self.state.lock().unwrap();
                if let Some(rb) = state.buffer.as_ref() {
                    let count = rb.occupied_len();
                    if count > 0 {
                        let tail = (0..count).find_map(|i| {
                            rb.get(i)
                                .and_then(|m| m.clone().downcast::<gst::Buffer>().ok())
                        });

                        let head = (0..count).rev().find_map(|i| {
                            rb.get(i)
                                .and_then(|m| m.clone().downcast::<gst::Buffer>().ok())
                        });

                        if let (Some(tail_buf), Some(head_buf)) = (tail, head)
                            && let (Some(start), Some(end)) = (tail_buf.pts(), head_buf.pts())
                        {
                            q.set(true, start, end);
                            return true;
                        }
                    }
                }
                q.set(false, gst::ClockTime::NONE, gst::ClockTime::NONE);
                true
            }
            _ => gst::Pad::query_default(pad, Some(&*self.obj()), query),
        }
    }

    fn perform_seek(&self, start: gst::ClockTime) -> bool {
        let mut state = self.state.lock().unwrap();
        let Some(rb) = state.buffer.as_ref() else {
            return false;
        };

        let count = rb.occupied_len();
        if count == 0 {
            return false;
        }

        let total_written = state.total_written;
        let start_valid = total_written.saturating_sub(count as u64);

        let mut found_idx = None;

        // Currently we fail to seek if no keyframe is found before start
        for i in (0..count).rev() {
            if let Some(item) = rb.get(i)
                && let Ok(buffer) = item.clone().downcast::<gst::Buffer>()
                && let Some(pts) = buffer.pts()
                && pts <= start
                && !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT)
            {
                found_idx = Some(i);
                break;
            }
        }

        if let Some(idx) = found_idx {
            state.playback_pos = start_valid + idx as u64;
            gst::debug!(
                CAT,
                imp = self,
                "Seeked to index {}, playback_pos {}",
                idx,
                state.playback_pos
            );
            return true;
        }

        false
    }

    fn src_event(&self, pad: &gst::Pad, event: gst::Event) -> bool {
        use gst::EventView;
        match event.view() {
            EventView::Seek(e) => {
                let (rate, flags, start_type, start, stop_type, stop) = e.get();

                let state = self.state.lock().unwrap();
                let mut segment = state.segment.clone();
                drop(state);

                if segment
                    .do_seek(rate, flags, start_type, start, stop_type, stop)
                    .unwrap_or(false)
                {
                    let position =
                        if let gst::GenericFormattedValue::Time(Some(t)) = segment.start() {
                            t
                        } else {
                            gst::ClockTime::ZERO
                        };

                    if flags.contains(gst::SeekFlags::FLUSH) {
                        pad.push_event(gst::event::FlushStart::new());
                    }

                    if self.perform_seek(position) {
                        let mut state = self.state.lock().unwrap();

                        if flags.contains(gst::SeekFlags::FLUSH) {
                            segment.set_time(position);
                            let running_time = self
                                .obj()
                                .current_running_time()
                                .unwrap_or(gst::ClockTime::ZERO);
                            segment.set_base(running_time);

                            state.segment = segment.clone();
                            state.pending_segment = Some(segment);
                            state.discont_pending = true;
                        } else {
                            state.segment = segment;
                        }

                        self.cond.notify_all();
                        return true;
                    }
                }
                false
            }
            EventView::Qos(_) => true,
            _ => gst::Pad::event_default(pad, Some(&*self.obj()), event),
        }
    }

    fn sink_chain(
        &self,
        _pad: &gst::Pad,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst::info!(
            CAT,
            imp = self,
            "Sink chain received buffer PTS: {:?}",
            buffer.pts()
        );

        let mut state = self.state.lock().unwrap();
        let Some(rb) = state.buffer.as_mut() else {
            return Err(gst::FlowError::NotNegotiated);
        };
        rb.push_overwrite(buffer.upcast());
        state.total_written += 1;
        self.cond.notify_all();
        Ok(gst::FlowSuccess::Ok)
    }

    fn sink_event(&self, _pad: &gst::Pad, event: gst::Event) -> bool {
        gst::info!(CAT, imp = self, "Sink received event: {:?}", event);
        if event.is_serialized() {
            let mut state = self.state.lock().unwrap();
            let Some(rb) = state.buffer.as_mut() else {
                return false;
            };
            rb.push_overwrite(event.upcast());
            state.total_written += 1;
            self.cond.notify_all();
        }
        true
    }

    fn sink_query(&self, pad: &gst::Pad, query: &mut gst::QueryRef) -> bool {
        if query.is_serialized() {
            gst::debug!(CAT, imp = self, "Handling serialized query: {:?}", query);

            let mut state = self.state.lock().unwrap();

            if state.buffer.is_none() {
                return false;
            }

            let query_owned = query.to_owned();
            state.pending_query = Some(PendingQuery {
                query: query_owned.clone(),
                result: None,
            });

            // Push a marker query to the ring buffer to maintain serialization order,
            // and then wait for it to be processed
            let rb = state.buffer.as_mut().unwrap();
            rb.push_overwrite(query_owned.upcast());
            state.total_written += 1;
            self.cond.notify_all();

            loop {
                if let Some(ref pending) = state.pending_query {
                    if let Some(result) = pending.result {
                        state.pending_query = None;
                        gst::debug!(
                            CAT,
                            imp = self,
                            "Serialized query completed with result: {}",
                            result
                        );
                        return result;
                    }
                } else {
                    return false;
                }

                state = self.cond.wait(state).unwrap();
            }
        } else {
            gst::Pad::query_default(pad, Some(&*self.obj()), query)
        }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for Timeshift {
    const NAME: &'static str = "GstTimeshift";
    type Type = super::Timeshift;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.pad_template("src").unwrap();
        let srcpad = gst::Pad::builder_from_template(&templ)
            .event_function(|pad, parent, event| {
                Timeshift::catch_panic_pad_function(
                    parent,
                    || false,
                    |timeshift| timeshift.src_event(pad, event),
                )
            })
            .activatemode_function(|pad, parent, mode, active| {
                Timeshift::catch_panic_pad_function(
                    parent,
                    || {
                        Err(gst::loggable_error!(
                            CAT,
                            "Panic activating srcpad with mode"
                        ))
                    },
                    |timeshift| timeshift.src_activatemode(pad, mode, active),
                )
            })
            .query_function(|pad, parent, query| {
                Timeshift::catch_panic_pad_function(
                    parent,
                    || false,
                    |timeshift| timeshift.src_query(pad, query),
                )
            })
            .build();

        let templ = klass.pad_template("sink").unwrap();
        let sinkpad = gst::Pad::builder_from_template(&templ)
            .chain_function(|pad, parent, buffer| {
                Timeshift::catch_panic_pad_function(
                    parent,
                    || Err(gst::FlowError::Error),
                    |timeshift| timeshift.sink_chain(pad, buffer),
                )
            })
            .event_function(|pad, parent, event| {
                Timeshift::catch_panic_pad_function(
                    parent,
                    || false,
                    |timeshift| timeshift.sink_event(pad, event),
                )
            })
            .query_function(|pad, parent, query| {
                Timeshift::catch_panic_pad_function(
                    parent,
                    || false,
                    |timeshift| timeshift.sink_query(pad, query),
                )
            })
            .build();

        Self {
            sinkpad,
            srcpad,
            state: Mutex::new(Default::default()),
            cond: Condvar::new(),
            settings: Mutex::new(Default::default()),
        }
    }
}

impl ObjectImpl for Timeshift {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecUInt::builder("buffer-size")
                    .nick("Size")
                    .blurb("Ring buffer size")
                    .default_value(DEFAULT_BUFFER_SIZE)
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "buffer-size" => {
                let mut settings = self.settings.lock().unwrap();
                settings.buffer_size = value.get::<u32>().unwrap_or(DEFAULT_BUFFER_SIZE);
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "buffer-size" => {
                let settings = self.settings.lock().unwrap();
                settings.buffer_size.to_value()
            }
            _ => unimplemented!(),
        }
    }

    fn constructed(&self) {
        self.parent_constructed();
        let obj = self.obj();
        obj.add_pad(&self.srcpad).unwrap();
        obj.add_pad(&self.sinkpad).unwrap();
    }
}

impl GstObjectImpl for Timeshift {}

impl ElementImpl for Timeshift {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Timeshift",
                "Generic",
                "Timeshift",
                "Lluc Simó Margalef <lluc.simo@protonmail.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &gst::Caps::new_any(),
            )
            .expect("Failed to create src pad template");
            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &gst::Caps::new_any(),
            )
            .expect("Failed to create sink pad template");
            vec![src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }

    fn change_state(
        &self,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst::trace!(CAT, imp = self, "Changing state {:?}", transition);

        if transition == gst::StateChange::NullToReady {
            let mut state = self.state.lock().unwrap();
            let settings = self.settings.lock().unwrap();
            state.buffer = Some(Box::new(HeapRb::new(settings.buffer_size as usize)));
            drop(state);
            drop(settings);
        }

        let res = self.parent_change_state(transition)?;

        if transition == gst::StateChange::ReadyToNull {
            let mut state = self.state.lock().unwrap();
            *state = Default::default();
            self.cond.notify_all();
        }

        Ok(res)
    }
}

trait ConsumerExt {
    type Item;

    fn get(&self, index: usize) -> Option<&Self::Item>;
}

impl<S: Storage> ConsumerExt for SharedRb<S> {
    type Item = S::Item;

    fn get(&self, index: usize) -> Option<&Self::Item> {
        let (first, second) = self.as_slices();
        if index < first.len() {
            Some(&first[index])
        } else {
            let adj_index = index - first.len();
            if adj_index < second.len() {
                Some(&second[adj_index])
            } else {
                None
            }
        }
    }
}

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "timeshift",
        gst::DebugColorFlags::empty(),
        Some("Timeshift element"),
    )
});
