fn main() {
    gst_plugin_version_helper::info();

    // The GL-enabled skia build references core GL symbols (glActiveTexture, …).
    // Link libGL so they resolve when the plugin .so is dlopen'd. This targets
    // desktop GL; an embedded/GLES build would link GLESv2 instead.
    if std::env::var("CARGO_FEATURE_GL").is_ok() {
        println!("cargo:rustc-link-lib=dylib=GL");
    }
}
