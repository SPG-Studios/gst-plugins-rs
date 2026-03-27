use gst::glib;
use image::ImageDecoder;

use crate::typefind::cat::CAT;
use crate::typefind::seeker::W;

#[inline(never)]
fn type_find(typefind: &mut gst::TypeFind) {
    use gst::{Caps, TypeFindProbability};

    let cursor = std::io::BufReader::new(W::from(&mut *typefind));
    // I use this alternative as there can be only one living mutable reference
    // and, unlike the other decoders, OpenRasterDecoder ties its lifetime
    // to the lifetime of cursor.
    // Thus making it all but impossible to access cursor simultaneously with
    // typefind
    let _ = image_extras::ora::OpenRasterDecoder::with_limits(cursor, Default::default())
        .and_then(|decoder| {
            let d = decoder.dimensions();
            gst::log!(
                CAT,
                "extracted OpenRaster width and height: {}x{}",
                d.0,
                d.1
            );
            Ok(())
        })
        .and_then(|_| {
            typefind.suggest(
                TypeFindProbability::Maximum,
                &Caps::builder("image/openraster").build(),
            );
            Ok(())
        })
        .inspect_err(|e| gst::log!(CAT, "File is not OpenRaster: {:?}", e));
}

pub(crate) fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    let mediatypes = gst::Caps::builder("image/openraster").build();
    gst::TypeFind::register(
        Some(plugin),
        "image/openraster",
        // Needs to be bumped before application/zip
        gst::Rank::SECONDARY + 1,
        Some("ora"),
        Some(&mediatypes),
        type_find,
    )
}
