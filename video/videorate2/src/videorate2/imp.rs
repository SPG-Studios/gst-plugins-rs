// Copyright (C) 2026 Jochen Henneberg <jh@henneberg-systemdesign.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;

use core::f64;
use std::cmp::Ordering;
use std::fmt;
use std::ops::Neg;
use std::sync::{LazyLock, Mutex};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "videorate2",
        gst::DebugColorFlags::empty(),
        Some("Videorate Element"),
    )
});

// Property defaults.
const DEFAULT_NEW_PREF: f64 = 1.0;
const DEFAULT_DROP_ONLY: bool = false;
const DEFAULT_SKIP_TO_FIRST: bool = false;
const DEFAULT_MAX_CLOSING_SEGMENT_DUPLICATION_DURATION: u64 = 1_000_000_000;
const DEFAULT_MAX_DUPLICATION_TIME: Option<u64> = None;
const DEFAULT_RATE: f64 = 1.0;

// Statistics read-only properties
const DEFAULT_IN: u64 = 0;
const DEFAULT_OUT: u64 = 0;
const DEFAULT_DROPPED: u64 = 0;
const DEFAULT_DUPLICATE: u64 = 0;

// Property value storage
#[derive(Debug)]
struct Settings {
    new_pref: f64,
    drop_only: bool,
    skip_to_first: bool,
    in_bufs: u64,
    out_bufs: u64,
    dropped_bufs: u64,
    duplicated_bufs: u64,
    max_closing_segment_duplication_duration: u64,
    max_duplication_time: Option<u64>,
    rate: f64,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            new_pref: DEFAULT_NEW_PREF,
            drop_only: DEFAULT_DROP_ONLY,
            skip_to_first: DEFAULT_SKIP_TO_FIRST,
            in_bufs: DEFAULT_IN,
            out_bufs: DEFAULT_OUT,
            dropped_bufs: DEFAULT_DROPPED,
            duplicated_bufs: DEFAULT_DUPLICATE,
            max_closing_segment_duplication_duration:
                DEFAULT_MAX_CLOSING_SEGMENT_DUPLICATION_DURATION,
            max_duplication_time: DEFAULT_MAX_DUPLICATION_TIME,
            rate: DEFAULT_RATE,
        }
    }
}

impl fmt::Display for Settings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "in {}, out: {}, dropped: {}",
            self.in_bufs, self.out_bufs, self.dropped_bufs
        )
    }
}

#[derive(Debug)]
enum OperationMode {
    Passthrough,
    Resample(gst::Fraction),
}

impl OperationMode {
    fn update_direction(&mut self, reverse: bool) {
        match (self, reverse) {
            (Self::Passthrough, _) => (),
            (Self::Resample(interval), true) if interval.numer().is_positive() => {
                *interval = interval.neg()
            }
            (Self::Resample(interval), false) if interval.numer().is_negative() => {
                *interval = interval.neg()
            }
            _ => (),
        }
    }

    // Calculate the duration for the given number of frames.
    fn duration(&self, frames: u64) -> Option<gst::Signed<gst::ClockTime>> {
        match self {
            Self::Passthrough => None,
            Self::Resample(d) => {
                let ts = gst::Signed::Positive(
                    (frames * gst::ClockTime::SECOND)
                        .mul_div_round(d.denom() as u64, d.numer().unsigned_abs() as u64)?,
                );
                if d.numer().is_negative() {
                    Some(ts.neg())
                } else {
                    Some(ts)
                }
            }
        }
    }
}

#[derive(Debug)]
struct SplitInterval {
    start: gst::ClockTime,
    stop: gst::ClockTime,
    // Split position of the interval, depends on 'new-pref' property
    // value.
    split: gst::ClockTime,
}

impl fmt::Display for SplitInterval {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} <- {} -> {}", self.start, self.split, self.stop)
    }
}

impl SplitInterval {
    fn drop(&self, max_dup_time: Option<u64>) -> bool {
        max_dup_time.is_some_and(|t| self.stop.saturating_sub(self.start).nseconds() > t)
    }
}

