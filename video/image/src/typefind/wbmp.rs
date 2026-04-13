use gst::glib;
use image::ImageDecoder;

use crate::typefind::cat::CAT;

#[inline(never)]
fn type_find(typefind: &mut gst::TypeFind) {
    use gst::{Caps, TypeFindProbability};

    let cursor = std::io::BufReader::new(typefind.as_reader());
    if let Ok(decoder) = image_extras::wbmp::WbmpDecoder::new(cursor) {
        let d = decoder.dimensions();
        gst::warning!(
            CAT,
            "extracted WAP bitmap width and height: {}x{}",
            d.0,
            d.1
        );
        typefind.suggest(
            TypeFindProbability::NearlyCertain,
            &Caps::builder("image/vnd.wap.wbmp").build(),
        );
    }
}

pub(crate) fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    let mediatypes = gst::Caps::builder("image/vnd.wap.wbmp").build();
    gst::TypeFind::register(
        Some(plugin),
        "image/vnd.wap.wbmp",
        // Needs to be bumped before the native
        gst::Rank::MARGINAL + 1,
        Some("wbmp"),
        Some(&mediatypes),
        type_find,
    )
}
