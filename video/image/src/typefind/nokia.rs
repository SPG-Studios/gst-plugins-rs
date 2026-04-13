use gst::glib;
use image::ImageDecoder;

use crate::typefind::cat::CAT;

fn type_find(typefind: &mut gst::TypeFind) {
    use gst::{Caps, TypeFindProbability};

    let cursor = std::io::BufReader::new(typefind.as_reader());
    if let Ok(decoder) = image_extras::otb::OtbDecoder::new(cursor) {
        let d = decoder.dimensions();
        gst::log!(CAT, "extracted Nokia OTA width and height: {}x{}", d.0, d.1);
        typefind.suggest(
            TypeFindProbability::Maximum,
            &Caps::builder("image/x-nokia-over-the-air-bitmap").build(),
        );
    }
}

pub(crate) fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    let mediatypes = gst::Caps::builder("image/x-nokia-over-the-air-bitmap").build();
    gst::TypeFind::register(
        Some(plugin),
        "image/x-nokia-over-the-air-bitmap",
        // Needs to be bumped before typefind
        gst::Rank::SECONDARY,
        Some("otb"),
        Some(&mediatypes),
        type_find,
    )
}