#[derive(Debug)]
struct State {
    mode: OperationMode,
    in_framerate: Option<gst::Fraction>,
    out_framerate: Option<gst::Fraction>,
    segment: Option<gst::Segment>,
    buffer: Option<gst::Buffer>,
    buffer_pts: Option<gst::ClockTime>,
    buffer_duration: Option<gst::ClockTime>,
    buffer_used: bool,
    start_time: Option<gst::ClockTime>,
    current_time: Option<gst::ClockTime>,
    current_buf: u64,
    pending_caps: Option<gst::Caps>,
    pending_segment: Option<gst::Segment>,
}

impl Default for State {
    fn default() -> State {
        State {
            mode: OperationMode::Passthrough,
            in_framerate: None,
            out_framerate: None,
            segment: None,
            buffer: None,
            buffer_pts: None,
            buffer_duration: None,
            buffer_used: false,
            start_time: None,
            current_time: None,
            current_buf: 0,
            pending_caps: None,
            pending_segment: None,
        }
    }
}

impl State {
    fn reverse(segment: &gst::Segment) -> bool {
        segment.rate() < 0.0
    }

    fn start_stop(segment: &gst::Segment) -> (Option<gst::ClockTime>, Option<gst::ClockTime>) {
        if let Some(seg) = segment.downcast_ref::<gst::ClockTime>() {
            if Self::reverse(segment) {
                (seg.stop(), seg.start())
            } else {
                (seg.start(), seg.stop())
            }
        } else {
            (None, None)
        }
    }

    fn is_reverse(&self) -> bool {
        let Some(ref s) = self.segment else {
            return false;
        };
        Self::reverse(s)
    }

    // Handles new segments, use if first segment or no buffers yet,
    // make pending if relevant changes compared to the current
    // segment. Replace the current segment with the pending segment
    // if no segment parameter is set. Returns true if the current
    // buffer has been dropped unused.
    // Returns new current_time of modified.
    fn update_segment(
        &mut self,
        segment: Option<&gst::Segment>,
        skip_to_first: bool,
    ) -> Option<gst::ClockTime> {
        let seg = segment.cloned().or(self.pending_segment.take())?;
        let rev = self.is_reverse();

        match (&mut self.segment, &segment) {
            (Some(ref mut s), None) => {
                gst::debug!(
                    CAT,
                    "Replace the current segment {s:?} with the pending {seg:?}"
                );
                let mut ret = None;
                if let Some(start_time) = Self::start_stop(&seg).0 {
                    if (start_time < self.current_time.unwrap()) == rev {
                        self.current_time = Some(start_time);
                        self.current_buf = 0;
                        ret = self.current_time;
                    }
                    self.start_time = Some(start_time);
                }

                // Replace the segment.
                *s = seg;
                // Update the mode according to the new direction.
                self.mode.update_direction(self.is_reverse());
                ret
            }
            (None, Some(ref s)) | (Some(_), Some(ref s)) if self.buffer.is_none() => {
                gst::debug!(CAT, "Use the new segment {s:?}");
                // Get the new start time.
                self.start_time = if skip_to_first {
                    None
                } else {
                    Self::start_stop(s).0
                };
                self.current_time = self.start_time;
                self.current_buf = 0;

                // Replace the segment.
                self.segment = segment.cloned();
                // Update the mode according to the new direction.
                self.mode.update_direction(self.is_reverse());
                self.current_time
            }
            (_, Some(ref s)) => {
                gst::debug!(CAT, "Make new segment pending {s:?}");
                self.pending_segment = segment.cloned();
                None
            }
            (None, None) => unreachable!(),
        }
    }

