use gst::glib;
use gst::{TypeFind, TypeFindProbability};

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    TypeFind::register(
        Some(plugin),
        "qoa_typefind",
        gst::Rank::None,
        Some("qoa"),
        Some(&gst::Caps::builder("audio/x-qoa").build()),
        |typefind| {
            if let Some(data) = typefind.peek(0, qoaudio::QOA_MIN_FILESIZE as u32) {
                let magic = u32::from_be_bytes(data[0..4].try_into().unwrap());
                if magic == qoaudio::QOA_MAGIC {
                    typefind.suggest(
                        TypeFindProbability::Maximum,
                        &gst::Caps::builder("audio/x-qoa").build(),
                    );
                }
            }
        },
    )
}
