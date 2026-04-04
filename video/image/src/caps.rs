use std::sync::LazyLock;

use image::ImageFormat;

macro_rules! append_caps {
    ($c:ident, $mime:literal) => (
        $c.append(gst::Caps::builder($mime).build());
    );
    ($c:ident, $mime:literal, $($mime2:literal),+) => (
        $c.append(gst::Caps::builder($mime).build());
        append_caps!($c, $($mime2),+)
    );
}

macro_rules! make_new_caps {
    ($caps:ident, $mime:literal) => {
        pub(crate) static $caps: LazyLock<gst::Caps> = LazyLock::new(|| {
            gst::Caps::builder($mime).build()
        });
    };

    ($caps:ident, $($mime:literal),+) => {
        pub(crate) static $caps: LazyLock<gst::Caps> = LazyLock::new(|| {
            let mut caps = gst::Caps::new_empty();
            let c = caps.make_mut();
            append_caps!(c, $($mime),+);
            caps
        });
    };

    ($caps:ident, $format:expr) => {
        pub(crate) static $caps: LazyLock<gst::Caps> = LazyLock::new(|| {
            gst::Caps::builder($format.to_mime_type()).build()
        });
    };

    ($caps:ident, $format:expr, $($mime:literal),+) => {
        pub(crate) static $caps: LazyLock<gst::Caps> = LazyLock::new(|| {
            let mut caps = gst::Caps::new_empty();
            let c = caps.make_mut();
            c.append(gst::Caps::builder($format.to_mime_type()).build());
            append_caps!(c, $($mime),+);
            caps
        });
    };
}

#[cfg(any(feature = "png", feature = "ico"))]
pub(crate) static APNG_CAPS: LazyLock<gst::Caps> = LazyLock::new(|| {
    let mut caps = gst::Caps::new_empty();
    let c = caps.make_mut();
    c.append(gst::Caps::builder("image/x-gst-apng").build());
    c.append(
        gst::Caps::builder("image/png")
            .field("animated", true)
            .build(),
    );
    caps
});

#[cfg(any(feature = "bmp", feature = "ico"))]
make_new_caps!(BMP_CAPS, ImageFormat::Bmp, "image/x-MS-bmp");
#[cfg(feature = "dds")]
make_new_caps!(DDS_CAPS, ImageFormat::Dds, "image/x-direct-draw-surface");
#[cfg(feature = "pnm")]
make_new_caps!(
    PNM_CAPS,
    ImageFormat::Pnm,
    "image/x-portable-bitmap",
    "image/x-portable-graymap",
    "image/x-portable-pixmap"
);

#[cfg(feature = "ff")]
/// farbfeld's MIME type in image-rs is application/octet-stream, correct it here
make_new_caps!(FARBFELD_CAPS, "image/x-farbfeld");
/// https://github.com/phoboslab/qoi/issues/167
make_new_caps!(QOI_CAPS, ImageFormat::Qoi, "image/qoi");
#[cfg(feature = "tga")]
make_new_caps!(TGA_CAPS, ImageFormat::Tga, "image/x-tga");
#[cfg(any(feature = "png", feature = "ico"))]
make_new_caps!(PNG_CAPS, ImageFormat::Png);
#[cfg(feature = "jpeg")]
make_new_caps!(JPEG_CAPS, ImageFormat::Jpeg);
#[cfg(feature = "gif")]
make_new_caps!(GIF_CAPS, ImageFormat::Gif);
#[cfg(feature = "webp")]
make_new_caps!(WEBP_CAPS, ImageFormat::WebP);
#[cfg(feature = "tiff")]
make_new_caps!(TIFF_CAPS, ImageFormat::Tiff);
#[cfg(feature = "ico")]
make_new_caps!(ICO_CAPS, ImageFormat::Ico);
#[cfg(feature = "hdr")]
make_new_caps!(HDR_CAPS, ImageFormat::Hdr);
#[cfg(feature = "exr")]
make_new_caps!(OPENEXR_CAPS, ImageFormat::OpenExr);

#[cfg(feature = "avif")]
pub(crate) static AVIF_CAPS: LazyLock<gst::Caps> = LazyLock::new(|| {
    let mut caps = gst::Caps::new_empty();
    let c = caps.make_mut();
    c.append(gst::Caps::builder(ImageFormat::Avif.to_mime_type()).build());
    c.append(
        gst::Caps::builder("video/quicktime")
            .field("variant", "avif")
            .build(),
    );
    caps
});

