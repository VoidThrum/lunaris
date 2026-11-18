//! AV1 out of RTP: the packets of one frame become one temporal unit, the
//! way a decoder reads it (every OBU with its size field).
//!
//! The payload format is the AV1 RTP specification's (aomediacodec.github.io
//! /av1-rtp-spec, 4.2-4.4): an aggregation header `Z Y W W N - - -`, then
//! OBU elements. `Z`: the first element continues an OBU of the packet
//! before. `Y`: the last element is continued in the next packet. `W`: the
//! number of elements, each but the last with a LEB128 length before it; 0 =
//! every element has a length. The RTP marker bit is on the last packet of a
//! frame, and all packets of a frame have one RTP timestamp.
//!
//! What a frame is put together as is what libwebrtc's
//! video_rtp_depacketizer_av1.cc `AssembleFrame` makes: the OBUs of all the
//! frame's packets in order, each with `obu_has_size_field` set and its size
//! written. FFmpeg's own `av1` decoder (the one that decodes on a GPU) reads
//! a packet as such a unit; libdav1d takes it as well.
//!
//! A frame of which a packet is missing is not given out at all: the
//! decoder would otherwise get a frame header without its tiles.

/// An OBU as it travels in RTP (no size field: av1-rtp-spec 4.2, "OBUs
/// SHOULD NOT have the obu_has_size_field set"), written the way a decoder
/// reads a stream of them: with `obu_has_size_field` and the size (LEB128)
/// after the header. libwebrtc video_rtp_depacketizer_av1.cc
/// `AssembleFrame` does the same for each OBU of a frame. An OBU that has
/// its size already is passed as it is. None for what is not an OBU.
pub fn av1_obu_with_size(obu: &[u8]) -> Option<Vec<u8>> {
    const HAS_EXTENSION: u8 = 0b0000_0100;
    const HAS_SIZE: u8 = 0b0000_0010;
    let header = *obu.first()?;
    if header & 0x80 != 0 {
        return None; // obu_forbidden_bit
    }
    if header & HAS_SIZE != 0 {
        return Some(obu.to_vec());
    }
    let header_len = if header & HAS_EXTENSION != 0 { 2 } else { 1 };
    if obu.len() < header_len {
        return None;
    }
    let payload = &obu[header_len..];
    let mut out = Vec::with_capacity(obu.len() + 5);
    out.push(header | HAS_SIZE);
    out.extend_from_slice(&obu[1..header_len]);
    let mut size = payload.len();
    loop {
        let byte = (size & 0x7f) as u8;
        size >>= 7;
        if size == 0 {
            out.push(byte);
            break;
        }
        out.push(byte | 0x80);
    }
    out.extend_from_slice(payload);
    Some(out)
}

fn read_leb128(data: &[u8], offset: &mut usize) -> Option<usize> {
    let mut value = 0usize;
    let mut shift = 0;
    while *offset < data.len() {
        let byte = data[*offset];
        *offset += 1;
        value |= ((byte & 0x7f) as usize) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
        shift += 7;
        if shift >= 35 {
            return None;
        }
    }
    None
}

/// The OBU elements of one RTP payload (after the aggregation header), in
/// order; None when the lengths do not fit the payload.
fn elements(payload: &[u8], w: u8) -> Option<Vec<&[u8]>> {
    let mut out = Vec::new();
    let mut offset = 1;
    if w == 0 {
        while offset < payload.len() {
            let size = read_leb128(payload, &mut offset)?;
            let end = offset.checked_add(size).filter(|end| *end <= payload.len())?;
            out.push(&payload[offset..end]);
            offset = end;
        }
    } else {
        for index in 0..w {
            if index + 1 < w {
                let size = read_leb128(payload, &mut offset)?;
                let end = offset.checked_add(size).filter(|end| *end <= payload.len())?;
                out.push(&payload[offset..end]);
                offset = end;
            } else {
                // The last element takes the rest and has no length.
                if offset >= payload.len() {
                    return None;
                }
                out.push(&payload[offset..]);
            }
        }
    }
    Some(out)
}

