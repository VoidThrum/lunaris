use std::{io, ops::Range};

use bytes::BytesMut;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnnexBStartCode {
    B3,
    B4,
}

impl AnnexBStartCode {
    pub fn code(&self) -> &'static [u8] {
        match self {
            Self::B3 => &[0, 0, 1],
            Self::B4 => &[0, 0, 0, 1],
        }
    }
    pub fn len(&self) -> usize {
        match self {
            Self::B3 => self.code().len(),
            Self::B4 => self.code().len(),
        }
    }
}

pub struct AnnexBData {
    pub payload_range: Range<usize>,
    pub start_code: AnnexBStartCode,
    pub start_code_range: Range<usize>,
    pub full: BytesMut,
}

/// Splits one encoded frame (already a single buffer) into its NAL units.
/// Each unit is a view into the frame's buffer (`BytesMut::split_to`), not a
/// copy. Bytes before the first start code are dropped; zero bytes that
/// trail a unit stay in it, except the one a 4-byte start code takes.
pub struct AnnexBSplitter {
    /// The unread rest of the frame; when `current_start_code` is set it
    /// begins with that start code.
    data: BytesMut,
    current_start_code: Option<AnnexBStartCode>,
}

impl AnnexBSplitter {
    pub fn new(data: BytesMut) -> Self {
        Self {
            data,
            current_start_code: None,
        }
    }

    pub fn next(&mut self) -> Result<Option<AnnexBData>, io::Error> {
        let start_code = match self.current_start_code {
            Some(start_code) => start_code,
            None => match find_start_code(&self.data, 0) {
                Some((at, start_code)) => {
                    let _ = self.data.split_to(at);
                    start_code
                }
                None => {
                    self.data.clear();
                    return Ok(None);
                }
            },
        };

        // The search for the next start code begins after this one, so its
        // bytes cannot be part of the next.
        let full = match find_start_code(&self.data, start_code.len()) {
            Some((at, next_start_code)) => {
                self.current_start_code = Some(next_start_code);
                self.data.split_to(at)
            }
            None => {
                self.current_start_code = None;
                self.data.split()
            }
        };

        Ok(Some(AnnexBData {
            payload_range: start_code.len()..full.len(),
            start_code,
            start_code_range: 0..start_code.len(),
            full,
        }))
    }
}

/// The first `00 00 01` in `data` at or after `from`, as (offset of the start
/// code, its kind): 4 bytes when a zero precedes it at or after `from`.
fn find_start_code(data: &[u8], from: usize) -> Option<(usize, AnnexBStartCode)> {
    let at = find_001(data, from)?;
    if at > from && data[at - 1] == 0 {
        Some((at - 1, AnnexBStartCode::B4))
    } else {
        Some((at, AnnexBStartCode::B3))
    }
}

/// Offset of the first `00 00 01` at or after `from`. Looks at the byte where
/// a match would end: a byte above 1 there (or a 1 that does not end a
/// match) rules out matches ending at it and the next two bytes.
fn find_001(data: &[u8], from: usize) -> Option<usize> {
    let mut end = from.checked_add(2)?;
    while end < data.len() {
        match data[end] {
            0 => end += 1,
            1 => {
                if data[end - 1] == 0 && data[end - 2] == 0 {
                    return Some(end - 2);
                }
                end += 3;
            }
            _ => end += 3,
        }
    }
    None
}

