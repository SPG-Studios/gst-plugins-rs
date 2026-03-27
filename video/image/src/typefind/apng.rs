use gst::glib;

use crate::typefind::cat::CAT;
use crate::typefind::seeker::W;

#[inline(never)]
fn type_find(typefind: &mut gst::TypeFind) {
    use gst::{Caps, TypeFindProbability};

    let cursor = std::io::BufReader::new(W::from(&mut *typefind));
    let mut options = png::DecodeOptions::default();
    options.set_ignore_checksums(true);
    options.set_ignore_iccp_chunk(true);
    options.set_ignore_text_chunk(true);
    // TODO: consider using image-rs directly
    if let Ok(v) = png::Decoder::new_with_options(cursor, options)
        // read_header_info is not enough, it just parses basic info
        // we need to find the acTL chunk
        .read_info()
        .map(|v| v.info().clone())
    {
        gst::log!(
            CAT,
            "extracted PNG width and height: {}x{}",
            v.width,
            v.height
        );

        if v.is_animated() {
            typefind.suggest(
                TypeFindProbability::Maximum,
                &Caps::builder("image/x-gst-apng").build(),
            );
        } else {
            typefind.suggest(
                TypeFindProbability::Maximum,
                &Caps::builder("image/png").build(),
            );
        }
    }
}

pub(crate) fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    let mut mediatypes = gst::Caps::new_empty();
    mediatypes
        .make_mut()
        .append(gst::Caps::builder("image/x-gst-apng").build());
    mediatypes
        .make_mut()
        .append(gst::Caps::builder("image/png").build());
    gst::TypeFind::register(
        Some(plugin),
        "image/x-gst-apng",
        // Needs to be bumped before the native
        gst::Rank::PRIMARY + 15,
        Some("png"),
        Some(&mediatypes),
        type_find,
    )
}
