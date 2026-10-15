use std::io;

use flate2::{Decompress, FlushDecompress};

/// Every complete zlib-stream message ends with a sync flush marker.
const FLUSH_SUFFIX: [u8; 4] = [0x00, 0x00, 0xff, 0xff];
const GROW_BY: usize = 64 * 1024;

/// Decompresses the gateway's `zlib-stream` transport: one zlib context shared by the whole
/// connection, with messages possibly split across several binary frames.
pub struct Inflater {
    zlib: Decompress,
    input: Vec<u8>,
    output: Vec<u8>,
}

impl Inflater {
    pub fn new() -> Self {
        Self {
            zlib: Decompress::new(true),
            input: Vec::new(),
            output: Vec::with_capacity(GROW_BY),
        }
    }

    /// Feeds one binary frame. Returns the decompressed message once it is complete.
    pub fn push(&mut self, frame: &[u8]) -> io::Result<Option<&[u8]>> {
        self.input.extend_from_slice(frame);
        if !self.input.ends_with(&FLUSH_SUFFIX) {
            return Ok(None);
        }

        self.output.clear();
        let mut offset = 0;
        loop {
            if self.output.len() == self.output.capacity() {
                self.output.reserve(GROW_BY);
            }
            let (in_before, out_before) = (self.zlib.total_in(), self.output.len());
            self.zlib
                .decompress_vec(&self.input[offset..], &mut self.output, FlushDecompress::Sync)
                .map_err(io::Error::other)?;
            offset += (self.zlib.total_in() - in_before) as usize;

            let output_full = self.output.len() == self.output.capacity();
            if offset == self.input.len() && !output_full {
                break;
            }
            if !output_full && self.output.len() == out_before && self.zlib.total_in() == in_before {
                return Err(io::Error::other("zlib stream made no progress"));
            }
        }

        self.input.clear();
        Ok(Some(&self.output))
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use flate2::{Compression, write::ZlibEncoder};

    use super::*;

    /// Compresses messages the way the gateway does: one stream, a sync flush after each message.
    fn compress(messages: &[&str]) -> Vec<Vec<u8>> {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        messages
            .iter()
            .map(|message| {
                encoder.write_all(message.as_bytes()).unwrap();
                encoder.flush().unwrap();
                std::mem::take(encoder.get_mut())
            })
            .collect()
    }

    #[test]
    fn decodes_consecutive_messages_sharing_one_context() {
        let frames = compress(&[r#"{"op":10}"#, r#"{"op":11}"#]);
        let mut inflater = Inflater::new();
        assert_eq!(inflater.push(&frames[0]).unwrap().unwrap(), br#"{"op":10}"#);
        assert_eq!(inflater.push(&frames[1]).unwrap().unwrap(), br#"{"op":11}"#);
    }

    #[test]
    fn waits_for_the_flush_suffix_when_a_message_is_split() {
        let big = "x".repeat(300_000);
        let frames = compress(&[&big]);
        let (head, tail) = frames[0].split_at(frames[0].len() / 2);
        let mut inflater = Inflater::new();
        assert!(inflater.push(head).unwrap().is_none());
        assert_eq!(inflater.push(tail).unwrap().unwrap(), big.as_bytes());
    }
}
