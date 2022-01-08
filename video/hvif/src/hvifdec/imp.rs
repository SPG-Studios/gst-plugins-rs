// Copyright (c) 2021 Emmanuel Gil Peyrot <linkmauve@linkmauve.fr>
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst::{gst_log, gst_trace};

use hvif::Hvif;
use once_cell::sync::Lazy;
use std::sync::Mutex;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "hvifdec",
        gst::DebugColorFlags::empty(),
        Some("HVIF decoder"),
    )
});

#[derive(Default)]
struct State {
    buffers: Vec<gst::Buffer>,
    total_size: usize,
}

pub struct HvifDec {
    srcpad: gst::Pad,
    sinkpad: gst::Pad,
    state: Mutex<State>,
}

impl HvifDec {
    fn sink_chain(
        &self,
        pad: &gst::Pad,
        _element: &super::HvifDecoder,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst_log!(CAT, obj: pad, "Handling buffer {:?}", buffer);

        let mut state = self.state.lock().unwrap();

        state.total_size += buffer.size();
        state.buffers.push(buffer);

        Ok(gst::FlowSuccess::Ok)
    }

    fn decode(&self, _element: &super::HvifDecoder) -> Result<(), gst::ErrorMessage> {
        let timestamp = gst::ClockTime::ZERO;
        let duration = gst::ClockTime::from_seconds(1);
        let mut state = self.state.lock().unwrap();

        if state.buffers.is_empty() {
            return Err(gst::error_msg!(
                gst::StreamError::Decode,
                ["No valid frames decoded before end of stream"]
            ));
        }

        let mut buf = Vec::with_capacity(state.total_size);

        for buffer in state.buffers.drain(..) {
            buf.extend_from_slice(&buffer.map_readable().expect("Failed to map buffer"));
        }

        drop(state);

        let hvif = Hvif::parse(&buf)
            .map_err(|_| gst::error_msg!(gst::StreamError::Decode, ["Failed to decode picture"]))?;

        // TODO: Allow the output size to be selected by the src pad’s caps.
        let width = 64;
        let height = 64;

        let caps = gst_video::VideoInfo::builder(gst_video::VideoFormat::Bgra, width, height)
            .fps((0, 1))
            .build()
            .unwrap()
            .to_caps()
            .unwrap();

        let segment = gst::FormattedSegment::<gst::ClockTime>::new();
        let _ = self.srcpad.push_event(gst::event::Caps::new(&caps));
        let _ = self.srcpad.push_event(gst::event::Segment::new(&segment));

        let mut out_buf = gst::Buffer::with_size((width * height * 4) as usize).unwrap();

        {
            // TODO: figure out why map_writable() doesn’t exist yet, and stop abusing
            // map_readable() to get a writable buffer.
            let map = out_buf.map_readable().unwrap();
            let buf = map.as_slice().as_ptr() as *mut u8;
            let format = cairo::Format::ARgb32;
            let width = width as i32;
            let height = height as i32;
            let stride = width * 4;
            // Safety: The safe cairo::ImageSurface::create_for_data() requires the slice to be
            // 'static, which is not possible here, hence the use of the unsafe method.
            let mut surface = unsafe {
                cairo::ImageSurface::create_for_data_unsafe(buf, format, width, height, stride)
            }
            .map_err(|_| {
                gst::error_msg!(gst::StreamError::Decode, ["Failed to create cairo surface"])
            })?;
            hvif::render(hvif.1, &mut surface).map_err(|_| {
                gst::error_msg!(
                    gst::StreamError::Decode,
                    ["Failed to render HVIF using cairo"]
                )
            })?;
        }

        {
            let out_buf_mut = out_buf.get_mut().unwrap();
            out_buf_mut.set_pts(timestamp);
            out_buf_mut.set_duration(duration);
        }

        match self.srcpad.push(out_buf) {
            Ok(_) => (),
            Err(gst::FlowError::Flushing) | Err(gst::FlowError::Eos) => (),
            Err(flow) => {
                return Err(gst::error_msg!(
                    gst::StreamError::Failed,
                    ["Failed to push buffers: {:?}", flow]
                ));
            }
        }

        Ok(())
    }

    fn sink_event(&self, pad: &gst::Pad, element: &super::HvifDecoder, event: gst::Event) -> bool {
        use gst::EventView;

        gst_log!(CAT, obj: pad, "Handling event {:?}", event);
        match event.view() {
            EventView::FlushStop(..) => {
                let mut state = self.state.lock().unwrap();
                *state = State::default();
                pad.event_default(Some(element), event)
            }
            EventView::Eos(..) => {
                if let Err(err) = self.decode(element) {
                    element.post_error_message(err);
                }
                pad.event_default(Some(element), event)
            }
            EventView::Segment(..) => true,
            _ => pad.event_default(Some(element), event),
        }
    }

    fn src_event(&self, pad: &gst::Pad, element: &super::HvifDecoder, event: gst::Event) -> bool {
        use gst::EventView;

        gst_log!(CAT, obj: pad, "Handling event {:?}", event);
        match event.view() {
            EventView::Seek(..) => false,
            _ => pad.event_default(Some(element), event),
        }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for HvifDec {
    const NAME: &'static str = "HvifDec";
    type Type = super::HvifDecoder;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.pad_template("sink").unwrap();
        let sinkpad = gst::Pad::builder_with_template(&templ, Some("sink"))
            .chain_function(|pad, parent, buffer| {
                HvifDec::catch_panic_pad_function(
                    parent,
                    || Err(gst::FlowError::Error),
                    |dec, element| dec.sink_chain(pad, element, buffer),
                )
            })
            .event_function(|pad, parent, event| {
                HvifDec::catch_panic_pad_function(
                    parent,
                    || false,
                    |dec, element| dec.sink_event(pad, element, event),
                )
            })
            .build();

        let templ = klass.pad_template("src").unwrap();
        let srcpad = gst::Pad::builder_with_template(&templ, Some("src"))
            .event_function(|pad, parent, event| {
                HvifDec::catch_panic_pad_function(
                    parent,
                    || false,
                    |dec, element| dec.src_event(pad, element, event),
                )
            })
            .build();

        Self {
            srcpad,
            sinkpad,
            state: Mutex::new(State::default()),
        }
    }
}

impl ObjectImpl for HvifDec {
    fn constructed(&self, obj: &Self::Type) {
        self.parent_constructed(obj);

        obj.add_pad(&self.sinkpad).unwrap();
        obj.add_pad(&self.srcpad).unwrap();
    }
}

impl GstObjectImpl for HvifDec {}

impl ElementImpl for HvifDec {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "HVIF decoder",
                "Codec/Decoder/Video",
                "Decodes vectorial HVIF images",
                "Emmanuel Gil Peyrot <linkmauve@linkmauve.fr>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let caps = gst::Caps::builder("image/x-hvif").build();

            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            let caps = gst::Caps::builder("video/x-raw")
                .field("format", &"BGRA")
                .build();

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
        element: &Self::Type,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst_trace!(CAT, obj: element, "Changing state {:?}", transition);

        if transition == gst::StateChange::PausedToReady {
            *self.state.lock().unwrap() = State::default();
        }

        self.parent_change_state(element, transition)
    }
}