/// Puts the packets of a frame together (see the top of this file).
#[derive(Default)]
pub struct Av1Assembler {
    /// The whole OBUs of the frame so far, each with its size.
    unit: Vec<u8>,
    /// The OBU that is still coming in pieces.
    obu: Vec<u8>,
    in_obu: bool,
    /// The frame in hand cannot be whole any more.
    broken: bool,
    /// Of the packet before: its number, whether it ended a frame, its time.
    last_seq: Option<u64>,
    last_marker: bool,
    timestamp: Option<u32>,
    /// Packets were dropped outside (`reset`).
    gap: bool,
}

impl Av1Assembler {
    /// Packets were lost or thrown away: what is in hand is dropped, and
    /// the frame the next packet belongs to is not trusted.
    pub fn reset(&mut self) {
        self.unit.clear();
        self.obu.clear();
        self.in_obu = false;
        self.gap = true;
    }

    /// One RTP packet, in order: its sequence number (extended, so that it
    /// only grows), RTP timestamp, marker bit and payload. Gives the frame's
    /// temporal unit when this packet completes one.
    pub fn push(&mut self, seq: u64, timestamp: u32, marker: bool, payload: &[u8]) -> Option<Vec<u8>> {
        let first_ever = self.last_seq.is_none() && !self.gap;
        let contiguous = !self.gap && self.last_seq.is_none_or(|last| seq == last.wrapping_add(1));
        self.gap = false;
        if !contiguous {
            self.broken = true;
        }
        if self.timestamp != Some(timestamp) {
            // Another frame begins. What is left of the one before never
            // got its last packet. This one is whole only if nothing is
            // missing between that frame's end and this packet.
            self.unit.clear();
            self.obu.clear();
            self.in_obu = false;
            self.broken = !(contiguous && (first_ever || self.last_marker));
        }
        self.last_seq = Some(seq);
        self.last_marker = marker;
        self.timestamp = Some(timestamp);

        if !self.broken && !self.take(payload) {
            self.broken = true;
        }
        if !marker {
            return None;
        }
        let whole = !self.broken && !self.in_obu && !self.unit.is_empty();
        let unit = std::mem::take(&mut self.unit);
        self.obu.clear();
        self.in_obu = false;
        // The next packet begins a frame of its own; whether it is whole is
        // decided there.
        self.timestamp = None;
        self.broken = false;
        whole.then_some(unit)
    }

