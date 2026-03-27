use gst::glib;
use image::ImageDecoder;

use crate::typefind::cat::CAT;
use crate::typefind::seeker::W;

#[inline(never)]
fn type_find(typefind: &mut gst::TypeFind) {
    use gst::{Caps, TypeFindProbability};

    let cursor = std::io::BufReader::new(W::from(&mut *typefind));
    let _ = image_extras::xpm::XpmDecoder::new(cursor)
        .and_then(|decoder| {
            let d = decoder.dimensions();
            gst::log!(CAT, "extracted X Pixmap width and height: {}x{}", d.0, d.1);
            Ok(())
        })
        .and_then(|_| {
            typefind.suggest(
            TypeFindProbability::Maximum,
            &Caps::builder("image/x-xpixmap").build(),
        );
            Ok(())
        })
        .inspect_err(|e| gst::log!(CAT, "File is not X Pixmap: {:?}", e));
}

pub(crate) fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    let mediatypes = gst::Caps::builder("image/x-xpixmap").build();
    gst::TypeFind::register(
        Some(plugin),
        "image/x-xpixmap",
        // Needs to be bumped before the native
        gst::Rank::SECONDARY + 1,
        Some("xpm"),
        Some(&mediatypes),
        type_find,
    )
}
