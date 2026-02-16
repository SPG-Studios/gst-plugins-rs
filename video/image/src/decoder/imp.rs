// SPDX-CopyrightText: 2026 Amyspark <amy@centricular.com>
// SPDX-License-Identifier: MPL-2.0
// Based on Mathieu Duponchelle's WebP plugin -- see video/webp/src/dec/imp.rs

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use image::Limits;
use image::{DynamicImage, GenericImageView, ImageFormat, ImageReader};
#[cfg(any(feature = "gif", feature = "webp"))]
use image::{AnimationDecoder, Frame, ImageDecoder};
#[cfg(any(feature = "gif", feature = "webp"))]
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
struct Settings {
    max_size: u64,
    max_alloc: u64,
}

#[derive(Default)]
struct State {
    buffers: Vec<gst::Buffer>,
    total_size: usize,
}

struct DynamicImageWrapper(DynamicImage);

impl AsRef<[u8]> for DynamicImageWrapper {
    fn as_ref(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

#[cfg(any(feature = "gif", feature = "webp"))]
struct AnimatedImageWrapper(Frame);

#[cfg(any(feature = "gif", feature = "webp"))]
impl AsRef<[u8]> for AnimatedImageWrapper {
    fn as_ref(&self) -> &[u8] {
        self.0.buffer()
    }
}

pub struct ImageRsDecoder {
    srcpad: gst::Pad,
    sinkpad: gst::Pad,
    settings: Mutex<Settings>,
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
        let settings = self.settings.lock().unwrap();

        if settings.max_size == 0 || (state.total_size + buffer.size()) as u64 <= settings.max_size {
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

    fn render_single_frame(&self, image: DynamicImage) -> Result<(), gst::ErrorMessage> {
        let wh = image.dimensions();

        let fmt = match image.color() {
            image::ColorType::Rgb8 => gst_video::VideoFormat::Rgb,
            image::ColorType::Rgba8 => gst_video::VideoFormat::Rgba,
            image::ColorType::L8 => gst_video::VideoFormat::Gray8,
            #[cfg(target_endian = "little")]
            image::ColorType::L16 => gst_video::VideoFormat::Gray16Le,
            #[cfg(target_endian = "big")]
            image::ColorType::L16 => gst_video::VideoFormat::Gray16Be,
            #[cfg(target_endian = "little")]
            image::ColorType::Rgba16 => gst_video::VideoFormat::Rgba64Le,
            #[cfg(target_endian = "big")]
            image::ColorType::Rgba16 => gst_video::VideoFormat::Rgba64Be,
            v => {
                gst::element_warning!(
                    self.obj(),
                    gst::StreamError::Decode,
                    ["Unknown format {:?}, converting to RGBA", v]
                );
                gst_video::VideoFormat::Rgba
            }
        };

        let caps = gst_video::VideoInfo::builder(fmt, wh.0, wh.1)
            .fps((0, 1))
            .build()
            .unwrap()
            .to_caps()
            .unwrap();

        let segment = gst::FormattedSegment::<gst::ClockTime>::new();

        let _ = self.srcpad.push_event(gst::event::Caps::new(&caps));
        let _ = self.srcpad.push_event(gst::event::Segment::new(&segment));

        let mut out_buf = if fmt == gst_video::VideoFormat::Rgba {
            let image_rgba8 = image.to_rgba8();
            gst::Buffer::from_slice(DynamicImageWrapper(DynamicImage::from(image_rgba8)))
        } else {
            gst::Buffer::from_slice(DynamicImageWrapper(image))
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

    #[cfg(any(feature = "gif", feature = "webp"))]
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
                let mut limits = Limits::default();
                limits.max_alloc = Some(*self.limit.lock().unwrap());
                let mut decoder = GifDecoder::new(reader.into_inner()).map_err(|v| {
                    gst::error_msg!(
                        gst::StreamError::Decode,
                        ["Failed decoding GIF container: {}", v]
                    )
                })?;
                decoder.set_limits(limits).map_err(|v| {
                    gst::error_msg!(
                        gst::StreamError::Decode,
                        ["Failed setting memory limits: {}", v]
                    )
                })?;

                self.render_many_frames(decoder)?;
            }
            #[cfg(feature = "webp")]
            Some(ImageFormat::WebP) => {
                let mut limits = Limits::default();
                limits.max_alloc = Some(*self.limit.lock().unwrap());
                let mut decoder = WebPDecoder::new(reader.into_inner()).map_err(|v| {
                    gst::error_msg!(
                        gst::StreamError::Decode,
                        ["Failed decoding AVIF container: {}", v]
                    )
                })?;
                decoder.set_limits(limits).map_err(|v| {
                    gst::error_msg!(
                        gst::StreamError::Decode,
                        ["Failed setting memory limits: {}", v]
                    )
                })?;

                self.render_many_frames(decoder)?;
            }
            Some(_) => {
                {
                    let settings = self.settings.lock().unwrap();
                    if settings.max_alloc != 0 {
                        let mut limits = Limits::default();
                        limits.max_alloc = Some(settings.max_alloc);
                        reader.limits(limits);
                    }
                }
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
    const NAME: &'static str = "GstImageRsDecoder";
    type Type = super::Decoder;
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
            settings: Mutex::new(Settings::default()),
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
                    .build()
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "max-alloc-bytes" => {
                let mut settings = self.settings.lock().unwrap();
                settings.max_alloc = value.get::<u64>().expect("type checked upstream");
            },
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
            },
            "max-size-bytes" => {
                let settings = self.settings.lock().unwrap();
                settings.max_size.to_value()
            },
            name => panic!("No getter for {name}"),
        }
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
                // Still image -- APNG et al. are disabled
                .field("framerate", gst::Fraction::new(0, 1))
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
