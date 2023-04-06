use gst::{TypeFind, TypeFindProbability};
use gst::glib;

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    TypeFind::register(
        Some(plugin),
        "qoa_typefind",
        gst::Rank::None,
        Some("qoa"),
        Some(&gst::Caps::builder("audio/x-qoa").build()),
        |typefind| {
            if let Some(data) = typefind.peek(0, qoaudio::QOA_MIN_FILESIZE as u32) {
                if qoaudio::QoaDecoder::decode_header(data).is_ok() {
                    typefind.suggest(
                        TypeFindProbability::Maximum,
                        &gst::Caps::builder("audio/x-qoa").build(),
                    );
                }
            }
        },
    )
}