    // On new framerates (in or out) the transform mode is updated.
    fn update_framerates(&mut self) {
        self.mode = match (self.in_framerate, self.out_framerate) {
            (_, None) => OperationMode::Passthrough,
            (None, Some(out_rate)) => {
                if out_rate.numer() > 0 {
                    // The unwrap() is safe here because framerate can be
                    // expected to be >= 0 from previous check.
                    let mut interval = out_rate;
                    if self.is_reverse() {
                        interval = interval.neg()
                    }
                    OperationMode::Resample(interval)
                } else {
                    OperationMode::Passthrough
                }
            }
            (Some(in_rate), Some(out_rate)) => {
                // Passthrough if invalid framerate.
                if in_rate.numer() > 0 && out_rate.numer() > 0 {
                    // The unwrap() is safe here because framerate can be
                    // expected to be >= 0 from previous check.
                    let mut interval = out_rate;
                    if self.is_reverse() {
                        interval = interval.neg()
                    }

                    let scale = out_rate / in_rate;
                    let one = gst::Fraction::new(1, 1);
                    match scale.cmp(&one) {
                        Ordering::Less | Ordering::Greater => OperationMode::Resample(interval),
                        Ordering::Equal => OperationMode::Passthrough,
                    }
                } else {
                    OperationMode::Passthrough
                }
            }
        };
    }

    fn update_start_time(&mut self, buffer: &gst::Buffer) -> Result<(), gst::FlowError> {
        // If there is no buffer yet and there is no start_time yet
        // then skip-to-first must be set which means the start time
        // is set to the next_buffer PTS.
        match (&self.buffer, self.start_time) {
            // Start time has been set from first segment or previous
            // buffer, default match arm, (fast path).
            (_, Some(_)) => Ok(()),
            // Need to set start time to buffer PTS.
            (None, None) => {
                self.start_time = self
                    .buffer_start_time(buffer.pts(), buffer.duration(), DEFAULT_RATE)
                    .ok();
                self.current_time = self.start_time;
                self.current_buf = 0;
                Ok(())
            }
            _ => Err(gst::FlowError::Error),
        }
    }

    // Forward the current time and return the duration for the next
    // buffer.
    fn update_current_time(&mut self) -> Option<gst::ClockTime> {
        let current_time = self.current_time?;

        self.current_buf = self.current_buf.saturating_add(1);
        let d = self.mode.duration(self.current_buf)?;

        self.current_time = Some((self.start_time.unwrap() + d).abs());
        Some(current_time.absdiff(self.current_time.unwrap()))
    }

    // Replace current buffer with next buffer, return true if current
    // buffer has never been pushed downstream.
    fn update_buffer(&mut self, mut buffer: Option<gst::Buffer>) -> bool {
        let dropped = self.buffer.is_some() && !self.buffer_used;

        if let Some(ref mut buffer) = buffer {
            self.buffer_pts = buffer.pts();
            self.buffer_duration = buffer.duration();
            if let Some(d) = self.mode.duration(1) {
                buffer.get_mut().unwrap().set_duration(d.abs());
            }
        } else {
            self.buffer_pts = None;
            self.buffer_duration = None;
        }
        self.buffer = buffer;
        self.buffer_used = false;

        dropped
    }

    // Get the start time of a buffer which is either PTS or PTS +
    // duration in case of reverse playback.
    fn buffer_start_time(
        &self,
        pts: Option<gst::ClockTime>,
        duration: Option<gst::ClockTime>,
        rate: f64,
    ) -> Result<gst::ClockTime, gst::FlowError> {
        pts.map(|t| {
            let ts = if self.is_reverse() {
                t.saturating_add(duration.unwrap_or(0.seconds()))
            } else {
                t
            };
            gst::ClockTime::try_from_seconds_f64(ts.seconds_f64() / rate).unwrap()
        })
        .ok_or(gst::FlowError::Error)
    }

