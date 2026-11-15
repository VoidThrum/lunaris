use std::io;

use bytes::BytesMut;

use crate::video::{
    annexb::AnnexBSplitter,
    h264::{Nal, NalHeader},
};

pub struct H264Reader {
    annex_b: AnnexBSplitter,
}

impl H264Reader {
    /// Reads the NAL units of one encoded frame; they are views into `frame`.
    pub fn new(frame: BytesMut) -> Self {
        Self {
            annex_b: AnnexBSplitter::new(frame),
        }
    }

    pub fn next_nal(&mut self) -> Result<Option<Nal>, io::Error> {
        while let Some(annex_b) = self.annex_b.next()? {
            let header_range = annex_b.payload_range.start..(annex_b.payload_range.start + 1);
            // A unit shorter than its header is skipped (it has nothing to send).
            if header_range.end > annex_b.payload_range.end {
                continue;
            }
            let Some(bytes) = annex_b.full.get(header_range.clone()) else { continue };
            let mut header = [0u8; 1];
            header.copy_from_slice(bytes);
            let header = NalHeader::parse(header);

            let payload_range = header_range.end..annex_b.payload_range.end;

            return Ok(Some(Nal {
                payload_range,
                header,
                header_range,
                start_code: annex_b.start_code,
                start_code_range: annex_b.start_code_range,
                full: annex_b.full,
            }));
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn units(frame: &[u8]) -> Vec<Vec<u8>> {
        let mut reader = H264Reader::new(BytesMut::from(frame));
        let mut out = Vec::new();
        while let Some(nal) = reader.next_nal().unwrap() {
            out.push(nal.full[nal.header_range.start..nal.payload_range.end].to_vec());
        }
        out
    }

    #[test]
    fn units_shorter_than_the_header_are_skipped() {
        // An empty unit between two start codes, one at the very end.
        let frame = [0, 0, 0, 1, 0, 0, 0, 1, 0x65, 0x88, 0, 0, 1];
        assert_eq!(units(&frame), vec![vec![0x65, 0x88]]);
        assert!(units(&[0, 0, 1]).is_empty());
    }
}
