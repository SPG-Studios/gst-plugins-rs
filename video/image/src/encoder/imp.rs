// SPDX-CopyrightText: 2026 Amyspark <amy@centricular.com>
// SPDX-License-Identifier: MPL-2.0
// Based on pngenc for the data flows

use gst::glib;
#[allow(unused)]
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_video::prelude::*;
use gst_video::subclass::prelude::*;

use byte_slice_cast::*;
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

struct State {
    format: utils::Format,
}

#[derive(Default)]
pub struct Encoder {
    state: Mutex<Option<State>>,
}

#[glib::object_subclass]
impl ObjectSubclass for Encoder {
    const NAME: &'static str = "GstImageRsEncoder";
    type Type = super::Encoder;
    type ParentType = gst_video::VideoEncoder;
}

impl ObjectImpl for Encoder {}

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

                for f in super::Format::all_values() {
                    let v: &'static str = f.into();
                    caps.append(gst::Caps::new_empty_simple(v));
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
        let instance = self.obj();

        let mut allowed_caps = match instance.src_pad().allowed_caps() {
            None => instance.src_pad().pad_template_caps(),
            Some(caps) => caps,
        };

        allowed_caps.fixate();

        let s = allowed_caps.structure(0)
            .ok_or(gst::loggable_error!(CAT, "Missing caps in set_format"))?;

        if s.is_empty() {
            return Err(gst::loggable_error!(CAT, "Downstream doesn't specify any format or properties"));
        }

        let output_state = instance
            .set_output_state(gst::Caps::builder(s.name()).build(), Some(state))
            .map_err(|_| gst::loggable_error!(CAT, "Failed to set output state"))?;
        instance
            .negotiate(output_state)
            .map_err(|_| gst::loggable_error!(CAT, "Failed to negotiate"))?;

        *self.state.lock().unwrap() = Some(State {
            format: s
                .name()
                .as_str()
                .try_into()
                .map_err(|v| gst::loggable_error!(CAT, "Failed to determine format: {}", v))?,
        });

        Ok(())
    }

    fn handle_frame(
        &self,
        frame: gst_video::VideoCodecFrame,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let surface_state = self.obj().output_state().ok_or(gst::FlowError::NotNegotiated)?;

        let video_info = surface_state.info();

        let format = {
            let state_guard = self.state.lock().unwrap();

            let state = state_guard.as_ref().ok_or(gst::FlowError::NotNegotiated)?;

            state.format
        };

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
                let input = input_map.as_slice_of::<u16>().map_err(|v| {
                    gst::error!(
                        CAT,
                        imp = self,
                        "Couldn't cast buffer to the expected format: {v}"
                    );
                    gst::FlowError::NotSupported
                })?;
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
                let input = input_map.as_slice_of::<u16>().map_err(|v| {
                    gst::error!(
                        CAT,
                        imp = self,
                        "Couldn't cast buffer to the expected format: {v}"
                    );
                    gst::FlowError::NotSupported
                })?;
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
                let input = input_map.as_slice_of::<u16>().map_err(|v| {
                    gst::error!(
                        CAT,
                        imp = self,
                        "Couldn't cast buffer to the expected format: {v}"
                    );
                    gst::FlowError::NotSupported
                })?;
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
                let input = input_map.as_slice_of::<u16>().map_err(|v| {
                    gst::error!(
                        CAT,
                        imp = self,
                        "Couldn't cast buffer to the expected format: {v}"
                    );
                    gst::FlowError::NotSupported
                })?;
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
        video_info: &gst_video::VideoInfo,
        format: super::Format,
    ) -> Result<gst::FlowSuccess, gst::FlowError>
    where
        P: PixelWithColorType,
        [P::Subpixel]: EncodableLayout,
        C: std::ops::Deref<Target = [P::Subpixel]>,
    {
        let color_space = utils::videoinfo_to_cicp(video_info.colorimetry()).map_err(|v| {
            gst::element_error!(
                self.obj(),
                gst::StreamError::Decode,
                ["Format {video_info:?} not supported: {v}"]
            );
            gst::FlowError::NotNegotiated
        })?;

        image.set_color_space(color_space).map_err(|e| {
            gst::error!(CAT, imp = self, "Failed to set color space: {e}");
            gst::FlowError::NotNegotiated
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