    // Calculate the interval and split position from current and next
    // buffer.
    fn interval(
        &self,
        next_buffer: &gst::Buffer,
        new_pref: f64,
        max_seg_close_duration: u64,
        rate: f64,
    ) -> Result<SplitInterval, gst::FlowError> {
        // Get the interval from the last buffer or start_time until
        // the next buffer and calculate the split point when switching
        // from current to next buffer.
        let start = match &self.buffer {
            Some(_) => self.buffer_start_time(self.buffer_pts, self.buffer_duration, rate)?,
            None => self.current_time.unwrap(),
        };
        let stop = self.buffer_start_time(next_buffer.pts(), next_buffer.duration(), rate)?;
        let mut split = match new_pref {
            0.0 => stop,
            1.0 => start,
            _ => {
                let offset = gst::ClockTime::try_from_seconds_f64(
                    new_pref * stop.absdiff(start).seconds_f64(),
                )
                .unwrap();
                if self.is_reverse() {
                    start - offset
                } else {
                    start + offset
                }
            }
        };

        // If there is a pending segment the split point may
        // change. If the current segment has an end position the
        // split point is that end position. Otherwise, if the next
        // segment is forward of the current segment the split
        // position is the start position of the next
        // segment. Otherwise use the default split logic.
        if let Some(pending_segment) = self.pending_segment.as_ref() {
            let end_current_segment = Self::start_stop(self.segment.as_ref().unwrap()).1;
            let start_next_segment = Self::start_stop(pending_segment).0;
            let current_time = self.current_time.unwrap();

            split = if let Some(end_current_segment) = end_current_segment {
                end_current_segment
            } else if start_next_segment
                .is_some_and(|ts| ts != current_time && ((ts < current_time) == self.is_reverse()))
            {
                start_next_segment.unwrap()
            } else {
                split
            };

            // Limit split - start interval to max_seg_close_duration.
            if (split.absdiff(start)).nseconds() > max_seg_close_duration {
                split = if self.is_reverse() {
                    gst::ClockTime::from_nseconds(start.nseconds() - max_seg_close_duration)
                } else {
                    gst::ClockTime::from_nseconds(start.nseconds() + max_seg_close_duration)
                }
            }
        }

        Ok(SplitInterval { start, stop, split })
    }
}

// Struct containing all the element data
pub struct Videorate {
    srcpad: gst::Pad,
    sinkpad: gst::Pad,
    settings: Mutex<Settings>,
    state: Mutex<State>,
}

impl Videorate {
    fn do_other_pad_caps_query(&self, pad: &gst::Pad, caps: &mut gst::Caps) -> bool {
        let (other_pad, store_framerate) = match pad.direction() {
            gst::PadDirection::Src => (&self.sinkpad, false),
            gst::PadDirection::Sink => (&self.srcpad, true),
            gst::PadDirection::Unknown => unreachable!(),
        };

        let framerate_full_range = gst::FractionRange::new(0, i32::MAX);
        caps.make_mut()
            .set_value("framerate", (&framerate_full_range).into());
        let caps = other_pad.peer_query_caps(Some(caps));
        if store_framerate {
            let rate = caps
                .structure(0)
                .unwrap()
                .get::<gst::Fraction>("framerate")
                .ok();
            let mut state = self.state.lock().unwrap();
            state.out_framerate = rate;
            state.update_framerates();
            drop(state);

            if caps.is_fixed() {
                let ev = gst::event::Caps::new(&caps);
                other_pad.push_event(ev);
            }
        }

        true
    }

    fn on_caps_query(&self, pad: &gst::Pad, query: &mut gst::query::Caps) -> bool {
        let allowed_caps = pad.pad_template_caps();
        let res = query
            .filter()
            .unwrap_or(&allowed_caps)
            .intersect_with_mode(&allowed_caps, gst::CapsIntersectMode::First);
        query.set_result(&res);

        let mut filter = query.filter_owned().unwrap_or(allowed_caps);
        self.do_other_pad_caps_query(pad, &mut filter)
    }

    fn on_accept_caps_query(&self, pad: &gst::Pad, query: &mut gst::query::AcceptCaps) -> bool {
        let accept = pad.pad_template_caps().can_intersect(query.caps());
        query.set_result(accept);

        true
    }

    fn time_outside_interval(
        now: &gst::ClockTime,
        until: gst::ClockTime,
        reverse: bool,
        including: bool,
    ) -> bool {
        match (*now).cmp(&until) {
            Ordering::Equal => !including,
            Ordering::Less => reverse,
            Ordering::Greater => !reverse,
        }
    }

    fn time_inside_interval(
        now: &gst::ClockTime,
        until: gst::ClockTime,
        reverse: bool,
        including: bool,
    ) -> bool {
        !Self::time_outside_interval(now, until, reverse, including)
    }

