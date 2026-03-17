use gst::glib;
use gst_video::{
    VideoColorMatrix, VideoColorPrimaries, VideoColorRange, VideoColorimetry, VideoTransferFunction,
};
use image::ImageFormat;
use image::metadata::{
    Cicp, CicpColorPrimaries, CicpMatrixCoefficients, CicpTransferCharacteristics,
    CicpVideoFullRangeFlag,
};

pub(crate) trait CanCicpRgb {
    /// Implements publicly Cicp::from(self.into_rgb()) == self
    /// (which checks for the two conditions below).
    fn is_rgb(&self) -> bool;
}

impl CanCicpRgb for Cicp {
    fn is_rgb(&self) -> bool {
        self.matrix == image::metadata::CicpMatrixCoefficients::Identity
            && self.full_range == image::metadata::CicpVideoFullRangeFlag::FullRange
    }
}

pub(crate) fn cicp_to_videoinfo(cicp: Cicp) -> Result<VideoColorimetry, gst::ErrorMessage> {
    let rg = match cicp.full_range {
        CicpVideoFullRangeFlag::NarrowRange => VideoColorRange::Range16_235,
        CicpVideoFullRangeFlag::FullRange => VideoColorRange::Range0_255,
        v => {
            return Err(gst::error_msg!(
                gst::CoreError::NotImplemented,
                ["Unknown color range {v:?}"]
            ));
        }
    };

    let mx = match cicp.matrix {
        CicpMatrixCoefficients::Unspecified => VideoColorMatrix::Unknown,
        v => match VideoColorMatrix::from_iso(v as u32) {
            VideoColorMatrix::Unknown => {
                return Err(gst::error_msg!(
                    gst::CoreError::NotImplemented,
                    ["Unknown color matrix {v:?}"]
                ));
            }
            v => v,
        },
    };

    let tf = match cicp.transfer {
        CicpTransferCharacteristics::Unspecified => VideoTransferFunction::Unknown,
        v => match VideoTransferFunction::from_iso(v as u32) {
            VideoTransferFunction::Unknown => {
                return Err(gst::error_msg!(
                    gst::CoreError::NotImplemented,
                    ["Unknown transfer function {v:?}"]
                ));
            }
            v => v,
        },
    };

    // See Rec. ITU-T H.273 (V4) (07/2024) table 2, p. 5
    // and the image-rs docs
    let pr = match cicp.primaries {
        CicpColorPrimaries::Unspecified => VideoColorPrimaries::Unknown,
        v => match VideoColorPrimaries::from_iso(v as u32) {
            VideoColorPrimaries::Unknown => {
                return Err(gst::error_msg!(
                    gst::CoreError::NotImplemented,
                    ["Unknown color primaries {v:?}"]
                ));
            }
            v => v,
        },
    };

    Ok(VideoColorimetry::new(rg, mx, tf, pr))
}

