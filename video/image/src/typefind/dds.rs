use gst::glib;
use image::ImageDecoder;

use crate::typefind::cat::CAT;
use crate::typefind::seeker::W;

#[inline(never)]
fn type_find(typefind: &mut gst::TypeFind) {
    use gst::{Caps, TypeFindProbability};

    let cursor = std::io::BufReader::new(W::from(&mut *typefind));
    if let Ok(decoder) = image::codecs::dds::DdsDecoder::new(cursor) {
        let d = decoder.dimensions();
        gst::log!(
            CAT,
            "extracted DirectDraw surface width and height: {}x{}",
            d.0,
            d.1
        );
        typefind.suggest(
            TypeFindProbability::Maximum,
            &Caps::builder("image/vnd-ms.dds").build(),
        );
    }
}

pub(crate) fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    let mediatypes = gst::Caps::builder("image/vnd-ms.dds").build();
    gst::TypeFind::register(
        Some(plugin),
        "image/vnd-ms.dds",
        gst::Rank::PRIMARY,
        Some("dds"),
        Some(&mediatypes),
        type_find,
    )
}
