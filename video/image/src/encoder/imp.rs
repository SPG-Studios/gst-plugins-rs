// SPDX-CopyrightText: 2026 Amyspark <amy@centricular.com>
// SPDX-License-Identifier: MPL-2.0
// Based on pngenc for the data flows

use gst::glib;
#[allow(unused)]
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_video::prelude::*;
use gst_video::subclass::prelude::*;

use image::{EncodableLayout, ImageBuffer, Luma, PixelWithColorType, Rgb, Rgba};

use std::io::Cursor;
use std::sync::LazyLock;
use std::sync::Mutex;

use crate::utils;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "ImageRsEncoder",
        gst::DebugColorFlags::empty(),
        Some("image-rs encoder"),
    )
});

#[derive(Debug, Clone, Copy)]
struct Settings {
    format: super::Format,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            format: super::Format::Tiff,
        }
    }
}

struct State {
    video_info: gst_video::VideoInfo,
}

fn mimetypes() -> impl IntoIterator<Item = &'static str> {
    [
        #[cfg(feature = "avif")]
        "image/avif",
        #[cfg(any(feature = "bmp", feature = "ico"))]
        "image/bmp",
        #[cfg(feature = "exr")]
        "image/exr",
        #[cfg(feature = "ff")]
        "image/x-farbfeld",
        #[cfg(feature = "jpeg")]
        "image/jpeg",
        #[cfg(any(feature = "png", feature = "ico"))]
        "image/png",
        // https://github.com/phoboslab/qoi/issues/167
        #[cfg(feature = "qoi")]
        "image/qoi",
        #[cfg(feature = "tga")]
        "image/x-tga",
        #[cfg(feature = "tiff")]
        "image/tiff",
        // FIXME: webp
    ]
}

#[derive(Default)]
pub struct Encoder {
    state: Mutex<Option<State>>,
    settings: Mutex<Settings>,
}

#[glib::object_subclass]
impl ObjectSubclass for Encoder {
    const NAME: &'static str = "GstImageRsEncoder";
    type Type = super::Encoder;
    type ParentType = gst_video::VideoEncoder;
}

impl ObjectImpl for Encoder {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecEnum::builder_with_default("format", super::Format::Tiff)
                    .nick("File format")
                    .blurb("Selects the container format")
                    .mutable_ready()
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "format" => {
                let mut settings = self.settings.lock().unwrap();
                settings.format = value.get().expect("type checked upstream");
            }
            _ => unreachable!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "format" => {
                let settings = self.settings.lock().unwrap();
                settings.format.to_value()
            }
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for Encoder {}

impl ElementImpl for Encoder {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "image-rs encoder",
                "Encoder/Video",
                "Encodes still images",
                "Amyspark <amy@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let sink_caps = gst_video::VideoCapsBuilder::new()
                .format_list([
                    #[cfg(target_endian = "little")]
                    gst_video::VideoFormat::Rgb,
                    #[cfg(target_endian = "big")]
                    gst_video::VideoFormat::Bgr,
                    #[cfg(target_endian = "little")]
                    gst_video::VideoFormat::Rgba,
                    #[cfg(target_endian = "big")]
                    gst_video::VideoFormat::Abgr,
                    gst_video::VideoFormat::Gray8,
                    #[cfg(target_endian = "little")]
                    gst_video::VideoFormat::Gray16Le,
                    #[cfg(target_endian = "big")]
                    gst_video::VideoFormat::Gray16Be,
                    #[cfg(target_endian = "little")]
                    gst_video::VideoFormat::Rgba64Le,
                    #[cfg(target_endian = "big")]
                    gst_video::VideoFormat::Rgba64Be,
                ])
                .build();
            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &sink_caps,
            )
            .unwrap();

