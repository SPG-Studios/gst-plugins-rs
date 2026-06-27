// Adapted from
// https://github.com/cloudflare/moq-rs/blob/main/moq-transport/src/session/writer.rs

#![allow(dead_code)]
use anyhow::Result;
use bytes::{Buf, Bytes};
use moq_transport::coding::{Encode, EncodeError};
use web_transport_quinn::SendStream;

pub struct Writer {
    stream: SendStream,
    buffer: bytes::BytesMut,
}

impl Writer {
    pub fn new(stream: SendStream) -> Self {
        Self {
            stream,
            buffer: Default::default(),
        }
    }

    pub async fn encode<T: Encode>(&mut self, msg: &T) -> Result<()> {
        self.buffer.clear();
        msg.encode(&mut self.buffer)?;

        while !self.buffer.is_empty() {
            let written = self.stream.write(&self.buffer).await?;
            if written == 0 {
                // The QUIC stream reported EOF before we finished.
                return Err(EncodeError::More(self.buffer.remaining()).into());
            }
            self.buffer.advance(written);
        }

        Ok(())
    }

    pub async fn write(&mut self, buf: &[u8]) -> Result<()> {
        let mut remaining = Bytes::copy_from_slice(buf);
        while remaining.has_remaining() {
            let written = self.stream.write(&remaining).await?;
            if written == 0 {
                return Err(EncodeError::More(remaining.remaining()).into());
            }
            remaining.advance(written);
        }

        Ok(())
    }

    pub fn close(&mut self) {
        let _ = self.stream.finish();
        let _ = self.stream.reset(0u32);
    }
}
