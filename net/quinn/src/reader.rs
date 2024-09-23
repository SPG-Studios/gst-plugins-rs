// Adapted from
// https://github.com/cloudflare/moq-rs/blob/main/moq-transport/src/session/reader.rs

#![allow(dead_code)]
use anyhow::Result;
use bytes::{Buf, Bytes, BytesMut};
use moq_transport::coding::{Decode, DecodeError};
use std::{cmp, io};
use web_transport_quinn::RecvStream;

pub struct Reader {
    stream: RecvStream,
    buffer: BytesMut,
}

impl Reader {
    pub fn new(stream: RecvStream) -> Self {
        Self {
            stream,
            buffer: Default::default(),
        }
    }

    pub async fn decode<T: Decode>(&mut self) -> Result<Option<T>> {
        loop {
            let mut cursor = io::Cursor::new(&self.buffer);

            let required = match T::decode(&mut cursor) {
                Ok(msg) => {
                    self.buffer.advance(cursor.position() as usize);
                    return Ok(Some(msg));
                }
                Err(DecodeError::More(required)) => self.buffer.len() + required,
                Err(err) => return Err(err.into()),
            };

            loop {
                match self.read_chunk(1024usize).await {
                    Ok(Some(bytes)) => self.buffer.extend_from_slice(&bytes),
                    Ok(None) => break,
                    Err(e) => {
                        // We do not want to raise an error if session was closed
                        if e.to_string().contains("connection error: closed") {
                            return Ok(None);
                        }
                        return Err(e);
                    }
                }

                if self.buffer.len() >= required {
                    break;
                }
            }
        }
    }

    async fn read_chunk(&mut self, max: usize) -> Result<Option<Bytes>> {
        use tokio::io::AsyncReadExt;

        if self.buffer.is_empty() {
            self.buffer.reserve(max);
            let n = self.stream.read_buf(&mut self.buffer).await?;
            if n == 0 {
                return Ok(None);
            }
        }

        let size = cmp::min(max, self.buffer.len());
        let data = self.buffer.split_to(size).freeze();

        Ok(Some(data))
    }

    pub async fn done(&mut self) -> Result<bool> {
        if !self.buffer.is_empty() {
            return Ok(false);
        }

        Ok(self.stream.read(&mut self.buffer).await?.is_none())
    }
}
