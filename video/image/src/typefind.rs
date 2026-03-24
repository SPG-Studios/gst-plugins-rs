use gst::glib;

use std::io::{Error, ErrorKind};

struct W<'a> {
    buf: &'a mut gst::TypeFind,
    pos: u32,
}

impl<'a> From<&'a mut gst::TypeFind> for W<'a> {
    fn from(buf: &'a mut gst::TypeFind) -> Self {
        Self { buf, pos: 0 }
    }
}

impl std::io::Read for W<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.len() > u32::MAX as usize {
            return Err(Error::from(ErrorKind::FileTooLarge));
        }
        match self.buf.peek(self.pos.into(), buf.len() as u32) {
            Some(v) => {
                buf.copy_from_slice(v);
                // SAFETY: peek verifies this
                self.pos += buf.len() as u32;
                Ok(buf.len())
            }
            None => Err(Error::from(ErrorKind::FileTooLarge)),
        }
    }
}

impl std::io::Seek for W<'_> {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        match pos {
            std::io::SeekFrom::Start(v) => {
                if v > u32::MAX as u64 {
                    return Err(Error::from(ErrorKind::FileTooLarge));
                }
                self.pos = v as u32;
            }
            std::io::SeekFrom::End(v) => {
                let len = self.buf.length().unwrap_or(0) as i64;
                let new_pos = match v.checked_add(len) {
                    Some(v) => {
                        if v < 0 {
                            return Err(Error::from(ErrorKind::FileTooLarge));
                        } else if v > u32::MAX as i64 {
                            return Err(Error::from(ErrorKind::FileTooLarge));
                        } else {
                            v
                        }
                    }
                    None => return Err(Error::from(ErrorKind::FileTooLarge)),
                };
                self.pos = new_pos as u32;
            }
            std::io::SeekFrom::Current(v) => {
                if v >= u32::MAX as i64 {
                    return Err(Error::from(ErrorKind::FileTooLarge));
                }
                let new_pos = match v.checked_add(self.pos as i64) {
                    Some(v) => {
                        if v < 0 {
                            return Err(Error::from(ErrorKind::FileTooLarge));
                        } else if v > u32::MAX as i64 {
                            return Err(Error::from(ErrorKind::FileTooLarge));
                        } else {
                            v
                        }
                    }
                    None => return Err(Error::from(ErrorKind::FileTooLarge)),
                };
                self.pos = new_pos as u32;
            }
        };
        Ok(self.pos as u64)
    }
}

#[inline(never)]
fn is_apng(typefind: &mut gst::TypeFind) {
    use gst::{Caps, TypeFindProbability};

    // I have no idea how long till the first iDAT,
    // so we'll need to swallow the whole typefind
    let cursor = std::io::BufReader::new(W::from(&mut *typefind));
    let mut options = png::DecodeOptions::default();
    options.set_ignore_checksums(true);
    options.set_ignore_iccp_chunk(true);
    options.set_ignore_text_chunk(true);
    // read_header_info is not enough, it just parses basic info
    // we need to find the acTL chunk
    if let Ok(v) = png::Decoder::new_with_options(cursor, options)
        .read_info()
        .map(|v| v.info().clone())
    {
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
    let mut foo = gst::Caps::new_empty();
    foo.make_mut().append(gst::Caps::builder("image/x-gst-apng").build());
    foo.make_mut().append(gst::Caps::builder("image/png").build());
    gst::TypeFind::register(
        Some(plugin),
        "image/x-gst-apng",
        // Needs to be bumped before typefind
        gst::Rank::PRIMARY + 100,
        Some("png"),
        Some(&foo),
        is_apng,
    )
}
