use gst::glib;

mod cat;

#[cfg(any(feature = "png", feature = "ico"))]
mod apng;
#[cfg(feature = "avif")]
mod avif;
#[cfg(feature = "dds")]
mod dds;
#[cfg(feature = "ff")]
mod farbfeld;
#[cfg(feature = "otb")]
mod nokia;
#[cfg(feature = "ora")]
mod openraster;
#[cfg(feature = "pcx")]
mod pcx;
#[cfg(feature = "qoi")]
mod qoi;
#[cfg(feature = "hdr")]
mod radiance;
#[cfg(feature = "sgi")]
mod sgi;
#[cfg(feature = "wbmp")]
mod wbmp;
#[cfg(feature = "xbm")]
mod xbm;
#[cfg(feature = "xpm")]
mod xpm;

pub(crate) fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    #[cfg(any(feature = "png", feature = "ico"))]
    apng::register(plugin)?;
    #[cfg(feature = "avif")]
    // Without this, AVIF files are interpreted as MPEG-4 HEIC files
    avif::register(plugin)?;
    #[cfg(feature = "dds")]
    dds::register(plugin)?;
    #[cfg(feature = "ff")]
    farbfeld::register(plugin)?;
    #[cfg(feature = "hdr")]
    radiance::register(plugin)?;
    #[cfg(feature = "ora")]
    openraster::register(plugin)?;
    #[cfg(feature = "otb")]
    nokia::register(plugin)?;
    #[cfg(feature = "pcx")]
    pcx::register(plugin)?;
    #[cfg(feature = "qoi")]
    qoi::register(plugin)?;
    #[cfg(feature = "sgi")]
    sgi::register(plugin)?;
    #[cfg(feature = "wbmp")]
    wbmp::register(plugin)?;
    #[cfg(feature = "xbm")]
    xbm::register(plugin)?;
    #[cfg(feature = "xpm")]
    xpm::register(plugin)?;
    Ok(())
}
