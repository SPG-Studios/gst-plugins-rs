use std::sync::LazyLock;

pub(crate) static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "ImageRsTypeFind",
        gst::DebugColorFlags::empty(),
        Some("image-rs typefinding"),
    )
});