pub(crate) fn videoinfo_to_cicp(color_space: VideoColorimetry) -> Result<Cicp, gst::ErrorMessage> {
    // This can NOT be done with VideoColorPrimaries::to_iso because it
    // is unsafe to convert an integer to an enum value.
    let mx = match color_space.matrix() {
        VideoColorMatrix::Unknown => CicpMatrixCoefficients::Unspecified,
        VideoColorMatrix::Rgb => CicpMatrixCoefficients::Identity,
        VideoColorMatrix::Fcc => CicpMatrixCoefficients::UsFCC,
        VideoColorMatrix::Bt709 => CicpMatrixCoefficients::Bt709,
        VideoColorMatrix::Bt601 => CicpMatrixCoefficients::Smpte170m,
        VideoColorMatrix::Smpte240m => CicpMatrixCoefficients::Smpte240m,
        VideoColorMatrix::Bt2020 => CicpMatrixCoefficients::Bt2020NonConstant,
        v => {
            return Err(gst::error_msg!(
                gst::CoreError::NotImplemented,
                ["Unknown color matrix {v:?}"]
            ));
        }
    };

    let tf = match color_space.transfer() {
        VideoTransferFunction::Unknown => CicpTransferCharacteristics::Unspecified,
        VideoTransferFunction::Gamma10 => CicpTransferCharacteristics::Linear,
        VideoTransferFunction::Gamma22 => CicpTransferCharacteristics::Bt470M,
        VideoTransferFunction::Bt709 => CicpTransferCharacteristics::Bt709,
        VideoTransferFunction::Smpte240m => CicpTransferCharacteristics::Smpte240m,
        VideoTransferFunction::Srgb => CicpTransferCharacteristics::SRgb,
        VideoTransferFunction::Gamma28 => CicpTransferCharacteristics::Bt470BG,
        VideoTransferFunction::Log100 => CicpTransferCharacteristics::Log100,
        VideoTransferFunction::Log316 => CicpTransferCharacteristics::LogSqrt,
        VideoTransferFunction::Bt202012 => CicpTransferCharacteristics::Bt2020_12bit,
        VideoTransferFunction::Bt202010 => CicpTransferCharacteristics::Bt2020_10bit,
        VideoTransferFunction::Smpte2084 => CicpTransferCharacteristics::Smpte2084,
        VideoTransferFunction::AribStdB67 => CicpTransferCharacteristics::Bt2100Hlg,
        VideoTransferFunction::Bt601 => CicpTransferCharacteristics::Bt601,
        v => {
            return Err(gst::error_msg!(
                gst::CoreError::NotImplemented,
                ["Unknown transfer function {v:?}"]
            ));
        }
    };

    let pr = match color_space.primaries() {
        VideoColorPrimaries::Bt709 => CicpColorPrimaries::SRgb,
        VideoColorPrimaries::Unknown => CicpColorPrimaries::Unspecified,
        VideoColorPrimaries::Bt470m => CicpColorPrimaries::RgbM,
        VideoColorPrimaries::Bt470bg => CicpColorPrimaries::RgbB,
        VideoColorPrimaries::Smpte170m => CicpColorPrimaries::Bt601,
        VideoColorPrimaries::Smpte240m => CicpColorPrimaries::Rgb240m,
        VideoColorPrimaries::Film => CicpColorPrimaries::GenericFilm,
        VideoColorPrimaries::Bt2020 => CicpColorPrimaries::Rgb2020,
        VideoColorPrimaries::Smptest428 => CicpColorPrimaries::Xyz,
        VideoColorPrimaries::Smpterp431 => CicpColorPrimaries::SmpteRp431,
        VideoColorPrimaries::Smpteeg432 => CicpColorPrimaries::SmpteRp432,
        VideoColorPrimaries::Ebu3213 => CicpColorPrimaries::Industry22,
        v => {
            return Err(gst::error_msg!(
                gst::CoreError::NotImplemented,
                ["Unknown color primaries {v:?}"]
            ));
        }
    };

    let rg = match color_space.range() {
        gst_video::VideoColorRange::Range0_255 => {
            image::metadata::CicpVideoFullRangeFlag::FullRange
        }
        gst_video::VideoColorRange::Range16_235 => {
            image::metadata::CicpVideoFullRangeFlag::NarrowRange
        }
        v => {
            return Err(gst::error_msg!(
                gst::CoreError::NotImplemented,
                ["Unknown color range {v:?}"]
            ));
        }
    };

    Ok(Cicp {
        full_range: rg,
        matrix: mx,
        primaries: pr,
        transfer: tf,
    })
}

#[derive(Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Clone, Copy, glib::Enum)]
#[repr(u32)]
#[enum_type(name = "GstRsImageFormat")]
pub(crate) enum Format {
    #[enum_value(name = "AV1 image file format", nick = "avif")]
    Avif,
    #[enum_value(name = "Microsoft bitmap", nick = "bmp")]
    Bmp,
    #[enum_value(name = "DirectDraw Surface", nick = "dds")]
    Dds,
    #[enum_value(name = "OpenEXR", nick = "exr")]
    Exr,
    #[enum_value(name = "The Farbfeld simple image encoding format", nick = "farbfeld")]
    Farbfeld,
    #[enum_value(name = "GIF image file format", nick = "gif")]
    Gif,
    #[enum_value(name = "Radiance HDR image file format", nick = "hdr")]
    Hdr,
    #[enum_value(name = "Microsoft icon", nick = "ico")]
    Ico,
    #[enum_value(name = "JPEG image file format", nick = "jpeg")]
    Jpeg,
    #[enum_value(name = "OpenRaster image file format", nick = "openraster")]
    OpenRaster,
    #[enum_value(name = "Nokia Over The Air bitmap", nick = "nokia")]
    Nokia,
    #[enum_value(name = "PiCture eXchange file format", nick = "pcx")]
    Pcx,
    #[enum_value(name = "Portable Network Graphics", nick = "jpeg")]
    Png,
    #[enum_value(name = "Netpbm image file format", nick = "pnm")]
    Pnm,
    #[enum_value(name = "The Quite OK Image Format", nick = "qoi")]
    Qoi,
    #[enum_value(name = "Silicon Graphics Image", nick = "sgi")]
    Sgi,
    #[enum_value(name = "Truevision Targa", nick = "tga")]
    Tga,
    #[enum_value(name = "Tagged Image File Format", nick = "tiff")]
    Tiff,
    #[enum_value(name = "Wireless Application Protocol Bitmap", nick = "wbmp")]
    Wbmp,
    #[enum_value(name = "WebP image file format", nick = "webp")]
    WebP,
    #[enum_value(name = "X Bitmap", nick = "xbm")]
    Xbm,
    #[enum_value(name = "X Pixmap", nick = "xpm")]
    Xpm,
}

