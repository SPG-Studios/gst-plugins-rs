// SPDX-CopyrightText: 2026 Amyspark <amy@centricular.com>
// SPDX-License-Identifier: MPL-2.0
// Based on Mathieu Duponchelle's WebP plugin -- see video/webp/src/dec/imp.rs

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_video::VideoColorimetry;

use image::codecs::gif::GifDecoder;
use image::codecs::png::PngDecoder;
use image::codecs::webp::WebPDecoder;
use image::{AnimationDecoder, Frames, ImageDecoder, ImageFormat, ImageReader, Limits};

use std::io::Cursor;
use std::sync::LazyLock;
use std::sync::Mutex;

use crate::cicp::ImageCicp;
use crate::format::Format;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "animatedimagersdec",
        gst::DebugColorFlags::empty(),
        Some("image-rs decoder for animated formats"),
    )
});

#[derive(Default)]
struct Settings {
    max_size: u64,
    max_alloc: u64,
}

#[derive(Default)]
struct State {
    buffers: Vec<gst::Buffer>,
    total_size: usize,
    format_from_caps: Option<Format>,
    in_par: Option<gst::Fraction>,
}

pub struct Decoder {
    srcpad: gst::Pad,
    sinkpad: gst::Pad,
    state: Mutex<State>,
    settings: Mutex<Settings>,
}

impl Decoder {
    fn sink_chain(
        &self,
        pad: &gst::Pad,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst::log!(CAT, obj = pad, "Handling buffer {buffer:?}");

        let mut state = self.state.lock().unwrap();
        let settings = self.settings.lock().unwrap();

        if settings.max_size == 0 || (state.total_size + buffer.size()) as u64 <= settings.max_size
        {
            gst::log!(CAT, imp = self, "Writing buffer size {}", buffer.size());
            state.total_size += buffer.size();
            state.buffers.push(buffer);

            Ok(gst::FlowSuccess::Ok)
        } else {
            gst::error!(
                CAT,
                obj = pad,
                "Exhausted memory limit of {:?} bytes",
                settings.max_size
            );
            Err(gst::FlowError::Error)
        }
    }

