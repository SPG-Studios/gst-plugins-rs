// SPDX-CopyrightText: 2026 Amyspark <amy@centricular.com>
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;

mod imp;

#[derive(Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Clone, Copy, glib::Enum)]
#[repr(u32)]
#[enum_type(name = "GstRsImageFormat")]
pub(crate) enum Format {
    #[enum_value(name = "AV1 image file format", nick = "avif")]
    Avif,
    #[enum_value(name = "Microsoft bitmap", nick = "bmp")]
    Bmp,
    #[enum_value(name = "OpenEXR", nick = "exr")]
    Exr,
    #[enum_value(name = "The Farbfeld simple image encoding format", nick = "farbfeld")]
    Farbfeld,
    #[enum_value(name = "JPEG image file format", nick = "jpeg")]
    Jpeg,
    #[enum_value(name = "Portable Network Graphics", nick = "jpeg")]
    Png,
    #[enum_value(name = "The Quite OK Image Format", nick = "qoi")]
    Qoi,
    #[enum_value(name = "Truevision Targa", nick = "tga")]
    Tga,
    #[enum_value(name = "Tagged Image File Format", nick = "tiff")]
    Tiff,
}

impl From<Format> for image::ImageFormat {
    #[allow(deprecated)]
    fn from(value: Format) -> Self {
        match value {
            Format::Avif => image::ImageFormat::Avif,
            Format::Bmp => image::ImageFormat::Bmp,
            Format::Exr => image::ImageFormat::OpenExr,
            Format::Farbfeld => image::ImageFormat::Farbfeld,
            Format::Jpeg => image::ImageFormat::Jpeg,
            Format::Png => image::ImageFormat::Png,
            Format::Qoi => image::ImageFormat::Qoi,
            Format::Tga => image::ImageFormat::Tga,
            Format::Tiff => image::ImageFormat::Tiff,
        }
    }
}

impl Into<&'static str> for Format {
    fn into(self) -> &'static str {
        match self {
            Format::Avif => "image/avif",
            Format::Bmp => "image/bmp",
            Format::Exr => "image/exr",
            Format::Farbfeld => "image/x-farbfeld",
            Format::Jpeg => "image/jpeg",
            Format::Png => "image/png",
            Format::Qoi => "image/qoi",
            Format::Tga => "image/x-tga",
            Format::Tiff => "image/tiff",
        }
    }
}

glib::wrapper! {
    pub struct Encoder(ObjectSubclass<imp::Encoder>) @extends gst_video::VideoEncoder, gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    #[cfg(feature = "doc")]
    Format::static_type().mark_as_plugin_api(gst::PluginAPIFlags::empty());

    gst::Element::register(
        Some(plugin),
        "imagersenc",
        gst::Rank::SECONDARY + 1,
        Encoder::static_type(),
    )
}