/// An Annex-B access unit without its filler data NAL units (H.264 type 12,
/// H.265 type 38, FD_NUT): bytes a constant-bitrate encoder adds to keep
/// its rate, which a decoder throws away. On a still screen they are most
/// of the stream. The frame is returned as it is when it has none.
pub fn without_filler(frame: Vec<u8>, h265: bool) -> Vec<u8> {
    let is_filler = |header: u8| if h265 { (header >> 1) & 0x3f == 38 } else { header & 0x1f == 12 };
    // Where each NAL unit begins, start code included (00 00 01, with the
    // zero byte before it when there is one).
    let mut starts: Vec<usize> = Vec::new();
    let mut i = 0;
    while i + 3 <= frame.len() {
        if frame[i] == 0 && frame[i + 1] == 0 && frame[i + 2] == 1 {
            starts.push(if i > 0 && frame[i - 1] == 0 { i - 1 } else { i });
            i += 3;
        } else {
            i += 1;
        }
    }
    let header_of = |start: usize| {
        let code = if frame[start + 2] == 1 { 3 } else { 4 };
        frame.get(start + code).copied()
    };
    if !starts.iter().any(|start| header_of(*start).is_some_and(is_filler)) {
        return frame;
    }
    let mut out = Vec::with_capacity(frame.len());
    out.extend_from_slice(&frame[..starts[0]]);
    for (n, start) in starts.iter().enumerate() {
        let end = starts.get(n + 1).copied().unwrap_or(frame.len());
        if !header_of(*start).is_some_and(is_filler) {
            out.extend_from_slice(&frame[*start..end]);
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use bytes::{BufMut, Bytes};
    use std::io::{Cursor, ErrorKind, Read};

    fn splitter(data: &[u8]) -> AnnexBSplitter {
        AnnexBSplitter::new(BytesMut::from(Bytes::from(data.to_vec())))
    }

    /// The byte-at-a-time splitter this module used before, kept as the
    /// reference the slice-based one must match.
    struct OldAnnexBSplitter<R: Read> {
        reader: R,
        start_code_buffer: [u8; 4],
        current_start_code: Option<AnnexBStartCode>,
        buffer: BytesMut,
    }

    impl<R: Read> OldAnnexBSplitter<R> {
        fn new(reader: R, capacity: usize) -> Self {
            Self {
                reader,
                start_code_buffer: [2u8; 4],
                current_start_code: None,
                buffer: BytesMut::with_capacity(capacity),
            }
        }

        fn next(&mut self) -> Result<Option<AnnexBData>, io::Error> {
            if self.current_start_code.is_none() {
                if let Some(start_code) = self.next_annex_b_start_code(false)? {
                    self.current_start_code = Some(start_code);
                    self.reset_start_code_buffer();
                    self.buffer.put(start_code.code());
                } else {
                    return Ok(None);
                }
            }
            let current_start_code = self.current_start_code.unwrap();
            let result = self.next_annex_b_start_code(true);
            match result {
                Ok(None) => {
                    self.current_start_code = None;
                    Ok(Some(AnnexBData {
                        payload_range: current_start_code.len()..self.buffer.len(),
                        start_code: current_start_code,
                        start_code_range: 0..current_start_code.len(),
                        full: self.buffer.split(),
                    }))
                }
                Err(err) if err.kind() == ErrorKind::UnexpectedEof => {
                    self.current_start_code = None;
                    Ok(Some(AnnexBData {
                        payload_range: current_start_code.len()..self.buffer.len(),
                        start_code: current_start_code,
                        start_code_range: 0..current_start_code.len(),
                        full: self.buffer.split(),
                    }))
                }
                Ok(Some(next_start_code)) => {
                    let mut full = self.buffer.split();
                    full.truncate(full.len() - next_start_code.len());
                    self.current_start_code = Some(next_start_code);
                    self.buffer.put(next_start_code.code());
                    self.reset_start_code_buffer();
                    Ok(Some(AnnexBData {
                        payload_range: current_start_code.len()..full.len(),
                        start_code: current_start_code,
                        start_code_range: 0..current_start_code.len(),
                        full,
                    }))
                }
                Err(err) => Err(err),
            }
        }

        fn next_annex_b_start_code(
            &mut self,
            buffer_bytes: bool,
        ) -> Result<Option<AnnexBStartCode>, io::Error> {
            loop {
                match &self.start_code_buffer {
                    [0, 0, 0, 1] => return Ok(Some(AnnexBStartCode::B4)),
                    [_, 0, 0, 1] => return Ok(Some(AnnexBStartCode::B3)),
                    _ => {}
                }
                let mut byte = [0u8; 1];
                if self.reader.read(&mut byte)? == 0 {
                    return Ok(None);
                }
                let byte = byte[0];
                if buffer_bytes {
                    self.buffer.put_u8(byte);
                }
                self.start_code_buffer.rotate_left(1);
                self.start_code_buffer[3] = byte;
            }
        }

        fn reset_start_code_buffer(&mut self) {
            self.start_code_buffer.copy_from_slice(&[2u8; 4]);
        }
    }

    type Unit = (Vec<u8>, AnnexBStartCode, Range<usize>, Range<usize>);

    fn units_new(data: &[u8]) -> Vec<Unit> {
        let mut s = splitter(data);
        let mut out = Vec::new();
        while let Some(u) = s.next().unwrap() {
            out.push((u.full.to_vec(), u.start_code, u.start_code_range, u.payload_range));
        }
        // Exhausted stays exhausted.
        assert!(s.next().unwrap().is_none());
        out
    }

    fn units_old(data: &[u8]) -> Vec<Unit> {
        let mut s = OldAnnexBSplitter::new(Cursor::new(data.to_vec()), 0);
        let mut out = Vec::new();
        while let Some(u) = s.next().unwrap() {
            out.push((u.full.to_vec(), u.start_code, u.start_code_range, u.payload_range));
        }
        assert!(s.next().unwrap().is_none());
        out
    }

    fn assert_same(data: &[u8]) {
        assert_eq!(units_new(data), units_old(data), "input {:?}", data);
    }

    #[test]
    fn test_splitter_single_b3() {
        let data = vec![0, 0, 1, 0x42, 0x01, 0x02];
        let mut splitter = splitter(&data);

        let nal = splitter.next().unwrap().unwrap();
        assert_eq!(nal.start_code, AnnexBStartCode::B3);
        assert_eq!(&nal.full[nal.payload_range.clone()], &[0x42, 0x01, 0x02]);
    }

    #[test]
    fn test_splitter_single_b4() {
        let data = vec![0, 0, 0, 1, 0x44, 0x05];
        let mut splitter = splitter(&data);

        let nal = splitter.next().unwrap().unwrap();
        assert_eq!(nal.start_code, AnnexBStartCode::B4);
        assert_eq!(&nal.full[nal.payload_range.clone()], &[0x44, 0x05]);
    }

    #[test]
    fn test_splitter_multiple_nalus() {
        let data = vec![0, 0, 0, 1, 0x42, 0x01, 0x02, 0, 0, 1, 0x44, 0x03, 0x04];
        let mut splitter = splitter(&data);

        let nal1 = splitter.next().unwrap().unwrap();
        assert_eq!(nal1.start_code, AnnexBStartCode::B4);
        assert_eq!(&nal1.full[nal1.payload_range.clone()], &[0x42, 0x01, 0x02]);

        let nal2 = splitter.next().unwrap().unwrap();
        assert_eq!(nal2.start_code, AnnexBStartCode::B3);
        assert_eq!(&nal2.full[nal2.payload_range.clone()], &[0x44, 0x03, 0x04]);
    }

    #[test]
    fn test_splitter_no_nalus() {
        let data = vec![0x01, 0x02, 0x03];
        let mut splitter = splitter(&data);

        let nal = splitter.next().unwrap();
        assert!(nal.is_none());
    }

    #[test]
    fn test_splitter_edge_case_start_of_stream() {
        let data = vec![0, 0, 0, 1, 0x42];
        let mut splitter = splitter(&data);

        let nal = splitter.next().unwrap().unwrap();
        assert_eq!(nal.start_code, AnnexBStartCode::B4);
        assert_eq!(&nal.full[nal.payload_range.clone()], &[0x42]);
    }

    #[test]
    fn units_are_views_into_the_frame() {
        let frame = Bytes::from(vec![0, 0, 0, 1, 0x65, 0xAA, 0, 0, 1, 0x41, 0xBB]);
        let base = frame.as_ptr() as usize;
        let mut s = AnnexBSplitter::new(BytesMut::from(frame));
        let first = s.next().unwrap().unwrap();
        let second = s.next().unwrap().unwrap();
        assert_eq!(first.full.as_ptr() as usize, base);
        assert_eq!(second.full.as_ptr() as usize, base + 6);
    }

    #[test]
    fn same_units_as_the_byte_reader_on_edge_cases() {
        let cases: Vec<Vec<u8>> = vec![
            vec![],
            vec![0],
            vec![0, 0],
            vec![0, 0, 1],
            vec![0, 0, 0, 1],
            vec![0, 0, 0, 0, 0, 1],
            vec![1, 0, 0, 1],
            vec![0, 1, 0, 0, 1, 7],
            vec![9, 9, 0, 0, 1, 0x65, 1, 2, 3],
            // Start codes back to back (empty units).
            vec![0, 0, 1, 0, 0, 1, 0, 0, 1],
            vec![0, 0, 0, 1, 0, 0, 0, 1, 0x41],
            vec![0, 0, 1, 0, 0, 0, 1],
            // Trailing zero bytes, at the end and before the next start code.
            vec![0, 0, 1, 0x65, 0, 0],
            vec![0, 0, 1, 0x65, 0, 0, 0],
            vec![0, 0, 1, 0x65, 0, 0, 0, 0, 1, 0x41],
            vec![0, 0, 1, 0x65, 0, 0, 0, 0, 0, 0, 1, 0x41, 0],
            // A unit that is only a start code at the very end.
            vec![0, 0, 1, 0x65, 0, 0, 0, 1],
            vec![0, 0, 1, 0x65, 1, 0, 0, 1],
            // Near misses.
            vec![0, 0, 2, 0, 1, 0, 0, 1, 0x67, 0, 2, 0, 0, 2],
            vec![0, 0, 0, 0, 0, 0],
            vec![1, 1, 1, 1],
        ];
        for case in &cases {
            assert_same(case);
        }
        // Every byte string of length up to 7 over {0, 1, 2}.
        for len in 0..=7u32 {
            for mut n in 0..3u32.pow(len) {
                let mut data = Vec::with_capacity(len as usize);
                for _ in 0..len {
                    data.push((n % 3) as u8);
                    n /= 3;
                }
                assert_same(&data);
            }
        }
    }

    #[test]
    fn same_units_as_the_byte_reader_on_random_frames() {
        // xorshift64*: deterministic, no extra dependency.
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = move || {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            state.wrapping_mul(0x2545_F491_4F6C_DD1D)
        };
        for _ in 0..3000 {
            let len = (next() % 400) as usize;
            let mut data = Vec::with_capacity(len + 8);
            while data.len() < len {
                match next() % 10 {
                    0 => data.extend_from_slice(&[0, 0, 1]),
                    1 => data.extend_from_slice(&[0, 0, 0, 1]),
                    2 | 3 => data.push(0),
                    4 => data.push(1),
                    _ => data.push((next() & 0xFF) as u8),
                }
            }
            assert_same(&data);
        }
    }
}

#[cfg(test)]
mod filler_tests {
    use super::without_filler;

    #[test]
    fn filler_data_is_taken_out_and_the_rest_is_untouched() {
        // H.264: SEI (6), a slice (1), filler (12) of 0xFF bytes, with both start codes.
        let sei = [0, 0, 0, 1, 0x06, 0x05, 0x01, 0x80];
        let slice = [0, 0, 1, 0x41, 0x9A, 0x00, 0x00, 0x03, 0x01, 0x22];
        let filler = [0, 0, 0, 1, 0x0C, 0xFF, 0xFF, 0xFF, 0x80];
        let frame: Vec<u8> = [&sei[..], &slice[..], &filler[..]].concat();
        assert_eq!(without_filler(frame.clone(), false), [&sei[..], &slice[..]].concat());
        // Filler first or in the middle.
        assert_eq!(without_filler([&filler[..], &sei[..], &filler[..], &slice[..]].concat(), false), [&sei[..], &slice[..]].concat());
        // Nothing to take out: the same bytes (an emulation prevention 00 00 03 is not a start code).
        let plain: Vec<u8> = [&sei[..], &slice[..]].concat();
        assert_eq!(without_filler(plain.clone(), false), plain);
        // The same bytes read as H.265 have no FD_NUT (type 38) in them.
        assert_eq!(without_filler(frame.clone(), true), frame);
        // H.265: a slice (type 1: header 0x02 0x01) and filler (type 38: header 0x4C 0x01).
        let hevc_slice = [0, 0, 0, 1, 0x02, 0x01, 0xD0, 0x10];
        let hevc_filler = [0, 0, 1, 0x4C, 0x01, 0xFF, 0xFF, 0x80];
        assert_eq!(without_filler([&hevc_slice[..], &hevc_filler[..]].concat(), true), hevc_slice.to_vec());
        // Odd input is left alone.
        assert_eq!(without_filler(vec![], false), Vec::<u8>::new());
        assert_eq!(without_filler(vec![0, 0, 1], false), vec![0, 0, 1]);
        assert_eq!(without_filler(vec![1, 2, 3, 4], false), vec![1, 2, 3, 4]);
    }
}