    fn push_buffers(
        &self,
        state: &mut State,
        until: gst::ClockTime,
        including: bool,
        buffer: &mut gst::Buffer,
        count: &mut u64,
        drop_only: bool,
    ) -> Result<(), gst::FlowError> {
        let mut current_time = state.current_time.unwrap();
        let reverse = state.is_reverse();
        let used = state.buffer_used;

        if Self::time_outside_interval(&current_time, until, reverse, including) {
            gst::debug!(
                CAT,
                imp = self,
                "Don't push buffers because outside interval"
            );
            return Ok(());
        }

        gst::trace!(
            CAT,
            imp = self,
            "Push buffer from {:?} -> {:?}",
            current_time,
            until
        );
        let buffer = buffer.make_mut();

        let mut push = !drop_only || !used;
        while Self::time_inside_interval(&current_time, until, reverse, including) {
            if reverse {
                buffer.set_pts(current_time.saturating_sub(buffer.duration().unwrap()));
            } else {
                buffer.set_pts(current_time);
            }
            buffer.set_offset(*count);

            if push {
                gst::trace!(
                    CAT,
                    imp = self,
                    "Pushing buffer {:?} at {:?}",
                    buffer,
                    current_time
                );
                self.srcpad.push(buffer.to_owned())?;
                *count += 1;
            }
            push = !drop_only;
            let d = state.update_current_time().unwrap();
            buffer.set_duration(d);
            current_time = state.current_time.unwrap();
        }

        state.buffer_used = true;
        Ok(())
    }

    fn on_next_buffer(&self, next_buffer: gst::Buffer) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut state = self.state.lock().unwrap();

        state.update_start_time(&next_buffer)?;
        gst::debug!(CAT, imp = self, "State: {:?}", state);
        assert!(state.current_time.is_some());

        // In case of passthrough just forward the buffer and update
        // the statistics.
        if matches!(state.mode, OperationMode::Passthrough) {
            gst::debug!(CAT, imp = self, "Passthrough mode - forward buffer");
            self.settings.lock().unwrap().out_bufs += 1;
            return self.srcpad.push(next_buffer);
        }

        let (new_pref, drop_only, max_seg_close_duration, max_dup_time, rate, out_bufs) = {
            let settings = self.settings.lock().unwrap();
            (
                settings.new_pref,
                settings.drop_only,
                settings.max_closing_segment_duplication_duration,
                settings.max_duplication_time,
                settings.rate,
                settings.out_bufs,
            )
        };

        // Calculate the current SplitInterval from new-pref and
        // buffer timestamps.
        let interval = state.interval(&next_buffer, new_pref, max_seg_close_duration, rate)?;
        gst::debug!(CAT, imp = self, "Process interval {}", interval);

        // Push duplicates of old buffer if exists.
        let mut out = out_bufs;
        if let Some(mut buf) = state.buffer.clone() {
            let drop_only = drop_only || interval.drop(max_dup_time);
            gst::trace!(CAT, imp = self, "Try to push old buffer");
            self.push_buffers(
                &mut state,
                interval.split,
                false,
                &mut buf,
                &mut out,
                drop_only,
            )?;
        }
        let mut duplicated = out;

        // Check if there is a new segment and the current time needs an update.
        state.update_segment(None, false);

        // Skip to the next buffer and remember if this buffer has
        // been dropped.
        let dropped = state.update_buffer(Some(next_buffer));

        if let Some(ref caps) = state.pending_caps.take() {
            gst::debug!(CAT, imp = self, "Pending caps {caps:?}, re-negotiate");
            let ev = gst::event::Caps::builder(caps).build();
            self.srcpad.push_event(ev);
        }

        // Push duplicates of next buffer.
        if let Some(mut buf) = state.buffer.clone() {
            gst::trace!(CAT, imp = self, "Try to push next buffer");
            self.push_buffers(
                &mut state,
                interval.stop,
                true,
                &mut buf,
                &mut out,
                drop_only,
            )?;
        }

        // Out has counted forward here, so this checks if the new
        // buffer has been pushed at all. If so, we have to substract
        // 1 to get the duplicates.
        if out > duplicated {
            duplicated = out.saturating_sub(1);
        }
        duplicated = duplicated.saturating_sub(out_bufs);
        drop(state);