    fn render_many_frames<'a>(
        &self,
        frames: Frames<'a>,
        wh: (u32, u32),
        pixel_aspect_ratio: Option<gst::Fraction>,
    ) -> Result<(), gst::ErrorMessage> {
        let mut prev_timestamp = gst::ClockTime::ZERO;

        let mut frame_list = frames.peekable();

        let color_info = match frame_list.peek() {
            Some(v) => match v {
                Ok(frame) => {
                    match VideoColorimetry::try_from(ImageCicp::from(frame.buffer().color_space()))
                    {
                        Ok(v) => Some(v),
                        Err(v) => {
                            gst::warning!(CAT, imp = self, "Failed converting to VideoInfo: {v}");
                            None
                        }
                    }
                }
                Err(v) => {
                    gst::warning!(
                        CAT,
                        imp = self,
                        "Failed retrieving color information from first frame: {v}"
                    );
                    None
                }
            },
            None => None,
        };

        let caps = gst_video::VideoInfo::builder(gst_video::VideoFormat::Rgba, wh.0, wh.1)
            .par_if_some(pixel_aspect_ratio)
            .colorimetry_if_some(color_info.as_ref())
            .build()
            .unwrap()
            .to_caps()
            .unwrap();

        let segment = gst::FormattedSegment::<gst::ClockTime>::new();

        let _ = self.srcpad.push_event(gst::event::Caps::new(&caps));
        let _ = self.srcpad.push_event(gst::event::Segment::new(&segment));

        for frame in frame_list {
            let frame = frame.map_err(|v| {
                gst::error_msg!(
                    gst::StreamError::Decode,
                    ["Failed to get next frame: {}", v]
                )
            })?;

            let delay: gst::ClockTime = std::time::Duration::from(frame.delay())
                .try_into()
                .map_err(|v| {
                    gst::error_msg!(gst::StreamError::Decode, ["Invalid frame duration: {}", v])
                })?;

            // We can consume the frame here because AnimatedEncoder
            // supports only RGBA output, and image-rs's ImageBuffer
            // class only accepts tightly packed buffers.
            let mut out_buf = gst::Buffer::from_slice(frame.into_buffer().into_vec());
            {
                let out_buf_mut = out_buf.get_mut().unwrap();
                out_buf_mut.set_pts(prev_timestamp);
                out_buf_mut.set_duration(delay);
            }

            prev_timestamp += delay;

            match self.srcpad.push(out_buf) {
                Ok(_) => (),
                Err(gst::FlowError::Flushing) | Err(gst::FlowError::Eos) => break,
                Err(flow) => {
                    return Err(gst::error_msg!(
                        gst::StreamError::Failed,
                        ["Failed to push buffers: {:?}", flow]
                    ));
                }
            }
        }

        Ok(())
    }

    fn set_format_from_caps(&self, event_caps: &gst::event::Caps) -> Result<(), gst::ErrorMessage> {
        let mime = event_caps.caps().structure(0).unwrap();
        let mut state = self.state.lock().unwrap();
        state.format_from_caps = Some(mime.name().as_str().try_into()?);
        state.in_par = mime.get::<gst::Fraction>("pixel-aspect-ratio").ok();

        Ok(())
    }

    fn decode(&self) -> Result<(), gst::ErrorMessage> {
        let mut state = self.state.lock().unwrap();

        let format = state.format_from_caps;

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

        let par = state.in_par;

        drop(state);

        let mut reader = ImageReader::new(Cursor::new(buf));

        {
            let settings = self.settings.lock().unwrap();

            if settings.max_alloc != 0 {
                let mut limits = Limits::default();
                limits.max_alloc = Some(settings.max_alloc);
                reader.limits(limits);
            }
        }

        if let Some(v) = format {
            reader.set_format(v.try_into().unwrap());
        } else {
            reader = reader.with_guessed_format().map_err(|v| {
                gst::error_msg!(
                    gst::StreamError::Decode,
                    ["Failed detecting container: {v}"]
                )
            })?;
        }

        match reader.format() {
            Some(ImageFormat::Gif) => {
                let decoder = GifDecoder::new(reader.into_inner()).map_err(|v| {
                    gst::error_msg!(
                        gst::StreamError::Decode,
                        ["Failed decoding GIF container: {v}"]
                    )
                })?;

                let wh = decoder.dimensions();

                let frames = decoder.into_frames();

                self.render_many_frames(frames, wh, par)
            }
            Some(ImageFormat::WebP) => {
                let decoder = WebPDecoder::new(reader.into_inner()).map_err(|v| {
                    gst::error_msg!(
                        gst::StreamError::Decode,
                        ["Failed decoding WebP container: {v}"]
                    )
                })?;

                let wh = decoder.dimensions();

                let frames = decoder.into_frames();

                self.render_many_frames(frames, wh, par)
            }
            Some(ImageFormat::Png) => {
                let decoder = PngDecoder::new(reader.into_inner()).map_err(|v| {
                    gst::error_msg!(
                        gst::StreamError::Decode,
                        ["Failed decoding PNG container: {v}"]
                    )
                })?;

                let wh = decoder.dimensions();

                let apng_decoder = decoder.apng().map_err(|v| {
                    gst::error_msg!(
                        gst::StreamError::Decode,
                        ["Failed decoding animated PNG container: {v}"]
                    )
                })?;

                let frames = apng_decoder.into_frames();

                self.render_many_frames(frames, wh, par)
            }
            // Some(v) => image-rs default format
            // None => either failure to detect or an image-extras format
            v => Err(gst::error_msg!(
                gst::StreamError::Decode,
                ["Unknown or non animated format: {v:?}"]
            )),
        }
    }

    fn sink_event(&self, pad: &gst::Pad, event: gst::Event) -> bool {
        use gst::EventView;

        gst::log!(CAT, obj = pad, "Handling event {:?}", event);
        match event.view() {
            EventView::Caps(v) => {
                if let Err(err) = self.set_format_from_caps(v) {
                    self.post_error_message(err);
                }
                gst::Pad::event_default(pad, Some(&*self.obj()), event)
            }
            EventView::FlushStop(..) => {
                let mut state = self.state.lock().unwrap();
                *state = State::default();
                gst::Pad::event_default(pad, Some(&*self.obj()), event)
            }
            EventView::Eos(..) | EventView::SegmentDone(..) => {
                if let Err(err) = self.decode() {
                    self.post_error_message(err);
                }
                gst::Pad::event_default(pad, Some(&*self.obj()), event)
            }
            EventView::Segment(..) => true,
            _ => gst::Pad::event_default(pad, Some(&*self.obj()), event),
        }
    }

    fn src_event(&self, pad: &gst::Pad, event: gst::Event) -> bool {
        use gst::EventView;

        gst::log!(CAT, obj = pad, "Handling event {:?}", event);
        match event.view() {
            EventView::Seek(..) => false,
            _ => gst::Pad::event_default(pad, Some(&*self.obj()), event),
        }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for Decoder {
    const NAME: &'static str = "GstImageRsAnimDecoder";
    type Type = super::Decoder;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.pad_template("sink").unwrap();
        let sinkpad = gst::Pad::builder_from_template(&templ)
            .chain_function(|pad, parent, buffer| {
                Decoder::catch_panic_pad_function(
                    parent,
                    || Err(gst::FlowError::Error),
                    |dec| dec.sink_chain(pad, buffer),
                )
            })
            .event_function(|pad, parent, event| {
                Decoder::catch_panic_pad_function(
                    parent,
                    || false,
                    |dec| dec.sink_event(pad, event),
                )
            })
            .build();

        let templ = klass.pad_template("src").unwrap();
        let srcpad = gst::Pad::builder_from_template(&templ)
            .event_function(|pad, parent, event| {
                Decoder::catch_panic_pad_function(parent, || false, |dec| dec.src_event(pad, event))
            })
            .flags(gst::PadFlags::FIXED_CAPS)
            .build();

        Self {
            srcpad,
            sinkpad,
            state: Mutex::new(State::default()),
            settings: Mutex::new(Settings::default()),
        }
    }
}

