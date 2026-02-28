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

impl From<Format> for &'static str {
    fn from(value: Format) -> Self {
        match value {
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

impl TryFrom<&str> for Format {
    type Error = String;

    // FIXME: there are more mimetypes that are equally valid,
    // (see decoder) but how do I export these in the pad templates?
    // Those make the conversion Format -> String not 1:1
    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "image/avif" => Ok(Format::Avif),
            "image/bmp" => Ok(Format::Bmp),
            "image/exr" => Ok(Format::Exr),
            "image/x-farbfeld" => Ok(Format::Farbfeld),
            "image/jpeg" => Ok(Format::Jpeg),
            "image/png" => Ok(Format::Png),
            "image/qoi" => Ok(Format::Qoi),
            "image/x-tga" => Ok(Format::Tga),
            "image/tiff" => Ok(Format::Tiff),
            v => Err(format!("Unsupported value {}", v)),
        }
    }
}

impl Format {
    fn all_values() -> impl IntoIterator<Item = Format> {
        [
            Format::Avif,
            Format::Bmp,
            Format::Exr,
            Format::Farbfeld,
            Format::Jpeg,
            Format::Png,
            Format::Qoi,
            Format::Tga,
            Format::Tiff,
        ]
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