        {
            let mut settings = self.settings.lock().unwrap();
            settings.out_bufs = out;
            settings.duplicated_bufs += duplicated;
            dropped.then(|| settings.dropped_bufs += 1);
            gst::trace!(CAT, imp = self, "Statistics: {}", settings);
        }

        Ok(gst::FlowSuccess::Ok)
    }

    fn sink_chain(
        &self,
        pad: &gst::Pad,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst::trace!(CAT, obj = pad, "Handling buffer {:?}", buffer);

        if self.state.lock().unwrap().segment.is_none() {
            gst::error!(CAT, obj = pad, "Buffer without segment");
            return Err(gst::FlowError::Error);
        };

        if buffer.pts().is_none() {
            gst::error!(CAT, obj = pad, "Buffer without PTS");
            return Err(gst::FlowError::Error);
        };

        self.settings.lock().unwrap().in_bufs += 1;
        self.on_next_buffer(buffer)
    }

    fn sink_event(&self, pad: &gst::Pad, event: gst::Event) -> bool {
        gst::log!(CAT, obj = pad, "Handling event {:?}", event);

        match event.view() {
            gst::EventView::Caps(e) => {
                let mut caps = e.caps_owned();

                {
                    let mut state = self.state.lock().unwrap();
                    let in_rate = caps
                        .structure(0)
                        .unwrap()
                        .get::<gst::Fraction>("framerate")
                        .ok();

                    caps.make_mut().set_value(
                        "framerate",
                        (&state.out_framerate.unwrap_or(gst::Fraction::new(0, 1))).into(),
                    );

                    // If we have seen a buffer already the caps
                    // update is postponed until that buffer is fully
                    // processed.
                    if state.buffer.is_some() {
                        state.pending_caps = Some(caps);
                        return true;
                    } else {
                        state.in_framerate = in_rate;
                        state.update_framerates();
                        gst::debug!(CAT, imp = self, "New mode: {:?}", state.mode);
                    }
                }

                let ev = gst::event::Caps::builder(&caps).seqnum(e.seqnum()).build();
                self.srcpad.push_event(ev)
            }
            gst::EventView::Segment(e) => {
                let segment = e.segment();
                gst::log!(CAT, imp = self, "Received segment {:?}", segment);
                self.state
                    .lock()
                    .unwrap()
                    .update_segment(Some(segment), self.settings.lock().unwrap().skip_to_first);
                gst::debug!(CAT, imp = self, "State: {:?}", self.state);
                self.srcpad.push_event(event)
            }
            _ => self.srcpad.push_event(event),
        }
    }

    fn src_event(&self, pad: &gst::Pad, event: gst::Event) -> bool {
        gst::log!(CAT, obj = pad, "Handling event {:?}", event);

        match event.view() {
            gst::EventView::Caps(e) => {
                let mut caps = e.caps_owned();

                {
                    let mut state = self.state.lock().unwrap();
                    state.in_framerate = caps
                        .structure(0)
                        .unwrap()
                        .get::<gst::Fraction>("framerate")
                        .ok();
                    state.update_framerates();
                    gst::debug!(CAT, imp = self, "New mode: {:?}", state.mode);

                    caps.make_mut().set_value(
                        "framerate",
                        (&state.out_framerate.unwrap_or(gst::Fraction::new(0, 1))).into(),
                    );
                }

                let ev = gst::event::Caps::builder(&caps).seqnum(e.seqnum()).build();
                self.srcpad.push_event(ev)
            }
            gst::EventView::Reconfigure(e) => {
                gst::log!(CAT, imp = self, "Received reconfigure {:?}", e);

                let mut caps = self
                    .sinkpad
                    .current_caps()
                    .unwrap_or(self.srcpad.pad_template_caps());
                self.do_other_pad_caps_query(&self.sinkpad, &mut caps)
            }
            _ => self.sinkpad.push_event(event),
        }
    }

    fn sink_query(&self, pad: &gst::Pad, query: &mut gst::QueryRef) -> bool {
        gst::log!(CAT, obj = pad, "Handling query {:?}", query);
        match query.view_mut() {
            gst::QueryViewMut::Caps(q) => self.on_caps_query(pad, q),
            gst::QueryViewMut::AcceptCaps(q) => self.on_accept_caps_query(pad, q),
            _ => self.srcpad.peer_query(query),
        }
    }

    fn src_query(&self, pad: &gst::Pad, query: &mut gst::QueryRef) -> bool {
        gst::log!(CAT, obj = pad, "Handling query {:?}", query);
        match query.view_mut() {
            gst::QueryViewMut::Caps(q) => self.on_caps_query(pad, q),
            gst::QueryViewMut::AcceptCaps(q) => self.on_accept_caps_query(pad, q),
            _ => self.sinkpad.peer_query(query),
        }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for Videorate {
    const NAME: &'static str = "GstRsVideorate";
    type Type = super::Videorate;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.pad_template("sink").unwrap();
        let sinkpad = gst::Pad::builder_from_template(&templ)
            .chain_function(|pad, parent, buffer| {
                Videorate::catch_panic_pad_function(
                    parent,
                    || Err(gst::FlowError::Error),
                    |videorate| videorate.sink_chain(pad, buffer),
                )
            })
            .event_function(|pad, parent, event| {
                Videorate::catch_panic_pad_function(
                    parent,
                    || false,
                    |videorate| videorate.sink_event(pad, event),
                )
            })
            .query_function(|pad, parent, query| {
                Videorate::catch_panic_pad_function(
                    parent,
                    || false,
                    |videorate| videorate.sink_query(pad, query),
                )
            })
            .build();

        let templ = klass.pad_template("src").unwrap();
        let srcpad = gst::Pad::builder_from_template(&templ)
            .query_function(|pad, parent, query| {
                Videorate::catch_panic_pad_function(
                    parent,
                    || false,
                    |videorate| videorate.src_query(pad, query),
                )
            })
            .event_function(|pad, parent, event| {
                Videorate::catch_panic_pad_function(
                    parent,
                    || false,
                    |videorate| videorate.src_event(pad, event),
                )
            })
            .build();

        Self {
            srcpad,
            sinkpad,
            settings: Default::default(),
            state: Default::default(),
        }
    }
}

