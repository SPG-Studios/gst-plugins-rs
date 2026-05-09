use gst::glib;
use image::ImageDecoder;

use crate::typefind::cat::CAT;

#[inline(never)]
fn type_find(typefind: &mut gst::TypeFind) {
    use gst::{Caps, TypeFindProbability};

    let cursor = std::io::BufReader::new(typefind.as_reader());
    if let Ok(decoder) = image_extras::pcx::PCXDecoder::new(cursor) {
        let d = decoder.dimensions();
        gst::log!(CAT, "extracted PCX width and height: {}x{}", d.0, d.1);
        typefind.suggest(
            TypeFindProbability::Maximum,
            &Caps::builder("image/vnd.zbrush.pcx").build(),
        );
    }
}

pub(crate) fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    let mediatypes = gst::Caps::builder("image/vnd.zbrush.pcx").build();
    gst::TypeFind::register(
        Some(plugin),
        "image/vnd.zbrush.pcx",
        gst::Rank::PRIMARY,
        Some("pcx"),
        Some(&mediatypes),
        type_find,
    )
}