impl TryFrom<Format> for ImageFormat {
    type Error = gst::ErrorMessage;

    fn try_from(value: Format) -> Result<Self, Self::Error> {
        match value {
            Format::Avif => Ok(ImageFormat::Avif),
            Format::Bmp => Ok(ImageFormat::Bmp),
            Format::Dds => Ok(ImageFormat::Dds),
            Format::Exr => Ok(ImageFormat::OpenExr),
            Format::Farbfeld => Ok(ImageFormat::Farbfeld),
            Format::Gif => Ok(ImageFormat::Gif),
            Format::Hdr => Ok(ImageFormat::Hdr),
            Format::Ico => Ok(ImageFormat::Ico),
            Format::Jpeg => Ok(ImageFormat::Jpeg),
            Format::Pnm => Ok(ImageFormat::Pnm),
            Format::Png => Ok(ImageFormat::Png),
            Format::Qoi => Ok(ImageFormat::Qoi),
            Format::Tga => Ok(ImageFormat::Tga),
            Format::Tiff => Ok(ImageFormat::Tiff),
            Format::WebP => Ok(ImageFormat::WebP),
            v => Err(gst::error_msg!(
                gst::StreamError::CodecNotFound,
                ["Unknown format {v:?}"]
            )),
        }
    }
}

impl TryFrom<ImageFormat> for Format {
    type Error = gst::ErrorMessage;

    fn try_from(value: ImageFormat) -> Result<Self, Self::Error> {
        match value {
            ImageFormat::Avif => Ok(Format::Avif),
            ImageFormat::Bmp => Ok(Format::Bmp),
            ImageFormat::Dds => Ok(Format::Dds),
            ImageFormat::OpenExr => Ok(Format::Exr),
            ImageFormat::Farbfeld => Ok(Format::Farbfeld),
            ImageFormat::Gif => Ok(Format::Gif),
            ImageFormat::Hdr => Ok(Format::Hdr),
            ImageFormat::Ico => Ok(Format::Ico),
            ImageFormat::Jpeg => Ok(Format::Jpeg),
            ImageFormat::Png => Ok(Format::Png),
            ImageFormat::Pnm => Ok(Format::Pnm),
            ImageFormat::Qoi => Ok(Format::Qoi),
            ImageFormat::Tga => Ok(Format::Tga),
            ImageFormat::Tiff => Ok(Format::Tiff),
            ImageFormat::WebP => Ok(Format::WebP),
            v => Err(gst::error_msg!(
                gst::StreamError::CodecNotFound,
                ["Unknown format {v:?}"]
            )),
        }
    }
}

impl TryFrom<&str> for Format {
    type Error = gst::ErrorMessage;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "image/x-MS-bmp" => Ok(Format::Bmp),
            "image/x-direct-draw-surface" => Ok(Format::Dds),
            "image/x-farbfeld" => Ok(Format::Farbfeld),
            "image/openraster" => Ok(Format::OpenRaster),
            "image/x-nokia-over-the-air-bitmap" => Ok(Format::Nokia),
            "image/vnd.zbrush.pcx" => Ok(Format::Pcx),
            "image/x-pcx" => Ok(Format::Pcx),
            "image/x-portable-bitmap" => Ok(Format::Pnm),
            "image/x-portable-graymap" => Ok(Format::Pnm),
            "image/x-portable-pixmap" => Ok(Format::Pnm),
            // https://github.com/phoboslab/qoi/issues/167
            "image/qoi" => Ok(Format::Qoi),
            "image/sgi" => Ok(Format::Sgi),
            "image/x-tga" => Ok(Format::Tga),
            "image/vnd.wap.wbmp" => Ok(Format::Wbmp),
            "image/x-xbitmap" | "image/x-xbm" => Ok(Format::Xbm),
            "image/x-xpixmap" => Ok(Format::Xpm),

            _ => match ImageFormat::from_mime_type(value) {
                Some(v) => Format::try_from(v),
                None => Err(gst::error_msg!(
                    gst::StreamError::CodecNotFound,
                    ["Unknown mimetype {value}"]
                )),
            },
        }
    }
}