impl ObjectImpl for Decoder {
    fn constructed(&self) {
        self.parent_constructed();

        let obj = self.obj();
        obj.add_pad(&self.sinkpad).unwrap();
        obj.add_pad(&self.srcpad).unwrap();
    }

    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecUInt64::builder("max-size-bytes")
                    .nick("Max. size (kB)")
                    .blurb("Max. amount of data to buffer (bytes, 0=disable)")
                    .default_value(10 * 1024 * 1024)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt64::builder("max-alloc-bytes")
                    .nick("Memory allocation limits")
                    .blurb("Max. amount of data to allocate for decoding (bytes, 0=disable)")
                    .default_value(128 * 1024 * 1024)
                    .mutable_ready()
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "max-alloc-bytes" => {
                let mut settings = self.settings.lock().unwrap();
                settings.max_alloc = value.get::<u64>().expect("type checked upstream");
            }
            "max-size-bytes" => {
                let mut settings = self.settings.lock().unwrap();
                settings.max_size = value.get::<u64>().expect("type checked upstream");
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "max-alloc-bytes" => {
                let settings = self.settings.lock().unwrap();
                settings.max_alloc.to_value()
            }
            "max-size-bytes" => {
                let settings = self.settings.lock().unwrap();
                settings.max_size.to_value()
            }
            name => panic!("No getter for {name}"),
        }
    }
}

impl GstObjectImpl for Decoder {}

impl ElementImpl for Decoder {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "image-rs decoder (animated formats)",
                "Codec/Decoder/Video",
                "Decodes animated image formats",
                "Amyspark <amy@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let mut caps = gst::Caps::new_empty();
            {
                let caps = caps.get_mut().unwrap();

                for f in Format::all_animated_formats() {
                    for v in f.to_mimetypes() {
                        caps.append(gst::Caps::new_empty_simple(v));
                    }
                }
            }

            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            let caps = gst_video::VideoCapsBuilder::new()
                .format(gst_video::VideoFormat::Rgba)
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
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst::trace!(CAT, imp = self, "Changing state {:?}", transition);

        if transition == gst::StateChange::PausedToReady {
            *self.state.lock().unwrap() = State::default();
        }

        self.parent_change_state(transition)
    }
}
