use gst::glib;
use image::ImageFormat;

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

            v => match ImageFormat::from_mime_type(value) {
                Some(v) => Format::try_from(v),
                None => Err(gst::error_msg!(
                    gst::StreamError::CodecNotFound,
                    ["Unknown mimetype {v}"]
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
                ImageFormat::Tga => vec![v.to_mime_type(), "image/x-tga"],
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