impl ObjectImpl for Videorate {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecDouble::builder("new-pref")
                    .nick("Preference for new frames")
                    .blurb("Value indicating how much to prefer new frames")
                    .minimum(0.0)
                    .maximum(1.0)
                    .default_value(DEFAULT_NEW_PREF)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoolean::builder("drop-only")
                    .nick("Only drop frames")
                    .blurb("Only drop frames, no duplicates are produced")
                    .default_value(DEFAULT_DROP_ONLY)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoolean::builder("skip-to-first")
                    .nick("No buffers before first")
                    .blurb("Don't produce buffers before the first one we receive")
                    .default_value(DEFAULT_SKIP_TO_FIRST)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt64::builder("max-closing-segment-duplication-duration")
                    .nick("Maximum closing segment duplication duration")
                    .blurb("Maximum duration of duplicated buffers to close current segment")
                    .default_value(DEFAULT_MAX_CLOSING_SEGMENT_DUPLICATION_DURATION)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt64::builder("max-duplication-time")
                    .nick("Maximum time to duplicate a frame")
                    .blurb("Do not duplicate frames if the gap exceeds this period (in ns) (0 = disabled)")
                    .default_value(DEFAULT_MAX_DUPLICATION_TIME.unwrap_or(0))
                    .mutable_ready()
                    .build(),
                glib::ParamSpecDouble::builder("rate")
                    .nick("Factor of speed for frame displaying")
                    .blurb("Speed for frame displaying")
                    .minimum(0.0)
                    .maximum(f64::MAX)
                    .default_value(DEFAULT_RATE)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt64::builder("in")
                    .nick("Input frames")
                    .blurb("Number of input frames")
                    .default_value(DEFAULT_IN)
                    .flags(glib::ParamFlags::READABLE)
                    .build(),
                glib::ParamSpecUInt64::builder("out")
                    .nick("Output frames")
                    .blurb("Number of output frames")
                    .default_value(DEFAULT_OUT)
                    .flags(glib::ParamFlags::READABLE)
                    .build(),
                glib::ParamSpecUInt64::builder("drop")
                    .nick("Dropped frames")
                    .blurb("Number of dropped frames")
                    .default_value(DEFAULT_DROPPED)
                    .flags(glib::ParamFlags::READABLE)
                    .build(),
                glib::ParamSpecUInt64::builder("duplicate")
                    .nick("Dropped frames")
                    .blurb("Number of duplicated frames")
                    .default_value(DEFAULT_DUPLICATE)
                    .flags(glib::ParamFlags::READABLE)
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
        let mut settings = self.settings.lock().unwrap();

