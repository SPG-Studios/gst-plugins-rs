use std::io::{Error, ErrorKind, Read, Seek, SeekFrom};

pub(crate) struct W<'a> {
    buf: &'a mut gst::TypeFind,
    pos: i64,
}

impl<'a> From<&'a mut gst::TypeFind> for W<'a> {
    fn from(buf: &'a mut gst::TypeFind) -> Self {
        Self { buf, pos: 0 }
    }
}

impl Read for W<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        // ZipArchive expects reads beyond the end of the file
        // to just return the trailing chunk
        let remaining = self.buf.length().unwrap_or(0) - self.pos as u64;
        let read_length = buf.len().min(u32::MAX as usize).min(remaining as usize) as u32;
        match self.buf.peek(self.pos, read_length) {
            Some(v) => {
                buf[..v.len()].copy_from_slice(v);
                // SAFETY: checked above
                self.pos = self.pos + v.len() as i64;
                Ok(v.len())
            }
            None => Ok(0),
        }
    }
}

impl Seek for W<'_> {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        match pos {
            SeekFrom::Start(v) => {
                self.pos = v.min(i64::MAX as u64) as i64;
            }
            SeekFrom::End(v) => {
                let len = self.buf.length().unwrap_or(0);
                let new_pos = match v.checked_add(len as i64) {
                    Some(v) => {
                        if v < 0 {
                            return Err(Error::from(ErrorKind::FileTooLarge));
                        } else {
                            v
                        }
                    }
                    None => i64::MAX,
                };
                self.pos = new_pos;
            }
            SeekFrom::Current(v) => {
                let new_pos = match v.checked_add(self.pos) {
                    Some(v) => {
                        if v < 0 {
                            return Err(Error::from(ErrorKind::FileTooLarge));
                        } else {
                            v
                        }
                    }
                    None => i64::MAX,
                };
                self.pos = new_pos;
            }
        };
        Ok(self.pos as u64)
    }
}