    /// Adds a payload's OBU elements; false when they do not fit what came before.
    fn take(&mut self, payload: &[u8]) -> bool {
        let Some(&header) = payload.first() else { return false };
        let z = header & 0x80 != 0;
        let y = header & 0x40 != 0;
        let w = (header & 0x30) >> 4;
        let Some(elements) = elements(payload, w) else { return false };
        let count = elements.len();
        for (index, element) in elements.into_iter().enumerate() {
            if index == 0 && z {
                // The rest of an OBU: its beginning must be here.
                if !self.in_obu {
                    return false;
                }
            } else {
                // A new OBU while one is unfinished: its end was lost.
                if self.in_obu {
                    return false;
                }
                self.in_obu = true;
            }
            self.obu.extend_from_slice(element);
            if index + 1 == count && y {
                continue; // goes on in the next packet
            }
            let Some(obu) = av1_obu_with_size(&self.obu) else { return false };
            self.unit.extend_from_slice(&obu);
            self.obu.clear();
            self.in_obu = false;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An OBU_FRAME (type 6) without a size field, `len` payload bytes of `fill`.
    fn frame_obu(len: usize, fill: u8) -> Vec<u8> {
        let mut obu = vec![0x30];
        obu.extend(std::iter::repeat(fill).take(len));
        obu
    }

    /// A sequence header OBU (type 1) without a size field.
    fn sequence_obu() -> Vec<u8> {
        vec![0x08, 1, 2, 3]
    }

    fn sized(obus: &[&[u8]]) -> Vec<u8> {
        obus.iter().flat_map(|obu| av1_obu_with_size(obu).unwrap()).collect()
    }

    fn header(z: bool, y: bool, w: u8) -> u8 {
        (z as u8) << 7 | (y as u8) << 6 | w << 4
    }

    #[test]
    fn one_packet_with_counted_elements() {
        // W=2: the first element has a length, the last takes the rest.
        let (seq, frame) = (sequence_obu(), frame_obu(20, 7));
        let mut payload = vec![header(false, false, 2), seq.len() as u8];
        payload.extend(&seq);
        payload.extend(&frame);
        let mut a = Av1Assembler::default();
        assert_eq!(a.push(10, 9000, true, &payload), Some(sized(&[&seq, &frame])));
        // W=1: one element, no length.
        let mut payload = vec![header(false, false, 1)];
        payload.extend(&frame);
        assert_eq!(a.push(11, 12000, true, &payload), Some(sized(&[&frame])));
        // W=3.
        let other = frame_obu(5, 9);
        let mut payload = vec![header(false, false, 3), seq.len() as u8];
        payload.extend(&seq);
        payload.push(frame.len() as u8);
        payload.extend(&frame);
        payload.extend(&other);
        assert_eq!(a.push(12, 15000, true, &payload), Some(sized(&[&seq, &frame, &other])));
    }

    #[test]
    fn every_element_with_a_length() {
        // W=0, and a length of two bytes (200 = C8 01).
        let (seq, frame) = (sequence_obu(), frame_obu(199, 7));
        let mut payload = vec![header(false, false, 0), seq.len() as u8];
        payload.extend(&seq);
        payload.extend([0xC8, 0x01]);
        payload.extend(&frame);
        let mut a = Av1Assembler::default();
        assert_eq!(a.push(1, 0, true, &payload), Some(sized(&[&seq, &frame])));
        // A length that goes past the payload: nothing, not a part.
        let mut bad = vec![header(false, false, 0), 50];
        bad.extend(&seq);
        assert_eq!(a.push(2, 3000, true, &bad), None);
    }

    #[test]
    fn an_obu_in_pieces_across_packets() {
        let (seq, frame) = (sequence_obu(), frame_obu(300, 5));
        let (a_part, rest) = frame.split_at(100);
        let (b_part, c_part) = rest.split_at(100);
        // Packet 1: the sequence header whole, the frame's beginning (Y).
        let mut p1 = vec![header(false, true, 2), seq.len() as u8];
        p1.extend(&seq);
        p1.extend(a_part);
        // Packet 2: the middle (Z and Y). Packet 3: the end (Z), marker.
        let mut p2 = vec![header(true, true, 1)];
        p2.extend(b_part);
        let mut p3 = vec![header(true, false, 1)];
        p3.extend(c_part);
        let mut a = Av1Assembler::default();
        assert_eq!(a.push(100, 777, false, &p1), None);
        assert_eq!(a.push(101, 777, false, &p2), None);
        assert_eq!(a.push(102, 777, true, &p3), Some(sized(&[&seq, &frame])));
        // The same frame again later, with its middle lost: nothing comes
        // out, and the frame after it does (it begins after a marker).
        assert_eq!(a.push(103, 900, false, &p1), None);
        assert_eq!(a.push(105, 900, true, &p3), None);
        assert_eq!(a.push(106, 1000, false, &p1), None);
        assert_eq!(a.push(107, 1000, false, &p2), None);
        assert_eq!(a.push(108, 1000, true, &p3), Some(sized(&[&seq, &frame])));
    }

    #[test]
    fn a_frame_whose_first_or_last_packet_is_lost() {
        let frame = frame_obu(30, 1);
        let mut whole = vec![header(false, false, 1)];
        whole.extend(&frame);
        let mut begin = vec![header(false, true, 1)];
        begin.extend(&frame[..10]);
        let mut end = vec![header(true, false, 1)];
        end.extend(&frame[10..]);

        let mut a = Av1Assembler::default();
        assert!(a.push(1, 100, true, &whole).is_some());
        // The first packet (seq 2) lost: the rest of that frame is dropped,
        // also when what is left would start with a whole OBU.
        assert_eq!(a.push(3, 200, true, &end), None);
        assert!(a.push(4, 300, true, &whole).is_some());
        assert_eq!(a.push(6, 400, true, &whole), None, "seq 5 may have been this frame's first packet");
        assert!(a.push(7, 500, true, &whole).is_some());
        // The last packet (with the marker) lost: the next frame begins
        // without a marker before it. It is dropped as well: its own first
        // packet may be among the lost.
        assert_eq!(a.push(8, 600, false, &begin), None);
        assert_eq!(a.push(10, 700, false, &begin), None);
        assert_eq!(a.push(11, 700, true, &end), None);
        assert!(a.push(12, 800, true, &whole).is_some());
        // A marker that never came although no packet is missing (the
        // timestamp changes): the unfinished frame is dropped, and so is
        // the one that follows without a frame end before it.
        assert_eq!(a.push(13, 900, false, &begin), None);
        assert_eq!(a.push(14, 1000, true, &whole), None);
        assert!(a.push(15, 1100, true, &whole).is_some());
    }

    #[test]
    fn a_continuation_without_its_beginning_and_a_beginning_without_its_end() {
        let frame = frame_obu(30, 1);
        let mut begin = vec![header(false, true, 1)];
        begin.extend(&frame[..10]);
        let mut end = vec![header(true, false, 1)];
        end.extend(&frame[10..]);
        let mut whole = vec![header(false, false, 1)];
        whole.extend(&frame);
        // The very first packet of a session continues an OBU nobody saw.
        let mut a = Av1Assembler::default();
        assert_eq!(a.push(1, 100, true, &end), None);
        assert!(a.push(2, 200, true, &whole).is_some());
        // An OBU announced as going on (Y), then a new one instead (no Z).
        assert_eq!(a.push(3, 300, false, &begin), None);
        assert_eq!(a.push(4, 300, true, &whole), None);
        assert!(a.push(5, 400, true, &whole).is_some());
        // The frame ends (marker) inside an OBU.
        assert_eq!(a.push(6, 500, true, &begin), None);
        assert!(a.push(7, 600, true, &whole).is_some());
    }

    #[test]
    fn packets_thrown_away_outside() {
        let frame = frame_obu(30, 1);
        let mut begin = vec![header(false, true, 1)];
        begin.extend(&frame[..10]);
        let mut end = vec![header(true, false, 1)];
        end.extend(&frame[10..]);
        let mut whole = vec![header(false, false, 1)];
        whole.extend(&frame);
        let mut a = Av1Assembler::default();
        assert_eq!(a.push(1, 100, false, &begin), None);
        // The receiver's queue was flushed.
        a.reset();
        assert_eq!(a.push(50, 100, true, &end), None);
        assert!(a.push(51, 200, true, &whole).is_some());
        // Right at the start of a session too.
        let mut a = Av1Assembler::default();
        a.reset();
        assert_eq!(a.push(7, 100, true, &whole), None);
        assert!(a.push(8, 200, true, &whole).is_some());
    }

    #[test]
    fn empty_and_broken_payloads() {
        let mut a = Av1Assembler::default();
        assert_eq!(a.push(1, 100, true, &[]), None);
        // Only the aggregation header.
        assert_eq!(a.push(2, 200, true, &[header(false, false, 1)]), None);
        // An element that is not an OBU (forbidden bit).
        assert_eq!(a.push(3, 300, true, &[header(false, false, 1), 0x80, 1]), None);
        let mut whole = vec![header(false, false, 1)];
        whole.extend(frame_obu(4, 1));
        assert!(a.push(4, 400, true, &whole).is_some());
    }

    /// What the agent's payloader makes of frames (rtc's `Av1Payloader`,
    /// which `LunarisAv1Payloader` wraps) comes back as those frames.
    #[test]
    fn frames_of_the_agents_payloader_come_back() {
        use rtc::rtp::packetizer::Payloader;
        let mut payloader = rtc::rtp::codec::av1::Av1Payloader::default();
        let mut a = Av1Assembler::default();
        let mut seq = 0u64;
        for (index, size) in [40usize, 1500, 5000, 20_000].into_iter().enumerate() {
            // Low-overhead input, as an encoder writes it: OBUs with sizes.
            let sequence = sequence_obu();
            let frame = frame_obu(size, index as u8 + 1);
            let unit = sized(&[&sequence, &frame]);
            let payloads = payloader.payload(1200, &bytes::Bytes::from(unit.clone())).expect("packets");
            assert!(!payloads.is_empty());
            let count = payloads.len();
            let mut out = None;
            for (i, payload) in payloads.iter().enumerate() {
                assert!(out.is_none());
                out = a.push(seq, index as u32 * 3000, i + 1 == count, payload);
                seq += 1;
            }
            assert_eq!(out, Some(unit), "a frame of {} bytes in {} packets", size, count);
        }
    }
}
