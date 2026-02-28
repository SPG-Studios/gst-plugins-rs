// SPDX-CopyrightText: 2026 Amyspark <amy@centricular.com>
// SPDX-License-Identifier: MPL-2.0
// Based on Mathieu Duponchelle's WebP plugin -- see video/webp/src/dec/imp.rs

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;

use image::codecs::gif::GifDecoder;
use image::codecs::png::PngDecoder;
use image::codecs::webp::WebPDecoder;
use image::{AnimationDecoder, Frame, Frames, ImageDecoder, ImageFormat, ImageReader};
use num_rational::Ratio;

use std::io::Cursor;
use std::sync::LazyLock;
use std::sync::Mutex;

use crate::utils;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "AnimatedImageRsDecoder",
        gst::DebugColorFlags::empty(),
        Some("image-rs decoder for animated formats"),
    )
});

#[derive(Default)]
struct State {
    buffers: Vec<gst::Buffer>,
    total_size: usize,
    format_from_caps: Option<ImageFormat>,
    in_fps: (i32, i32),
    in_par: (i32, i32),
}

pub struct Decoder {
    srcpad: gst::Pad,
    sinkpad: gst::Pad,
    state: Mutex<State>,
}

fn mimetypes() -> impl IntoIterator<Item = &'static str> {
    [
        // FIXME upstream: AVIF also supports animations
        // but needs image-rs support
        // "image/avif",
        "image/gif",
        "image/x-pcx",
        "image/png",
        "image/webp",
    ]
}

struct AnimatedImageWrapper(Frame);

impl AsRef<[u8]> for AnimatedImageWrapper {
    fn as_ref(&self) -> &[u8] {
        self.0.buffer()
    }
}

impl Decoder {
    fn sink_chain(
        &self,
        pad: &gst::Pad,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst::log!(CAT, obj = pad, "Handling buffer {:?}", buffer);

        let mut state = self.state.lock().unwrap();

        state.total_size += buffer.size();
        state.buffers.push(buffer);

        Ok(gst::FlowSuccess::Ok)
    }

    fn render_many_frames<'a>(
        &self,
        frames: Frames<'a>,
        fps: (i32, i32),
        wh: (u32, u32),
        pixel_aspect_ratio: (i32, i32),
    ) -> Result<(), gst::ErrorMessage> {
        let mut prev_timestamp = gst::ClockTime::ZERO;

        let fmt = if cfg!(target_endian = "little") {
            gst_video::VideoFormat::Rgba
        } else {
            gst_video::VideoFormat::Abgr
        };

        let mut frame_list = frames.peekable();

        let color_info = match frame_list.peek() {
            Some(v) => match v {
                Ok(frame) => Some(utils::cicp_to_videoinfo(frame.buffer().color_space())),
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

        let caps = gst_video::VideoInfo::builder(fmt, wh.0, wh.1)
            .fps(fps)
            .par(pixel_aspect_ratio)
            .colorimetry_if_some(color_info.as_ref())
            .build()
            .unwrap()
            .to_caps()
            .unwrap();

        let segment = gst::FormattedSegment::<gst::ClockTime>::new();

        let _ = self.srcpad.push_event(gst::event::Caps::new(&caps));
        let _ = self.srcpad.push_event(gst::event::Segment::new(&segment));

        // into_frames already blends the previous and current frames
        // see https://github.com/image-rs/image/blob/0779d359908cf9bf04cbd1998a1a9940e368cd56/src/codecs/gif.rs#L355
        for frame in frame_list {
            let frame = frame.map_err(|v| {
                gst::error_msg!(
                    gst::StreamError::Decode,
                    ["Failed to get next frame: {}", v]
                )
            })?;

            let delay = {
                let d: Ratio<u32> = frame.delay().numer_denom_ms().into();
                (d.to_integer() as u64).mseconds()
            };

            // AnimatedEncoder doesn't support anything other than RGBA
            let mut out_buf = gst::Buffer::from_slice(AnimatedImageWrapper(frame));
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

    fn set_format_from_caps(&self, caps: &gst::event::Caps) -> Result<(), gst::ErrorMessage> {
        match caps.structure() {
            Some(mime) => {
                let mut state = self.state.lock().unwrap();
                match mime.name().as_str() {
                    "image/gif" => state.format_from_caps = Some(ImageFormat::Gif),

                    "image/webp" => state.format_from_caps = Some(ImageFormat::WebP),

                    "image/png" => state.format_from_caps = Some(ImageFormat::Png),

                    v => {
                        return Err(gst::error_msg!(
                            gst::StreamError::CodecNotFound,
                            ["Unknown mimetype {v}"]
                        ));
                    }
                };
                state.in_fps = match mime.get::<gst::Fraction>("framerate") {
                    Ok(framerate) => {
                        gst::debug!(
                            CAT,
                            imp = self,
                            "got framerate of {}/{} fps",
                            framerate.numer(),
                            framerate.denom()
                        );
                        framerate.into()
                    }
                    Err(v) => {
                        gst::debug!(
                            CAT,
                            imp = self,
                            "no framerate, assuming single image: {v:?}"
                        );
                        (0, 1)
                    }
                };
                state.in_par = match mime.get::<gst::Fraction>("pixel-aspect-ratio") {
                    Ok(v) => v.into(),
                    Err(v) => {
                        gst::debug!(CAT, imp = self, "no pixel aspect ratio found: {v:?}");
                        (1, 1)
                    }
                };
            }
            None => {
                gst::warning!(
                    CAT,
                    imp = self,
                    "No mimetype or framerate available from caps"
                );
            }
        };

        Ok(())
    }

    fn decode(&self) -> Result<(), gst::ErrorMessage> {
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

        let fps = state.in_fps;
        let par = state.in_par;

        drop(state);

        let reader = ImageReader::new(Cursor::new(buf));

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

                self.render_many_frames(frames, fps, wh, par)
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

                self.render_many_frames(frames, fps, wh, par)
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

                self.render_many_frames(frames, fps, wh, par)
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
            EventView::Eos(..) => {
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
    const NAME: &'static str = "GstRsDecoder";
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
            .build();

        Self {
            srcpad,
            sinkpad,
            state: Mutex::new(State::default()),
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
}

impl GstObjectImpl for Decoder {}

impl ElementImpl for Decoder {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "image-rs decoder (animated formats)",
                "Codec/Decoder/Video",
                "Decodes potentially animated images",
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

                for mimetype in mimetypes() {
                    caps.append(gst::Caps::new_empty_simple(mimetype));
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
