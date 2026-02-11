// SPDX-CopyrightText: 2026 Amyspark <amy@centricular.com>
// SPDX-License-Identifier: MPL-2.0
// Based on Mathieu Duponchelle's WebP plugin -- see video/webp/src/dec/imp.rs

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use image::{
    AnimationDecoder, DynamicImage, Frame, GenericImageView, ImageDecoder, ImageFormat, ImageReader,
};
use num_rational::Ratio;

#[cfg(feature = "gif")]
use image::codecs::gif::GifDecoder;
#[cfg(feature = "webp")]
use image::codecs::webp::WebPDecoder;

use std::io::Cursor;
use std::sync::{LazyLock, Mutex};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "ImageRsDecoder",
        gst::DebugColorFlags::empty(),
        Some("image-rs decoder"),
    )
});

#[derive(Default)]
struct State {
    buffers: Vec<gst::Buffer>,
    total_size: usize,
}

struct Wrapper(DynamicImage);

impl AsRef<[u8]> for Wrapper {
    fn as_ref(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

struct Wrapper2(Frame);

impl AsRef<[u8]> for Wrapper2 {
    fn as_ref(&self) -> &[u8] {
        self.0.buffer()
    }
}

pub struct ImageRsDecoder {
    srcpad: gst::Pad,
    sinkpad: gst::Pad,
    state: Mutex<State>,
}

impl ImageRsDecoder {
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

    fn render_single_frame(&self, image: DynamicImage) -> Result<(), gst::ErrorMessage> {
        let wh = image.dimensions();

        let caps = gst_video::VideoInfo::builder(gst_video::VideoFormat::Rgba, wh.0, wh.1)
            .fps((0, 1))
            .build()
            .unwrap()
            .to_caps()
            .unwrap();

        let segment = gst::FormattedSegment::<gst::ClockTime>::new();

        let _ = self.srcpad.push_event(gst::event::Caps::new(&caps));
        let _ = self.srcpad.push_event(gst::event::Segment::new(&segment));

        let mut out_buf = if image.color() == image::ColorType::Rgba8 {
            gst::Buffer::from_slice(Wrapper(image))
        } else {
            let image_rgba8 = image.to_rgba8();
            gst::Buffer::from_slice(Wrapper(DynamicImage::from(image_rgba8)))
        };
        {
            let out_buf_mut = out_buf.get_mut().unwrap();
            out_buf_mut.set_pts(gst::ClockTime::ZERO);
            out_buf_mut.set_duration(gst::ClockTime::MAX);
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

    fn render_many_frames<'a>(
        &self,
        decoder: impl AnimationDecoder<'a> + ImageDecoder,
    ) -> Result<(), gst::ErrorMessage> {
        let mut prev_timestamp = gst::ClockTime::ZERO;
        let wh = decoder.dimensions();

        let caps = gst_video::VideoInfo::builder(gst_video::VideoFormat::Rgba, wh.0, wh.1)
            .fps((0, 1))
            .build()
            .unwrap()
            .to_caps()
            .unwrap();

        let segment = gst::FormattedSegment::<gst::ClockTime>::new();

        let _ = self.srcpad.push_event(gst::event::Caps::new(&caps));
        let _ = self.srcpad.push_event(gst::event::Segment::new(&segment));

        // into_frames already blends the previous and current frames
        // see https://github.com/image-rs/image/blob/0779d359908cf9bf04cbd1998a1a9940e368cd56/src/codecs/gif.rs#L355
        for frame in decoder.into_frames() {
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
            let mut out_buf = gst::Buffer::from_slice(Wrapper2(frame));
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

    fn decode(&self, pad: &gst::Pad) -> Result<(), gst::ErrorMessage> {
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

        let cursor = Cursor::new(buf);
        let mut reader = ImageReader::new(cursor);

        reader = match pad.current_caps() {
            Some(caps) => match caps.structure(0) {
                Some(mime) => {
                    match mime.name().as_str() {
                        #[cfg(feature = "avif")]
                        "image/avif" => reader.set_format(ImageFormat::Avif),

                        // The ICO format support enables PNG and BMP as transitive deps
                        #[cfg(any(feature = "bmp", feature = "ico"))]
                        "image/bmp" | "image/x-MS-bmp" => reader.set_format(ImageFormat::Bmp),

                        #[cfg(feature = "dds")]
                        "image/vnd-ms.dds" | "image/x-direct-draw-surface" => {
                            reader.set_format(ImageFormat::Dds)
                        }

                        #[cfg(feature = "exr")]
                        "image/x-exr" => reader.set_format(ImageFormat::OpenExr),

                        #[cfg(feature = "ff")]
                        "image/x-farbfeld" => reader.set_format(ImageFormat::Farbfeld),

                        #[cfg(feature = "gif")]
                        "image/gif" => reader.set_format(ImageFormat::Gif),

                        #[cfg(feature = "hdr")]
                        "image/vnd.radiance" => reader.set_format(ImageFormat::Hdr),

                        #[cfg(feature = "ico")]
                        "image/x-icon" => reader.set_format(ImageFormat::Ico),

                        #[cfg(feature = "jpeg")]
                        "image/jpeg" => reader.set_format(ImageFormat::Jpeg),

                        #[cfg(any(feature = "png", feature = "ico"))]
                        "image/png" => reader.set_format(ImageFormat::Png),

                        #[cfg(feature = "pnm")]
                        "image/x-portable-anymap"
                        | "image/x-portable-bitmap"
                        | "image/x-portable-graymap"
                        | "image/x-portable-pixmap" => reader.set_format(ImageFormat::Pnm),

                        #[cfg(feature = "qoi")]
                        "image/qoi" => reader.set_format(ImageFormat::Bmp),

                        #[cfg(feature = "tga")]
                        "image/x-targa" | "image/x-tga" => reader.set_format(ImageFormat::Tga),

                        #[cfg(feature = "tiff")]
                        "image/tiff" => reader.set_format(ImageFormat::Tiff),

                        #[cfg(feature = "webp")]
                        "image/webp" => reader.set_format(ImageFormat::WebP),

                        v => gst::element_warning!(
                            self.obj(),
                            gst::StreamError::CodecNotFound,
                            ["Unknown mimetype {}", v]
                        ),
                    };

                    Ok(reader)
                }
                None => reader.with_guessed_format().map_err(|v| {
                    gst::error_msg!(
                        gst::StreamError::Decode,
                        [
                            "No mimetype available from caps, failed guessing format: {}",
                            v
                        ]
                    )
                }),
            },
            None => reader.with_guessed_format().map_err(|v| {
                gst::error_msg!(
                    gst::StreamError::Decode,
                    ["No caps available, failed guessing format: {}", v]
                )
            }),
        }?;

        match reader.format() {
            #[cfg(feature = "gif")]
            Some(ImageFormat::Gif) => {
                let decoder = GifDecoder::new(reader.into_inner()).map_err(|v| {
                    gst::error_msg!(
                        gst::StreamError::Decode,
                        ["Failed decoding GIF container: {}", v]
                    )
                })?;

                self.render_many_frames(decoder)?;
            }
            #[cfg(feature = "webp")]
            Some(ImageFormat::WebP) => {
                let decoder = WebPDecoder::new(reader.into_inner()).map_err(|v| {
                    gst::error_msg!(
                        gst::StreamError::Decode,
                        ["Failed decoding AVIF container: {}", v]
                    )
                })?;

                self.render_many_frames(decoder)?;
            }
            Some(_) => {
                let image = reader.decode().map_err(|v| {
                    gst::error_msg!(
                        gst::StreamError::Decode,
                        ["Failed decoding still image: {}", v]
                    )
                })?;
                self.render_single_frame(image)?;
            }
            None => {
                gst::error_msg!(
                    gst::StreamError::Decode,
                    ["Failed reading for format detection"]
                );
            }
        }

        Ok(())
    }

    fn sink_event(&self, pad: &gst::Pad, event: gst::Event) -> bool {
        use gst::EventView;

        gst::log!(CAT, obj = pad, "Handling event {:?}", event);
        match event.view() {
            EventView::FlushStop(..) => {
                let mut state = self.state.lock().unwrap();
                *state = State::default();
                gst::Pad::event_default(pad, Some(&*self.obj()), event)
            }
            EventView::Eos(..) => {
                if let Err(err) = self.decode(pad) {
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
impl ObjectSubclass for ImageRsDecoder {
    const NAME: &'static str = "GstRsImageDecoder";
    type Type = super::ImageRsDecoder;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.pad_template("sink").unwrap();
        let sinkpad = gst::Pad::builder_from_template(&templ)
            .chain_function(|pad, parent, buffer| {
                ImageRsDecoder::catch_panic_pad_function(
                    parent,
                    || Err(gst::FlowError::Error),
                    |dec| dec.sink_chain(pad, buffer),
                )
            })
            .event_function(|pad, parent, event| {
                ImageRsDecoder::catch_panic_pad_function(
                    parent,
                    || false,
                    |dec| dec.sink_event(pad, event),
                )
            })
            .build();

        let templ = klass.pad_template("src").unwrap();
        let srcpad = gst::Pad::builder_from_template(&templ)
            .event_function(|pad, parent, event| {
                ImageRsDecoder::catch_panic_pad_function(
                    parent,
                    || false,
                    |dec| dec.src_event(pad, event),
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

impl ObjectImpl for ImageRsDecoder {
    fn constructed(&self) {
        self.parent_constructed();

        let obj = self.obj();
        obj.add_pad(&self.sinkpad).unwrap();
        obj.add_pad(&self.srcpad).unwrap();
    }
}

impl GstObjectImpl for ImageRsDecoder {}

impl ElementImpl for ImageRsDecoder {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "image-rs decoder",
                "Codec/Decoder/Video",
                "Decodes potentially animated images",
                "Amyspark <amy@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let mimetypes = vec![
                // FIXME upstream: AVIF also supports animations
                // but needs image-rs support
                #[cfg(feature = "avif")]
                "image/avif",
                #[cfg(any(feature = "bmp", feature = "ico"))]
                "image/bmp",
                #[cfg(any(feature = "bmp", feature = "ico"))]
                "image/x-MS-bmp",
                #[cfg(feature = "dds")]
                "image/vnd-ms.dds",
                #[cfg(feature = "dds")]
                "image/x-direct-draw-surface",
                #[cfg(feature = "exr")]
                "image/exr",
                #[cfg(feature = "ff")]
                "image/x-farbfeld",
                #[cfg(feature = "gif")]
                "image/gif",
                #[cfg(feature = "ico")]
                "image/x-icon",
                #[cfg(feature = "jpeg")]
                // FIXME upstream: doesn't support MJPEG
                "image/jpeg",
                #[cfg(any(feature = "png", feature = "ico"))]
                "image/png",
                #[cfg(feature = "pnm")]
                "image/x-portable-anymap",
                #[cfg(feature = "pnm")]
                "image/x-portable-bitmap",
                #[cfg(feature = "pnm")]
                "image/x-portable-graymap",
                #[cfg(feature = "pnm")]
                "image/x-portable-pixmap",
                #[cfg(feature = "qoi")]
                "image/qoi",
                #[cfg(feature = "tga")]
                "image/x-targa",
                #[cfg(feature = "tga")]
                "image/x-tga",
                #[cfg(feature = "tiff")]
                "image/tiff",
                #[cfg(feature = "webp")]
                "image/webp",
            ];

            let mut caps = gst::Caps::new_empty();
            {
                let caps = caps.get_mut().unwrap();

                for mimetype in mimetypes {
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
