use gst::glib;
use image::ImageDecoder;

use crate::typefind::cat::CAT;
use crate::typefind::seeker::W;

#[inline(never)]
fn type_find(typefind: &mut gst::TypeFind) {
    use gst::{Caps, TypeFindProbability};

    let cursor = std::io::BufReader::new(W::from(&mut *typefind));
    if let Ok(decoder) = image::codecs::farbfeld::FarbfeldDecoder::new(cursor) {
        let d = decoder.dimensions();
        gst::log!(CAT, "extracted Farbfeld width and height: {}x{}", d.0, d.1);
        typefind.suggest(
            TypeFindProbability::Maximum,
            &Caps::builder("image/x-farbfeld").build(),
        );
    }
}

pub(crate) fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    let mediatypes = gst::Caps::builder("image/x-farbfeld").build();
    gst::TypeFind::register(
        Some(plugin),
        "image/x-farbfeld",
        gst::Rank::PRIMARY,
        Some("ff"),
        Some(&mediatypes),
        type_find,
    )
}