            let mut src_caps = gst::Caps::new_empty();
            {
                let caps = src_caps.get_mut().unwrap();

                for mimetype in mimetypes() {
                    caps.append(gst::Caps::new_empty_simple(mimetype));
                }
            };
            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &src_caps,
            )
            .unwrap();

            vec![sink_pad_template, src_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl VideoEncoderImpl for Encoder {
    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        *self.state.lock().unwrap() = None;
        Ok(())
    }

    fn set_format(
        &self,
        state: &gst_video::VideoCodecState<'static, gst_video::video_codec_state::Readable>,
    ) -> Result<(), gst::LoggableError> {
        let video_info = state.info().clone();
        gst::debug!(CAT, imp = self, "Setting format {:?}", video_info);

        *self.state.lock().unwrap() = Some(State { video_info });

        let format: &str = self.settings.lock().unwrap().format.into();

        let instance = self.obj();
        let output_state = instance
            .set_output_state(gst::Caps::builder(format).build(), Some(state))
            .map_err(|_| gst::loggable_error!(CAT, "Failed to set output state"))?;
        instance
            .negotiate(output_state)
            .map_err(|_| gst::loggable_error!(CAT, "Failed to negotiate"))
    }

    fn handle_frame(
        &self,
        frame: gst_video::VideoCodecFrame,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let video_info = {
            let state_guard = self.state.lock().unwrap();

            let state = state_guard.as_ref().ok_or(gst::FlowError::NotNegotiated)?;

            state.video_info.clone()
        };

        let format = self.settings.lock().unwrap().format;

        gst::debug!(
            CAT,
            imp = self,
            "Sending frame {}",
            frame.system_frame_number()
        );

        let input_buffer = frame
            .input_buffer_owned()
            .expect("frame without input buffer");
        let input_map = input_buffer.into_mapped_buffer_readable().unwrap();
        match video_info.format() {
            #[cfg(target_endian = "little")]
            gst_video::VideoFormat::Rgba => {
                let image = ImageBuffer::<Rgba<u8>, _>::from_raw(
                    video_info.width(),
                    video_info.height(),
                    input_map.as_slice(),
                )
                .ok_or(gst::FlowError::NotSupported)?;

                self.render_frame(image, frame, video_info, format)
            }
            #[cfg(target_endian = "big")]
            gst_video::VideoFormat::Abgr => {
                let image = ImageBuffer::<Rgba<u8>, _>::from_raw(
                    video_info.width(),
                    video_info.height(),
                    input_map.as_slice(),
                )
                .ok_or(gst::FlowError::NotSupported)?;

                self.render_frame(image, frame, video_info, format)
            }
            #[cfg(target_endian = "little")]
            gst_video::VideoFormat::Rgb => {
                let image = ImageBuffer::<Rgb<u8>, _>::from_raw(
                    video_info.width(),
                    video_info.height(),
                    input_map.as_slice(),
                )
                .ok_or(gst::FlowError::NotSupported)?;

                self.render_frame(image, frame, video_info, format)
            }
            #[cfg(target_endian = "big")]
            gst_video::VideoFormat::Bgr => {
                let image = ImageBuffer::<Rgb<u8>, _>::from_raw(
                    video_info.width(),
                    video_info.height(),
                    input_map.as_slice(),
                )
                .ok_or(gst::FlowError::NotSupported)?;

                self.render_frame(image, frame, video_info, format)
            }
            gst_video::VideoFormat::Gray8 => {
                let image = ImageBuffer::<Luma<u8>, _>::from_raw(
                    video_info.width(),
                    video_info.height(),
                    input_map.as_slice(),
                )
                .ok_or(gst::FlowError::NotSupported)?;

                self.render_frame(image, frame, video_info, format)
            }
            #[cfg(target_endian = "little")]
            gst_video::VideoFormat::Gray16Le => {
                let input = unsafe {
                    std::slice::from_raw_parts(
                        input_map.as_ptr() as *const u16,
                        input_map.size() / 2,
                    )
                };
                let image = ImageBuffer::<Luma<u16>, _>::from_raw(
                    video_info.width(),
                    video_info.height(),
                    input,
                )
                .ok_or(gst::FlowError::NotSupported)?;

                self.render_frame(image, frame, video_info, format)
            }
            #[cfg(target_endian = "big")]
            gst_video::VideoFormat::Gray16Be => {
                let input = unsafe {
                    std::slice::from_raw_parts(
                        input_map.as_ptr() as *const u16,
                        input_map.size() / 2,
                    )
                };
                let image = ImageBuffer::<Luma<u16>, _>::from_raw(
                    video_info.width(),
                    video_info.height(),
                    input,
                )
                .ok_or(gst::FlowError::NotSupported)?;

                self.render_frame(image, frame, video_info, format)
            }
            #[cfg(target_endian = "little")]
            gst_video::VideoFormat::Rgba64Le => {
                let input = unsafe {
                    std::slice::from_raw_parts(
                        input_map.as_ptr() as *const u16,
                        input_map.size() / 2,
                    )
                };
                let image = ImageBuffer::<Rgba<u16>, _>::from_raw(
                    video_info.width(),
                    video_info.height(),
                    input,
                )
                .ok_or(gst::FlowError::NotSupported)?;

                self.render_frame(image, frame, video_info, format)
            }
            #[cfg(target_endian = "big")]
            gst_video::VideoFormat::Rgba64Be => {
                let input = unsafe {
                    std::slice::from_raw_parts(
                        input_map.as_ptr() as *const u16,
                        input_map.size() / 2,
                    )
                };
                let image = ImageBuffer::<Rgba<u16>, _>::from_raw(
                    video_info.width(),
                    video_info.height(),
                    input,
                )
                .ok_or(gst::FlowError::NotSupported)?;

                self.render_frame(image, frame, video_info, format)
            }
            _ => unimplemented!(),
        }
    }
}

impl Encoder {
    fn render_frame<P, C>(
        &self,
        mut image: ImageBuffer<P, C>,
        mut frame: gst_video::VideoCodecFrame,
        video_info: gst_video::VideoInfo,
        format: super::Format,
    ) -> Result<gst::FlowSuccess, gst::FlowError>
    where
        P: PixelWithColorType,
        [P::Subpixel]: EncodableLayout,
        C: std::ops::Deref<Target = [P::Subpixel]>,
    {
        let color_space = utils::videoinfo_to_cicp(video_info.colorimetry());

        image.set_color_space(color_space).map_err(|e| {
            gst::error!(CAT, imp = self, "Failed to write image data: {e}");
            gst::FlowError::Error
        })?;

        let buffer = Vec::with_capacity(4096);
        let mut cursor = Cursor::new(buffer);
        image.write_to(&mut cursor, format.into()).map_err(|e| {
            gst::error!(CAT, imp = self, "Failed to write image data: {e}");
            gst::FlowError::Error
        })?;

        let output_buffer = gst::Buffer::from_mut_slice(cursor.into_inner());
        // All images outputted by image-rs are whole frames
        // (see comment in pngenc, same applies)
        frame.set_flags(gst_video::VideoCodecFrameFlags::SYNC_POINT);
        frame.set_output_buffer(output_buffer);
        self.obj().finish_frame(frame)
    }
}