        match pspec.name() {
            "new-pref" => {
                let new_pref = value.get().expect("type checked upstream");
                gst::info!(
                    CAT,
                    imp = self,
                    "Changing new-pref from {} to {}",
                    settings.new_pref,
                    new_pref
                );
                settings.new_pref = new_pref;
                drop(settings);
            }
            "drop-only" => {
                let drop_only = value.get().expect("type checked upstream");
                gst::info!(
                    CAT,
                    imp = self,
                    "Changing drop-only from {} to {}",
                    settings.drop_only,
                    drop_only
                );
                settings.drop_only = drop_only;
            }
            "skip-to-first" => {
                let skip_to_first = value.get().expect("type checked upstream");
                gst::info!(
                    CAT,
                    imp = self,
                    "Changing skip-to-first from {} to {}",
                    settings.skip_to_first,
                    skip_to_first
                );
                settings.skip_to_first = skip_to_first;
            }
            "max-closing-segment-duplication-duration" => {
                let max_closing_segment_duplication_duration =
                    value.get().expect("type checked upstream");
                gst::info!(
                    CAT,
                    imp = self,
                    "Changing max-closing-segment-duplication-duration from {} to {}",
                    settings.max_closing_segment_duplication_duration,
                    max_closing_segment_duplication_duration
                );
                settings.max_closing_segment_duplication_duration =
                    max_closing_segment_duplication_duration;
            }
            "max-duplication-time" => {
                let max_dup_time = value.get().expect("type checked upstream");
                gst::info!(
                    CAT,
                    imp = self,
                    "Changing max-duplication-time from {} to {}",
                    settings.max_duplication_time.unwrap_or(0),
                    max_dup_time
                );
                if max_dup_time > 0 {
                    settings.max_duplication_time = Some(max_dup_time);
                } else {
                    settings.max_duplication_time = None;
                }
            }
            "rate" => {
                let rate = value.get().expect("type checked upstream");
                gst::info!(
                    CAT,
                    imp = self,
                    "Changing rate from {} to {}",
                    settings.rate,
                    rate
                );
                settings.rate = rate;
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "new-pref" => self.settings.lock().unwrap().new_pref.to_value(),
            "drop-only" => self.settings.lock().unwrap().drop_only.to_value(),
            "skip-to-first" => self.settings.lock().unwrap().skip_to_first.to_value(),
            "in" => self.settings.lock().unwrap().in_bufs.to_value(),
            "out" => self.settings.lock().unwrap().out_bufs.to_value(),
            "drop" => self.settings.lock().unwrap().dropped_bufs.to_value(),
            "duplicate" => self.settings.lock().unwrap().duplicated_bufs.to_value(),
            "max-closing-segment-duplication-duration" => self
                .settings
                .lock()
                .unwrap()
                .max_closing_segment_duplication_duration
                .to_value(),
            "max-duplication-time" => self
                .settings
                .lock()
                .unwrap()
                .max_duplication_time
                .unwrap_or(0)
                .to_value(),
            "rate" => self.settings.lock().unwrap().rate.to_value(),
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for Videorate {}

impl ElementImpl for Videorate {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Videorate",
                "Video",
                "Adjust framerate on the output",
                "Jochen Henneberg <jochen@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            // Our element can accept any possible caps on both pads
            let caps = gst::Caps::builder("video/x-raw")
                .any_features()
                .field("framerate", gst::FractionRange::new(0, i32::MAX))
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
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst::trace!(CAT, imp = self, "Changing state {:?}", transition);

        self.parent_change_state(transition)
    }
}
