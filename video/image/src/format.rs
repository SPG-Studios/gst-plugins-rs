use std::fmt::Display;

use gst::glib;
use image::ImageFormat;

use crate::caps::*;

#[derive(Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Clone, Copy, glib::Enum)]
#[repr(u32)]
#[enum_type(name = "GstRsImageFormat")]
pub(crate) enum Format {
    #[enum_value(name = "Animated PNG", nick = "apng")]
    Apng,
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
    #[enum_value(name = "Portable Network Graphics", nick = "png")]
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

#[derive(Debug, Copy, Clone)]
pub(crate) enum UnsupportedFormat<'a> {
    MimetypeNotFound(&'a str),
    NonNativeFormat(Format),
}

impl From<UnsupportedFormat<'_>> for gst::ErrorMessage {
    fn from(value: UnsupportedFormat) -> Self {
        gst::ErrorMessage::from(&value)
    }
}

impl From<&UnsupportedFormat<'_>> for gst::ErrorMessage {
    fn from(value: &UnsupportedFormat) -> Self {
        match value {
            UnsupportedFormat::MimetypeNotFound(v) => {
                gst::error_msg!(gst::StreamError::CodecNotFound, ["Unknown mimetype {v}"])
            }
            UnsupportedFormat::NonNativeFormat(v) => {
                gst::error_msg!(gst::StreamError::CodecNotFound, ["Unknown format {v:?}"])
            }
        }
    }
}

impl Display for UnsupportedFormat<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", gst::ErrorMessage::from(self))?;
        Ok(())
    }
}

impl TryFrom<Format> for ImageFormat {
    type Error = UnsupportedFormat<'static>;

    fn try_from(value: Format) -> Result<Self, Self::Error> {
        match value {
            Format::Avif => Ok(ImageFormat::Avif),
            Format::Apng => Ok(ImageFormat::Png),
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
            v => Err(UnsupportedFormat::NonNativeFormat(v)),
        }
    }
}

impl From<ImageFormat> for Format {
    fn from(value: ImageFormat) -> Format {
        match value {
            ImageFormat::Avif => Format::Avif,
            ImageFormat::Bmp => Format::Bmp,
            ImageFormat::Dds => Format::Dds,
            ImageFormat::OpenExr => Format::Exr,
            ImageFormat::Farbfeld => Format::Farbfeld,
            ImageFormat::Gif => Format::Gif,
            ImageFormat::Hdr => Format::Hdr,
            ImageFormat::Ico => Format::Ico,
            ImageFormat::Jpeg => Format::Jpeg,
            ImageFormat::Png => Format::Png,
            ImageFormat::Pnm => Format::Pnm,
            ImageFormat::Qoi => Format::Qoi,
            ImageFormat::Tga => Format::Tga,
            ImageFormat::Tiff => Format::Tiff,
            ImageFormat::WebP => Format::WebP,
            v => unimplemented!("{:?}", v),
        }
    }
}

impl<'a> TryFrom<&'a gst::StructureRef> for Format {
    type Error = UnsupportedFormat<'a>;

    fn try_from(value: &'a gst::StructureRef) -> Result<Self, Self::Error> {
        match value.name().as_str() {
            "image/x-gst-apng" => Ok(Format::Apng),
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
            "image/png" => {
                if let Ok(v) = value.get::<bool>("animated") {
                    if v {
                        return Ok(Format::Apng);
                    }
                }
                Ok(Format::Png)
            }
            "video/quicktime" => {
                if let Ok(v) = value.get::<String>("variant") {
                    if v == "avif" {
                        return Ok(Format::Avif);
                    }
                }
                Err(UnsupportedFormat::MimetypeNotFound(value.name().as_str()))
            }
            v => match ImageFormat::from_mime_type(v) {
                Some(v) => Ok(Format::from(v)),
                None => Err(UnsupportedFormat::MimetypeNotFound(v)),
            },
        }
    }
}

impl Format {
    pub(crate) fn all_animated_formats() -> impl IntoIterator<Item = &'static gst::Caps> {
        [
            #[cfg(any(feature = "png", feature = "ico"))]
            &*APNG_CAPS,
            // FIXME upstream: AVIF also supports animations
            // but needs image-rs support
            // #[cfg(feature = "avif")]
            // Format::Avif,
            #[cfg(feature = "gif")]
            &GIF_CAPS,
            #[cfg(feature = "webp")]
            &WEBP_CAPS,
        ]
    }

    /// Missing formats from gdkpixbufdec:
    /// - application/x-navi-animation
    /// - image/svg
    /// - image/svg+xml
    pub(crate) fn all_decoding_formats() -> impl IntoIterator<Item = &'static gst::Caps> {
        [
            // FIXME upstream: AVIF also supports animations
            // but needs image-rs support
            #[cfg(feature = "avif")]
            &*AVIF_CAPS,
            #[cfg(any(feature = "bmp", feature = "ico"))]
            &*BMP_CAPS,
            #[cfg(feature = "dds")]
            &*DDS_CAPS,
            #[cfg(feature = "exr")]
            &*OPENEXR_CAPS,
            #[cfg(feature = "ff")]
            &*FARBFELD_CAPS,
            #[cfg(feature = "hdr")]
            &*HDR_CAPS,
            #[cfg(feature = "ico")]
            &*ICO_CAPS,
            #[cfg(feature = "jpeg")]
            // FIXME upstream: doesn't support MJPEG
            &*JPEG_CAPS,
            #[cfg(feature = "ora")]
            &*ORA_CAPS,
            #[cfg(feature = "otb")]
            &*OTB_CAPS,
            #[cfg(feature = "pcx")]
            &*PCX_CAPS,
            #[cfg(any(feature = "png", feature = "ico"))]
            &*PNG_CAPS,
            #[cfg(feature = "pnm")]
            &*PNM_CAPS,
            // https://github.com/phoboslab/qoi/issues/167
            #[cfg(feature = "qoi")]
            &*QOI_CAPS,
            #[cfg(feature = "sgi")]
            &*SGI_CAPS,
            #[cfg(feature = "tga")]
            &*TGA_CAPS,
            #[cfg(feature = "tiff")]
            &*TIFF_CAPS,
            #[cfg(feature = "wbmp")]
            &*WBMP_CAPS,
            #[cfg(feature = "xbm")]
            &*XBM_CAPS,
            #[cfg(feature = "xpm")]
            &*XPM_CAPS,
        ]
    }

    pub(crate) fn all_encoder_formats() -> impl IntoIterator<Item = &'static gst::Caps> {
        [
            #[cfg(any(feature = "png", feature = "ico"))]
            &*PNG_ENC_CAPS,
            #[cfg(feature = "tiff")]
            &*TIFF_ENC_CAPS,
            #[cfg(feature = "jpeg")]
            &*JPEG_ENC_CAPS,
            #[cfg(feature = "bmp")]
            &*BMP_ENC_CAPS,
            #[cfg(feature = "tga")]
            &*TGA_ENC_CAPS,
            #[cfg(feature = "avif")]
            &*AVIF_ENC_CAPS,
            #[cfg(feature = "ff")]
            &*FARBFELD_ENC_CAPS,
            #[cfg(feature = "qoi")]
            &*QOI_ENC_CAPS,
        ]
    }
}
