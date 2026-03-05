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
use image::flat::{NormalForm, SampleLayout};
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

                for f in utils::Format::all_values() {
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

enum SliceContainer {
    Reference(gst::MappedBuffer<gst::buffer::Readable>),
    Owned(Vec<u8>),
}

impl SliceContainer {
    /// Original by Markus Ebner on gifenc
    fn convert_to_tightly_packed_framebuffer(
        input_map: &gst::MappedBuffer<gst::buffer::Readable>,
        video_info: &gst_video::VideoInfo,
    ) -> Vec<u8> {
        assert_eq!(video_info.n_planes(), 1);
        let line_size = (video_info.width() * video_info.n_components()) as usize;
        let line_stride = video_info.comp_stride(0) as usize;
        let mut raw_frame: Vec<u8> = Vec::with_capacity(line_size * video_info.height() as usize);

        input_map
            .chunks_exact(line_stride)
            .map(|padded_line| &padded_line[..line_size])
            .for_each(|line| raw_frame.extend_from_slice(line));

        raw_frame
    }

    /// Make a new struct that will automatically repackage the given buffer
    /// if it's not tightly packed for image-rs.
    ///
    /// https://gstreamer.freedesktop.org/documentation/additional/design/mediatype-video-raw.html?gi-language=c#formats
    fn new(
        layout: &SampleLayout,
        video_info: &gst_video::VideoInfo,
        input_map: gst::MappedBuffer<gst::buffer::Readable>,
    ) -> Self {
        if layout.is_normal(NormalForm::RowMajorPacked) {
            Self::Reference(input_map)
        } else {
            let packed_frame = Self::convert_to_tightly_packed_framebuffer(&input_map, video_info);
            Self::Owned(packed_frame)
        }
    }
}

impl AsSliceOf for SliceContainer {
    #[inline]
    fn as_slice_of<T: FromByteSlice>(&self) -> Result<&[T], Error> {
        match self {
            SliceContainer::Reference(v) => v.as_slice_of::<T>(),
            SliceContainer::Owned(v) => v.as_slice_of::<T>(),
        }
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

        let s = allowed_caps
            .structure(0)
            .ok_or(gst::loggable_error!(CAT, "Missing caps in set_format"))?;

        if s.is_empty() {
            return Err(gst::loggable_error!(
                CAT,
                "Downstream doesn't specify any format or properties"
            ));
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
        let surface_state = self
            .obj()
            .output_state()
            .ok_or(gst::FlowError::NotNegotiated)?;

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

        match video_info.format() {
            #[cfg(target_endian = "little")]
            gst_video::VideoFormat::Rgba => {
                self.render_to_image::<Rgba<u8>>(frame, video_info, format)
            }
            #[cfg(target_endian = "big")]
            gst_video::VideoFormat::Abgr => {
                self.ingest_image::<Rgba<u8>>(frame, video_info, format)
            }
            #[cfg(target_endian = "little")]
            gst_video::VideoFormat::Rgb => {
                self.render_to_image::<Rgb<u8>>(frame, video_info, format)
            }
            #[cfg(target_endian = "big")]
            gst_video::VideoFormat::Bgr => self.ingest_image::<Rgb<u8>>(frame, video_info, format),
            gst_video::VideoFormat::Gray8 => {
                self.render_to_image::<Luma<u8>>(frame, video_info, format)
            }
            #[cfg(target_endian = "little")]
            gst_video::VideoFormat::Gray16Le => {
                self.render_to_image::<Luma<u16>>(frame, video_info, format)
            }
            #[cfg(target_endian = "big")]
            gst_video::VideoFormat::Gray16Be => {
                self.ingest_image::<Luma<u16>>(frame, video_info, format)
            }
            #[cfg(target_endian = "little")]
            gst_video::VideoFormat::Rgba64Le => {
                self.render_to_image::<Rgba<u16>>(frame, video_info, format)
            }
            #[cfg(target_endian = "big")]
            gst_video::VideoFormat::Rgba64Be => {
                self.ingest_image::<Rgba<u16>>(frame, video_info, format)
            }
            _ => unimplemented!(),
        }
    }
}

impl Encoder {
    fn render_to_image<T>(
        &self,
        mut frame: gst_video::VideoCodecFrame,
        video_info: &gst_video::VideoInfo,
        format: utils::Format,
    ) -> Result<gst::FlowSuccess, gst::FlowError>
    where
        T: PixelWithColorType,
        [T::Subpixel]: EncodableLayout,
        T::Subpixel: byte_slice_cast::FromByteSlice,
    {
        let input_buffer = frame
            .input_buffer_owned()
            .expect("frame without input buffer");
        let input_map = input_buffer.into_mapped_buffer_readable().unwrap();

        let layout = SampleLayout {
            channels: video_info.n_components().try_into().unwrap(),
            channel_stride: video_info.comp_offset(1),
            width: video_info.width(),
            width_stride: video_info.comp_pstride(0).try_into().unwrap(),
            height: video_info.height(),
            height_stride: video_info.comp_stride(0).try_into().unwrap(),
        };

        let container = SliceContainer::new(&layout, video_info, input_map);

        let slice = container.as_slice_of::<T::Subpixel>().map_err(|v| {
            gst::error!(
                CAT,
                imp = self,
                "Couldn't cast buffer to the expected format: {v}"
            );
            gst::FlowError::NotSupported
        })?;

        let mut image =
            ImageBuffer::<T, _>::from_raw(video_info.width(), video_info.height(), slice)
                .ok_or(gst::FlowError::NotSupported)?;

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
