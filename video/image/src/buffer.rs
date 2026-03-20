use byte_slice_cast::*;
use image::{DynamicImage, ImageBuffer, PixelWithColorType};

use std::ops::Deref;

pub(crate) enum Wrapper {
    Image(DynamicImage),
    Vec(Vec<u8>),
}

impl AsRef<[u8]> for Wrapper {
    fn as_ref(&self) -> &[u8] {
        match self {
            Wrapper::Image(v) => v.as_bytes(),
            Wrapper::Vec(v) => v.as_slice(),
        }
    }
}

#[track_caller]
#[inline(never)]
fn convert_strides<P, C>(image: &ImageBuffer<P, C>) -> Option<Vec<u8>>
where
    P: PixelWithColorType,
    C: Deref<Target = [P::Subpixel]> + AsByteSlice<P::Subpixel>,
{
    let layout = image.sample_layout();
    let row_stride = layout
        .height_stride
        .strict_mul(std::mem::size_of::<P::Subpixel>());

    if !row_stride.is_multiple_of(4) {
        let fixed_row_stride = row_stride.next_multiple_of(4);
        assert!(fixed_row_stride > row_stride);
        let padding = fixed_row_stride - row_stride;
        let new_len = fixed_row_stride.strict_mul(layout.height as usize);
        let mut buffer = Vec::<u8>::with_capacity(new_len);
        for row in image.as_raw().as_byte_slice().chunks_exact(row_stride) {
            buffer.extend(row);
            buffer.resize(buffer.len() + padding, 0);
        }
        assert_eq!(buffer.len(), new_len);
        Some(buffer)
    } else {
        None
    }
}

pub(crate) trait GStreamerImage {
    fn wrap_for_gstreamer(self) -> Wrapper;
}

impl GStreamerImage for DynamicImage {
    /// TODO: if VideoMeta is supported, just reuse the image
    /// and broadcast the strides
    fn wrap_for_gstreamer(self) -> Wrapper {
        use DynamicImage::*;
        use Wrapper::*;
        match self {
            ImageRgb8(ref v) => match convert_strides(v) {
                Some(v) => Vec(v),
                None => Image(self),
            },
            ImageRgba8(ref v) => match convert_strides(v) {
                Some(v) => Vec(v),
                None => Image(self),
            },
            ImageLuma8(ref v) => match convert_strides(v) {
                Some(v) => Vec(v),
                None => Image(self),
            },
            #[cfg(target_endian = "little")]
            ImageLuma16(ref v) => match convert_strides(v) {
                Some(v) => Vec(v),
                None => Image(self),
            },
            #[cfg(target_endian = "big")]
            ImageLuma16(ref v) => match convert_strides(v) {
                Some(v) => Vec(v),
                None => Image(self),
            },
            #[cfg(target_endian = "little")]
            ImageRgba16(ref v) => match convert_strides(v) {
                Some(v) => Vec(v),
                None => Image(self),
            },
            #[cfg(target_endian = "big")]
            ImageRgba16(ref v) => match convert_strides(v) {
                Some(v) => Vec(v),
                None => Image(self),
            },
            _ => unreachable!(),
        }
    }
}