#[cfg(feature = "otb")]
/// https://snisurset.net/code/abydos/supported.html
make_new_caps!(OTB_CAPS, "image/x-nokia-over-the-air-bitmap");
#[cfg(feature = "ora")]
make_new_caps!(ORA_CAPS, "image/openraster");
#[cfg(feature = "pcx")]
make_new_caps!(PCX_CAPS, "image/vnd.zbrush.pcx", "image/x-pcx");
#[cfg(feature = "sgi")]
make_new_caps!(SGI_CAPS, "image/sgi");
#[cfg(feature = "wbmp")]
make_new_caps!(WBMP_CAPS, "image/vnd.wap.wbmp");
#[cfg(feature = "xbm")]
make_new_caps!(XBM_CAPS, "image/x-xbitmap", "image/x-xbm");
#[cfg(feature = "xpm")]
make_new_caps!(XPM_CAPS, "image/x-xpixmap");

macro_rules! make_new_encoder_caps {
    ($caps:ident, $format_caps:ident, $format:expr) => {
        pub(crate) static $caps: LazyLock<gst::Caps> = LazyLock::new(|| {
            let mut caps = $format_caps.clone();
            caps.make_mut().iter_mut().for_each(|v| {
                v.set(
                    "format",
                    gst::List::new($format.into_iter().map(|f| f.to_str())),
                )
            });
            caps
        });
    };
}

#[cfg(any(feature = "png", feature = "ico"))]
make_new_encoder_caps!(
    PNG_ENC_CAPS,
    PNG_CAPS,
    [
        #[cfg(target_endian = "big")]
        gst_video::VideoFormat::Rgba64Be,
        #[cfg(target_endian = "little")]
        gst_video::VideoFormat::Rgba64Le,
        gst_video::VideoFormat::Rgba,
        gst_video::VideoFormat::Rgb,
        #[cfg(target_endian = "big")]
        gst_video::VideoFormat::Gray16Be,
        #[cfg(target_endian = "little")]
        gst_video::VideoFormat::Gray16Le,
        gst_video::VideoFormat::Gray8,
    ]
);
#[cfg(feature = "tiff")]
make_new_encoder_caps!(
    TIFF_ENC_CAPS,
    TIFF_CAPS,
    [
        #[cfg(target_endian = "big")]
        gst_video::VideoFormat::Rgba64Be,
        #[cfg(target_endian = "little")]
        gst_video::VideoFormat::Rgba64Le,
        gst_video::VideoFormat::Rgba,
        gst_video::VideoFormat::Rgb,
        #[cfg(target_endian = "big")]
        gst_video::VideoFormat::Gray16Be,
        #[cfg(target_endian = "little")]
        gst_video::VideoFormat::Gray16Le,
        gst_video::VideoFormat::Gray8,
    ]
);
#[cfg(feature = "jpeg")]
make_new_encoder_caps!(
    JPEG_ENC_CAPS,
    JPEG_CAPS,
    [
        gst_video::VideoFormat::Rgb,
        gst_video::VideoFormat::Gray8,
    ]
);
#[cfg(any(feature = "bmp", feature = "ico"))]
make_new_encoder_caps!(
    BMP_ENC_CAPS,
    BMP_CAPS,
    [
        gst_video::VideoFormat::Rgba,
        gst_video::VideoFormat::Rgb,
        // FIXME upstream: image-rs stores grayscale BMP as RGBA
        // gst_video::VideoFormat::Gray8,
    ]
);
#[cfg(feature = "tga")]
make_new_encoder_caps!(
    TGA_ENC_CAPS,
    TGA_CAPS,
    [
        gst_video::VideoFormat::Rgba,
        gst_video::VideoFormat::Rgb,
        gst_video::VideoFormat::Gray8,
    ]
);
#[cfg(feature = "avif")]
make_new_encoder_caps!(
    AVIF_ENC_CAPS,
    AVIF_CAPS,
    [gst_video::VideoFormat::Rgba, gst_video::VideoFormat::Rgb]
);
#[cfg(feature = "ff")]
make_new_encoder_caps!(
    FARBFELD_ENC_CAPS,
    FARBFELD_CAPS,
    [
        #[cfg(target_endian = "big")]
        gst_video::VideoFormat::Rgba64Be,
        #[cfg(target_endian = "little")]
        gst_video::VideoFormat::Rgba64Le,
    ]
);
#[cfg(feature = "qoi")]
make_new_encoder_caps!(
    QOI_ENC_CAPS,
    QOI_CAPS,
    [gst_video::VideoFormat::Rgba, gst_video::VideoFormat::Rgb]
);