impl Format {
    pub(crate) fn all_animated_formats() -> impl IntoIterator<Item = Format> {
        [
            // FIXME upstream: AVIF also supports animations
            // but needs image-rs support
            // #[cfg(feature = "avif")]
            // Format::Avif,
            #[cfg(feature = "gif")]
            Format::Gif,
            #[cfg(any(feature = "png", feature = "ico"))]
            Format::Png,
            #[cfg(feature = "webp")]
            Format::WebP,
        ]
    }

    /// Missing formats from gdkpixbufdec:
    /// - application/x-navi-animation
    /// - image/x-cmu-raster
    /// - image/x-sun-raster
    /// - image/svg
    /// - image/svg+xml
    pub(crate) fn all_decoding_formats() -> impl IntoIterator<Item = Format> {
        [
            // FIXME upstream: AVIF also supports animations
            // but needs image-rs support
            #[cfg(feature = "avif")]
            Format::Avif,
            #[cfg(any(feature = "bmp", feature = "ico"))]
            Format::Bmp,
            #[cfg(feature = "dds")]
            Format::Dds,
            #[cfg(feature = "exr")]
            Format::Exr,
            #[cfg(feature = "ff")]
            Format::Farbfeld,
            #[cfg(feature = "ico")]
            Format::Ico,
            #[cfg(feature = "jpeg")]
            // FIXME upstream: doesn't support MJPEG
            Format::Jpeg,
            #[cfg(feature = "ora")]
            Format::OpenRaster,
            #[cfg(feature = "otb")]
            Format::Nokia,
            #[cfg(feature = "pcx")]
            Format::Pcx,
            #[cfg(any(feature = "png", feature = "ico"))]
            Format::Png,
            #[cfg(feature = "pnm")]
            Format::Pnm,
            // https://github.com/phoboslab/qoi/issues/167
            #[cfg(feature = "qoi")]
            Format::Qoi,
            #[cfg(feature = "sgi")]
            Format::Sgi,
            #[cfg(feature = "tga")]
            Format::Tga,
            #[cfg(feature = "tiff")]
            Format::Tiff,
            #[cfg(feature = "wbmp")]
            Format::Wbmp,
            #[cfg(feature = "webp")]
            Format::WebP,
            #[cfg(feature = "xbm")]
            Format::Xbm,
            #[cfg(feature = "xpm")]
            Format::Xpm,
        ]
    }

    pub(crate) fn all_encoder_formats() -> impl IntoIterator<Item = Format> {
        [
            #[cfg(feature = "avif")]
            Format::Avif,
            #[cfg(feature = "bmp")]
            Format::Bmp,
            #[cfg(feature = "exr")]
            Format::Exr,
            #[cfg(feature = "ff")]
            Format::Farbfeld,
            #[cfg(feature = "jpeg")]
            Format::Jpeg,
            #[cfg(any(feature = "png", feature = "ico"))]
            Format::Png,
            #[cfg(feature = "qoi")]
            Format::Qoi,
            #[cfg(feature = "tga")]
            Format::Tga,
            #[cfg(feature = "tiff")]
            Format::Tiff,
        ]
    }

    pub(crate) fn to_mimetypes(self) -> impl IntoIterator<Item = &'static str> {
        match ImageFormat::try_from(self) {
            Ok(v) => match v {
                ImageFormat::Bmp => vec![v.to_mime_type(), "image/x-MS-bmp"],
                ImageFormat::Dds => vec![v.to_mime_type(), "image/x-direct-draw-surface"],
                // farbfeld's MIME type in image-rs is
                // application/octet-stream, correct it here
                ImageFormat::Farbfeld => vec!["image/x-farbfeld"],
                ImageFormat::Pnm => vec![
                    "image/x-portable-bitmap",
                    "image/x-portable-graymap",
                    "image/x-portable-pixmap",
                    v.to_mime_type(),
                ],
                ImageFormat::Qoi => vec![
                    v.to_mime_type(),
                    // https://github.com/phoboslab/qoi/issues/167
                    "image/qoi",
                ],
                ImageFormat::Tga => vec![
                    v.to_mime_type(),
                    "image/x-tga",
                ],
                _ => vec![v.to_mime_type()],
            },
            // image-extras
            Err(_) => match self {
                Format::Nokia => vec![
                    // https://snisurset.net/code/abydos/supported.html
                    "image/x-nokia-over-the-air-bitmap",
                ],
                Format::OpenRaster => vec!["image/openraster"],
                Format::Pcx => vec!["image/vnd.zbrush.pcx", "image/x-pcx"],
                Format::Sgi => vec!["image/sgi"],
                Format::Wbmp => vec!["image/vnd.wap.wbmp"],
                Format::Xbm => vec!["image/x-xbitmap", "image/x-xbm"],
                Format::Xpm => vec!["image/x-xpixmap"],
                _ => vec![],
            },
        }
    }
}
