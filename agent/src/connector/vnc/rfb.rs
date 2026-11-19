//! The RFB protocol (RFC 6143, "The Remote Framebuffer Protocol", version
//! 3.8), client side: the handshake (§7.1, §7.2, §7.3), the client's
//! messages (§7.5), the server's messages (§7.6) and the encodings this
//! client asks for (§7.7, §7.8). Section numbers are the RFC's.
//!
//! The pixel format is always the one this client sets (§7.4,
//! SetPixelFormat): 32 bits per pixel, depth 24, little-endian, true
//! colour, red at bit 16, green at 8, blue at 0. A pixel is then the bytes
//! B, G, R, X in memory: the picture's own layout (raw_video.rs), so Raw
//! rectangles are copied as they come, and a ZRLE CPIXEL (§7.7.6) is the
//! three bytes B, G, R.

use std::io;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// §7.7 and §7.8: the encodings asked for, the preferred first.
pub const ENCODING_RAW: i32 = 0;
pub const ENCODING_COPY_RECT: i32 = 1;
pub const ENCODING_ZRLE: i32 = 16;
/// §7.8.1 Cursor pseudo-encoding.
pub const ENCODING_CURSOR: i32 = -239;
/// §7.8.2 DesktopSize pseudo-encoding.
pub const ENCODING_DESKTOP_SIZE: i32 = -223;
/// ExtendedDesktopSize (-308): registered in RFC 6143 §8.4.1's table only by
/// number; its layout is the RFB community specification's
/// ("ExtendedDesktopSize Pseudo-encoding", rfbproto.rst of
/// github.com/rfbproto/rfbproto): rectangle x = reason, y = status, width
/// and height = the framebuffer's new size, then number-of-screens (u8),
/// 3 bytes of padding and 16 bytes per screen.
pub const ENCODING_EXTENDED_DESKTOP_SIZE: i32 = -308;

/// What this client asks for, in its order of preference (§7.5.2).
pub const ENCODINGS: [i32; 6] = [
    ENCODING_ZRLE,
    ENCODING_COPY_RECT,
    ENCODING_RAW,
    ENCODING_CURSOR,
    ENCODING_EXTENDED_DESKTOP_SIZE,
    ENCODING_DESKTOP_SIZE,
];

/// §7.2: security types.
pub const SECURITY_INVALID: u8 = 0;
pub const SECURITY_NONE: u8 = 1;
pub const SECURITY_VNC_AUTH: u8 = 2;

/// §7.4: PIXEL_FORMAT, 16 bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PixelFormat {
    pub bits_per_pixel: u8,
    pub depth: u8,
    pub big_endian: bool,
    pub true_colour: bool,
    pub red_max: u16,
    pub green_max: u16,
    pub blue_max: u16,
    pub red_shift: u8,
    pub green_shift: u8,
    pub blue_shift: u8,
}

impl PixelFormat {
    /// The one this client uses (see the module's notes).
    pub const BGRX: PixelFormat = PixelFormat {
        bits_per_pixel: 32,
        depth: 24,
        big_endian: false,
        true_colour: true,
        red_max: 255,
        green_max: 255,
        blue_max: 255,
        red_shift: 16,
        green_shift: 8,
        blue_shift: 0,
    };

    pub fn to_bytes(self) -> [u8; 16] {
        let mut b = [0u8; 16];
        b[0] = self.bits_per_pixel;
        b[1] = self.depth;
        b[2] = self.big_endian as u8;
        b[3] = self.true_colour as u8;
        b[4..6].copy_from_slice(&self.red_max.to_be_bytes());
        b[6..8].copy_from_slice(&self.green_max.to_be_bytes());
        b[8..10].copy_from_slice(&self.blue_max.to_be_bytes());
        b[10] = self.red_shift;
        b[11] = self.green_shift;
        b[12] = self.blue_shift;
        b
    }

    pub fn from_bytes(b: &[u8; 16]) -> PixelFormat {
        PixelFormat {
            bits_per_pixel: b[0],
            depth: b[1],
            big_endian: b[2] != 0,
            true_colour: b[3] != 0,
            red_max: u16::from_be_bytes([b[4], b[5]]),
            green_max: u16::from_be_bytes([b[6], b[7]]),
            blue_max: u16::from_be_bytes([b[8], b[9]]),
            red_shift: b[10],
            green_shift: b[11],
            blue_shift: b[12],
        }
    }
}

/// Why a connection did not get through the handshake, in words for the
/// person.
#[derive(Debug)]
pub enum HandshakeError {
    Io(io::Error),
    NotRfb,
    /// The server lists no type this client has (the types it lists).
    Unsupported(Vec<u8>),
    /// The server asks for a password and none is set.
    PasswordNeeded,
    /// §7.1.2 / §7.1.3: the server's own reason.
    Refused(String),
    WrongPassword(String),
}

impl std::fmt::Display for HandshakeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HandshakeError::Io(e) => write!(f, "The VNC server could not be reached ({e})"),
            HandshakeError::NotRfb => f.write_str("That address does not answer as a VNC server"),
            HandshakeError::Unsupported(types) => write!(
                f,
                "The VNC server asks for a kind of security Lunaris does not support ({}); only \"None\" and \"VNC password\" are supported. Allow one of them on the server",
                types.iter().map(|t| security_name(*t)).collect::<Vec<_>>().join(", ")
            ),
            HandshakeError::PasswordNeeded => f.write_str("The VNC server asks for a password: enter it in this computer's settings"),
            HandshakeError::Refused(reason) if reason.is_empty() => f.write_str("The VNC server refused the connection"),
            HandshakeError::Refused(reason) => write!(f, "The VNC server refused the connection: {reason}"),
            HandshakeError::WrongPassword(reason) if reason.is_empty() => f.write_str("The VNC password is not correct"),
            HandshakeError::WrongPassword(reason) => write!(f, "The VNC password is not correct ({reason})"),
        }
    }
}

impl std::error::Error for HandshakeError {}

impl From<io::Error> for HandshakeError {
    fn from(e: io::Error) -> Self {
        HandshakeError::Io(e)
    }
}

/// Names of the security types (RFC 6143 §8.1.2 registry; the ones after
/// 2 are only named, not supported).
pub fn security_name(t: u8) -> String {
    match t {
        1 => "None".into(),
        2 => "VNC Authentication".into(),
        5 => "RA2".into(),
        6 => "RA2ne".into(),
        16 => "Tight".into(),
        18 => "TLS".into(),
        19 => "VeNCrypt".into(),
        20 => "SASL".into(),
        22 => "xvp".into(),
        30 => "Apple Remote Desktop".into(),
        113 | 117 | 129 => "UltraVNC / RA2".into(),
        other => format!("type {other}"),
    }
}

/// What the server told at ServerInit (§7.3.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerInit {
    pub width: u16,
    pub height: u16,
    pub pixel_format: PixelFormat,
    pub name: String,
}

/// The protocol version both sides use (§7.1.1): 3.3, 3.7 or 3.8.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Version {
    V3_3,
    V3_7,
    V3_8,
}

/// Longest text read from the server (a reason, the desktop's name, cut
/// text): a guard against a length that would take all memory.
pub const MAX_TEXT: u32 = 1 << 20;

async fn read_u8<R: AsyncRead + Unpin>(r: &mut R) -> io::Result<u8> {
    r.read_u8().await
}

async fn read_string<R: AsyncRead + Unpin>(r: &mut R) -> io::Result<String> {
    let len = r.read_u32().await?;
    if len > MAX_TEXT {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "text too long"));
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf).await?;
    // §7.1.2: a reason string is ASCII; §7.3.2: the name is Latin-1 in
    // practice. Both read as Latin-1, which takes every byte.
    Ok(latin1_to_string(&buf))
}

pub fn latin1_to_string(bytes: &[u8]) -> String {
    bytes.iter().map(|b| *b as char).collect()
}

/// The text as Latin-1 (§7.5.6, §7.6.4), or `None` when it has a character
/// Latin-1 does not have.
pub fn string_to_latin1(text: &str) -> Option<Vec<u8>> {
    text.chars().map(|c| u8::try_from(u32::from(c)).ok()).collect()
}

/// §7.2.2 VNC Authentication: the 16-byte challenge encrypted with DES
/// (two blocks, ECB), the key being the password's first 8 bytes, padded
/// with zeros.
///
/// The RFC does not say so, but every VNC server takes the key with the
/// bits of each byte in reverse order (the DES code these servers share
/// numbers key bits the other way): libvncserver `rfbEncryptBytes`
/// (libvncserver/common/vncauth.c, which loads the key through d3des
/// `deskey` with its `bytebit` table) and TigerVNC
/// `common/rfb/d3des.c` do it. Read for the behaviour only (both are GPL;
/// nothing is copied); the test below checks the result against OpenSSL's
/// DES with the reversed key.
pub fn vnc_auth_response(challenge: &[u8; 16], password: &str) -> [u8; 16] {
    use des::cipher::{BlockCipherEncrypt, KeyInit};
    let mut key = [0u8; 8];
    for (i, b) in password.bytes().take(8).enumerate() {
        key[i] = b.reverse_bits();
    }
    let cipher = des::Des::new(&key.into());
    let mut out = *challenge;
    for block in out.chunks_exact_mut(8) {
        let mut b: [u8; 8] = block.try_into().unwrap();
        cipher.encrypt_block((&mut b).into());
        block.copy_from_slice(&b);
    }
    out
}

/// The handshake up to ServerInit (§7.1 to §7.3), the share flag set (the
/// computer's other viewers stay connected).
pub async fn handshake<S: AsyncRead + AsyncWrite + Unpin>(stream: &mut S, password: &str) -> Result<(Version, ServerInit), HandshakeError> {
    // §7.1.1 ProtocolVersion: "RFB xxx.yyy\n".
    let mut server_version = [0u8; 12];
    stream.read_exact(&mut server_version).await?;
    let version = parse_version(&server_version).ok_or(HandshakeError::NotRfb)?;
    let reply: &[u8; 12] = match version {
        Version::V3_8 => b"RFB 003.008\n",
        Version::V3_7 => b"RFB 003.007\n",
        Version::V3_3 => b"RFB 003.003\n",
    };
    stream.write_all(reply).await?;
    stream.flush().await?;

    // §7.1.2 Security Handshake.
    let chosen = if version >= Version::V3_7 {
        let count = read_u8(stream).await?;
        if count == 0 {
            return Err(HandshakeError::Refused(read_string(stream).await?));
        }
        let mut types = vec![0u8; count as usize];
        stream.read_exact(&mut types).await?;
        // A password given: VNC Authentication when offered; else None.
        let choice = if !password.is_empty() && types.contains(&SECURITY_VNC_AUTH) {
            SECURITY_VNC_AUTH
        } else if types.contains(&SECURITY_NONE) {
            SECURITY_NONE
        } else if types.contains(&SECURITY_VNC_AUTH) {
            return Err(HandshakeError::PasswordNeeded);
        } else {
            return Err(HandshakeError::Unsupported(types));
        };
        stream.write_u8(choice).await?;
        stream.flush().await?;
        choice
    } else {
        // Version 3.3: the server decides (u32).
        let t = stream.read_u32().await?;
        match t {
            0 => return Err(HandshakeError::Refused(read_string(stream).await?)),
            1 => SECURITY_NONE,
            2 if password.is_empty() => return Err(HandshakeError::PasswordNeeded),
            2 => SECURITY_VNC_AUTH,
            other => return Err(HandshakeError::Unsupported(vec![other.min(255) as u8])),
        }
    };

    if chosen == SECURITY_VNC_AUTH {
        // §7.2.2: 16-byte challenge, 16-byte response.
        let mut challenge = [0u8; 16];
        stream.read_exact(&mut challenge).await?;
        stream.write_all(&vnc_auth_response(&challenge, password)).await?;
        stream.flush().await?;
    }

    // §7.1.3 SecurityResult: always after VNC Authentication; after None
    // only from version 3.8 on.
    if chosen == SECURITY_VNC_AUTH || version == Version::V3_8 {
        let result = stream.read_u32().await?;
        if result != 0 {
            // A reason follows only from 3.8 on.
            let reason = if version == Version::V3_8 { read_string(stream).await.unwrap_or_default() } else { String::new() };
            return Err(if chosen == SECURITY_VNC_AUTH { HandshakeError::WrongPassword(reason) } else { HandshakeError::Refused(reason) });
        }
    }

    // §7.3.1 ClientInit: shared-flag = 1.
    stream.write_u8(1).await?;
    stream.flush().await?;
    // §7.3.2 ServerInit.
    let width = stream.read_u16().await?;
    let height = stream.read_u16().await?;
    let mut pf = [0u8; 16];
    stream.read_exact(&mut pf).await?;
    let name = read_string(stream).await?;
    Ok((version, ServerInit { width, height, pixel_format: PixelFormat::from_bytes(&pf), name }))
}

/// §7.1.1: "RFB 003.008\n" and the like. Versions above 3.8 are spoken as
/// 3.8, 3.4 to 3.6 as 3.3 (the RFC: "other versions ... should be treated
/// as 3.3"); 3.889 (Apple) as 3.8, as Apple's server expects.
pub fn parse_version(v: &[u8; 12]) -> Option<Version> {
    if &v[0..4] != b"RFB " || v[7] != b'.' || v[11] != b'\n' {
        return None;
    }
    let num = |s: &[u8]| std::str::from_utf8(s).ok()?.parse::<u32>().ok();
    let major = num(&v[4..7])?;
    let minor = num(&v[8..11])?;
    Some(match (major, minor) {
        (3, m) if m >= 8 => Version::V3_8,
        (3, 7) => Version::V3_7,
        (3, _) => Version::V3_3,
        (m, _) if m > 3 => Version::V3_8,
        _ => return None,
    })
}

// --- client to server (§7.5) ---

/// §7.5.1 SetPixelFormat.
pub fn set_pixel_format(pf: PixelFormat) -> Vec<u8> {
    let mut m = vec![0u8, 0, 0, 0];
    m.extend_from_slice(&pf.to_bytes());
    m
}

/// §7.5.2 SetEncodings.
pub fn set_encodings(encodings: &[i32]) -> Vec<u8> {
    let mut m = vec![2u8, 0];
    m.extend_from_slice(&(encodings.len() as u16).to_be_bytes());
    for e in encodings {
        m.extend_from_slice(&e.to_be_bytes());
    }
    m
}

/// §7.5.3 FramebufferUpdateRequest.
pub fn update_request(incremental: bool, x: u16, y: u16, w: u16, h: u16) -> Vec<u8> {
    let mut m = vec![3u8, incremental as u8];
    for v in [x, y, w, h] {
        m.extend_from_slice(&v.to_be_bytes());
    }
    m
}

/// §7.5.4 KeyEvent.
pub fn key_event(down: bool, keysym: u32) -> Vec<u8> {
    let mut m = vec![4u8, down as u8, 0, 0];
    m.extend_from_slice(&keysym.to_be_bytes());
    m
}

/// §7.5.5 PointerEvent.
pub fn pointer_event(mask: u8, x: u16, y: u16) -> Vec<u8> {
    let mut m = vec![5u8, mask];
    m.extend_from_slice(&x.to_be_bytes());
    m.extend_from_slice(&y.to_be_bytes());
    m
}

/// §7.5.6 ClientCutText (Latin-1).
pub fn client_cut_text(latin1: &[u8]) -> Vec<u8> {
    let mut m = vec![6u8, 0, 0, 0];
    m.extend_from_slice(&(latin1.len() as u32).to_be_bytes());
    m.extend_from_slice(latin1);
    m
}

// --- server to client (§7.6) ---

/// One rectangle of a FramebufferUpdate, decoded.
#[derive(Debug, PartialEq)]
pub enum Rect {
    /// BGRX pixels, `w * h * 4` bytes.
    Pixels { x: u16, y: u16, w: u16, h: u16, bgrx: Vec<u8> },
    CopyRect { x: u16, y: u16, w: u16, h: u16, src_x: u16, src_y: u16 },
    /// §7.8.1: the cursor's picture as RGBA (mask applied) and its hot spot.
    Cursor { hot_x: u16, hot_y: u16, w: u16, h: u16, rgba: Vec<u8> },
    /// §7.8.2 DesktopSize or ExtendedDesktopSize with status 0.
    Resize { w: u16, h: u16 },
    /// ExtendedDesktopSize with a non-zero status: an answer to a request of
    /// a client (this one sends none), nothing to do.
    Ignored,
}

/// A message from the server.
#[derive(Debug, PartialEq)]
pub enum ServerMessage {
    /// §7.6.1, its rectangles in order.
    Update(Vec<Rect>),
    /// §7.6.2 (this client uses true colour; read and dropped).
    ColourMap,
    /// §7.6.3.
    Bell,
    /// §7.6.4 (Latin-1).
    CutText(String),
}

/// The largest picture taken (a guard; RFB sizes are 16 bits anyway).
const MAX_PIXELS: usize = 16384 * 16384;

/// Reads one server message. `zlib` is the connection's ZRLE stream
/// (§7.7.6: one zlib stream for all ZRLE rectangles of a connection).
pub async fn read_message<R: AsyncRead + Unpin>(r: &mut R, zlib: &mut flate2::Decompress) -> io::Result<ServerMessage> {
    let kind = r.read_u8().await?;
    match kind {
        0 => {
            let _pad = r.read_u8().await?;
            let count = r.read_u16().await?;
            let mut rects = Vec::with_capacity(count as usize);
            for _ in 0..count {
                let x = r.read_u16().await?;
                let y = r.read_u16().await?;
                let w = r.read_u16().await?;
                let h = r.read_u16().await?;
                let encoding = r.read_i32().await?;
                rects.push(read_rect(r, zlib, x, y, w, h, encoding).await?);
            }
            Ok(ServerMessage::Update(rects))
        }
        1 => {
            let _pad = r.read_u8().await?;
            let _first = r.read_u16().await?;
            let n = r.read_u16().await?;
            let mut skip = vec![0u8; n as usize * 6];
            r.read_exact(&mut skip).await?;
            Ok(ServerMessage::ColourMap)
        }
        2 => Ok(ServerMessage::Bell),
        3 => {
            let mut pad = [0u8; 3];
            r.read_exact(&mut pad).await?;
            Ok(ServerMessage::CutText(read_string(r).await?))
        }
        other => Err(io::Error::new(io::ErrorKind::InvalidData, format!("unknown server message type {other}"))),
    }
}

async fn read_rect<R: AsyncRead + Unpin>(
    r: &mut R,
    zlib: &mut flate2::Decompress,
    x: u16,
    y: u16,
    w: u16,
    h: u16,
    encoding: i32,
) -> io::Result<Rect> {
    let pixels = w as usize * h as usize;
    if pixels > MAX_PIXELS {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "rectangle too large"));
    }
    match encoding {
        // §7.7.1 Raw.
        ENCODING_RAW => {
            let mut bgrx = vec![0u8; pixels * 4];
            r.read_exact(&mut bgrx).await?;
            Ok(Rect::Pixels { x, y, w, h, bgrx })
        }
        // §7.7.2 CopyRect.
        ENCODING_COPY_RECT => {
            let src_x = r.read_u16().await?;
            let src_y = r.read_u16().await?;
            Ok(Rect::CopyRect { x, y, w, h, src_x, src_y })
        }
        // §7.7.6 ZRLE.
        ENCODING_ZRLE => {
            let len = r.read_u32().await?;
            if len as usize > pixels * 4 + 64 * 1024 + 1024 {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "ZRLE data too long"));
            }
            let mut data = vec![0u8; len as usize];
            r.read_exact(&mut data).await?;
            let raw = inflate(zlib, &data)?;
            let bgrx = decode_zrle(&raw, w as usize, h as usize)?;
            Ok(Rect::Pixels { x, y, w, h, bgrx })
        }
        // §7.8.1 Cursor: w*h pixels, then a bitmask of ((w + 7) / 8) * h
        // bytes, 1 = the pixel is part of the cursor, most significant bit
        // first.
        ENCODING_CURSOR => {
            let mut px = vec![0u8; pixels * 4];
            r.read_exact(&mut px).await?;
            let row_bytes = (w as usize).div_ceil(8);
            let mut mask = vec![0u8; row_bytes * h as usize];
            r.read_exact(&mut mask).await?;
            let mut rgba = vec![0u8; pixels * 4];
            for row in 0..h as usize {
                for col in 0..w as usize {
                    let i = row * w as usize + col;
                    let on = mask[row * row_bytes + col / 8] & (0x80 >> (col % 8)) != 0;
                    // BGRX -> RGBA.
                    rgba[i * 4] = px[i * 4 + 2];
                    rgba[i * 4 + 1] = px[i * 4 + 1];
                    rgba[i * 4 + 2] = px[i * 4];
                    rgba[i * 4 + 3] = if on { 255 } else { 0 };
                }
            }
            Ok(Rect::Cursor { hot_x: x, hot_y: y, w, h, rgba })
        }
        // §7.8.2 DesktopSize: no data.
        ENCODING_DESKTOP_SIZE => Ok(Rect::Resize { w, h }),
        ENCODING_EXTENDED_DESKTOP_SIZE => {
            let screens = r.read_u8().await?;
            let mut skip = vec![0u8; 3 + screens as usize * 16];
            r.read_exact(&mut skip).await?;
            // y is the status: 0 = a change (of the server or another client).
            Ok(if y == 0 { Rect::Resize { w, h } } else { Rect::Ignored })
        }
        other => Err(io::Error::new(io::ErrorKind::InvalidData, format!("the server used encoding {other}, which was not asked for"))),
    }
}

fn inflate(zlib: &mut flate2::Decompress, data: &[u8]) -> io::Result<Vec<u8>> {
    let mut out = Vec::with_capacity(data.len() * 4);
    let mut input = data;
    loop {
        if out.capacity() - out.len() < 64 * 1024 {
            out.reserve(256 * 1024);
        }
        let before_in = zlib.total_in();
        let before_out = zlib.total_out();
        zlib.decompress_vec(input, &mut out, flate2::FlushDecompress::Sync)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("ZRLE zlib: {e}")))?;
        let used = (zlib.total_in() - before_in) as usize;
        let made = (zlib.total_out() - before_out) as usize;
        input = &input[used..];
        if input.is_empty() && out.len() < out.capacity() {
            return Ok(out);
        }
        if used == 0 && made == 0 {
            return Ok(out);
        }
        if out.len() > MAX_PIXELS * 4 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "ZRLE data too large"));
        }
    }
}

/// Reads from the decompressed ZRLE data.
struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Cursor<'_> {
    fn u8(&mut self) -> io::Result<u8> {
        let b = *self.data.get(self.pos).ok_or_else(short)?;
        self.pos += 1;
        Ok(b)
    }
    fn take(&mut self, n: usize) -> io::Result<&[u8]> {
        let s = self.data.get(self.pos..self.pos + n).ok_or_else(short)?;
        self.pos += n;
        Ok(s)
    }
    /// A CPIXEL: B, G, R (see the module's notes) -> BGRX.
    fn cpixel(&mut self) -> io::Result<[u8; 4]> {
        let p = self.take(3)?;
        Ok([p[0], p[1], p[2], 0])
    }
    /// §7.7.6 run length: bytes added while they are 255, plus one.
    fn run_length(&mut self) -> io::Result<usize> {
        let mut len = 1usize;
        loop {
            let b = self.u8()?;
            len += b as usize;
            if b != 255 {
                return Ok(len);
            }
        }
    }
}

fn short() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "ZRLE data ends early")
}

/// §7.7.6 ZRLE: 64x64 tiles, left to right, top to bottom; each tile starts
/// with its subencoding byte.
pub fn decode_zrle(data: &[u8], w: usize, h: usize) -> io::Result<Vec<u8>> {
    let mut out = vec![0u8; w * h * 4];
    let mut c = Cursor { data, pos: 0 };
    let mut ty = 0;
    while ty < h {
        let th = (h - ty).min(64);
        let mut tx = 0;
        while tx < w {
            let tw = (w - tx).min(64);
            let mut tile = vec![[0u8; 4]; tw * th];
            let sub = c.u8()?;
            match sub {
                // Raw CPIXELs.
                0 => {
                    for p in tile.iter_mut() {
                        *p = c.cpixel()?;
                    }
                }
                // One colour.
                1 => {
                    let p = c.cpixel()?;
                    tile.fill(p);
                }
                // Packed palette: 1 bit an index for 2 colours, 2 for 3-4,
                // 4 for 5-16; each row starts at a byte.
                2..=16 => {
                    let size = sub as usize;
                    let palette: Vec<[u8; 4]> = (0..size).map(|_| c.cpixel()).collect::<io::Result<_>>()?;
                    let bits = match size {
                        2 => 1,
                        3..=4 => 2,
                        _ => 4,
                    };
                    let row_bytes = (tw * bits).div_ceil(8);
                    for row in 0..th {
                        let bytes = c.take(row_bytes)?;
                        for col in 0..tw {
                            let bit = col * bits;
                            let byte = bytes[bit / 8];
                            let shift = 8 - bits - (bit % 8);
                            let index = (byte >> shift) as usize & ((1 << bits) - 1);
                            tile[row * tw + col] = *palette.get(index).ok_or_else(short)?;
                        }
                    }
                }
                // Plain RLE: (CPIXEL, run length) until the tile is full.
                128 => {
                    let mut i = 0;
                    while i < tile.len() {
                        let p = c.cpixel()?;
                        let len = c.run_length()?;
                        if i + len > tile.len() {
                            return Err(short());
                        }
                        tile[i..i + len].fill(p);
                        i += len;
                    }
                }
                // Palette RLE: palette of (sub - 128) colours, then indices;
                // an index with its top bit set is followed by a run length.
                130..=255 => {
                    let size = sub as usize - 128;
                    let palette: Vec<[u8; 4]> = (0..size).map(|_| c.cpixel()).collect::<io::Result<_>>()?;
                    let mut i = 0;
                    while i < tile.len() {
                        let b = c.u8()?;
                        let p = *palette.get((b & 0x7f) as usize).ok_or_else(short)?;
                        let len = if b & 0x80 != 0 { c.run_length()? } else { 1 };
                        if i + len > tile.len() {
                            return Err(short());
                        }
                        tile[i..i + len].fill(p);
                        i += len;
                    }
                }
                other => return Err(io::Error::new(io::ErrorKind::InvalidData, format!("ZRLE subencoding {other}"))),
            }
            for row in 0..th {
                let dst = ((ty + row) * w + tx) * 4;
                for col in 0..tw {
                    out[dst + col * 4..dst + col * 4 + 4].copy_from_slice(&tile[row * tw + col]);
                }
            }
            tx += 64;
        }
        ty += 64;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert_eq!(parse_version(b"RFB 003.008\n"), Some(Version::V3_8));
        assert_eq!(parse_version(b"RFB 003.007\n"), Some(Version::V3_7));
        assert_eq!(parse_version(b"RFB 003.003\n"), Some(Version::V3_3));
        assert_eq!(parse_version(b"RFB 003.005\n"), Some(Version::V3_3));
        assert_eq!(parse_version(b"RFB 003.889\n"), Some(Version::V3_8));
        assert_eq!(parse_version(b"RFB 004.001\n"), Some(Version::V3_8));
        assert_eq!(parse_version(b"SSH-2.0-xxx\n"), None);
    }

    /// The bit-reversed DES key. The expected response was computed apart
    /// from this code with OpenSSL (`openssl enc -des-ecb -nopad -K
    /// 0e86ceceeef64e26`, the key being each byte of "password" with its
    /// bits reversed) for the challenge 00 01 .. 0f.
    #[test]
    fn vnc_authentication_reverses_the_key_bits() {
        let challenge: [u8; 16] = std::array::from_fn(|i| i as u8);
        let response = vnc_auth_response(&challenge, "password");
        let expected: [u8; 16] = [
            0xb8, 0x66, 0x92, 0x41, 0x25, 0xc8, 0xee, 0xbb, 0x9d, 0xeb, 0xc1, 0xdb, 0x61, 0xc5, 0x38, 0xe2,
        ];
        assert_eq!(response, expected);
        // Only the first 8 characters count.
        assert_eq!(vnc_auth_response(&challenge, "password-and-more"), response);
    }

    #[test]
    fn latin1_only() {
        assert_eq!(string_to_latin1("Café"), Some(vec![b'C', b'a', b'f', 0xe9]));
        assert_eq!(string_to_latin1("Việt"), None);
        assert_eq!(latin1_to_string(&[0x48, 0xe9]), "Hé");
    }

    #[test]
    fn messages_are_laid_out_as_the_rfc_says() {
        assert_eq!(update_request(true, 1, 2, 3, 4), vec![3, 1, 0, 1, 0, 2, 0, 3, 0, 4]);
        assert_eq!(key_event(true, 0xff0d), vec![4, 1, 0, 0, 0, 0, 0xff, 0x0d]);
        assert_eq!(pointer_event(0x09, 300, 2), vec![5, 9, 1, 44, 0, 2]);
        assert_eq!(client_cut_text(b"hi"), vec![6, 0, 0, 0, 0, 0, 0, 2, b'h', b'i']);
        assert_eq!(set_encodings(&[16, -239]), vec![2, 0, 0, 2, 0, 0, 0, 16, 0xff, 0xff, 0xff, 0x11]);
        let pf = set_pixel_format(PixelFormat::BGRX);
        assert_eq!(pf, vec![0, 0, 0, 0, 32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 16, 8, 0, 0, 0, 0]);
    }

    #[test]
    fn zrle_tiles() {
        // A 66x2 rectangle: two tiles (64x2 and 2x2). The first is plain
        // RLE (one run of 100, one of 28), the second packed palette (2).
        let mut d = vec![128u8];
        d.extend_from_slice(&[1, 2, 3]);
        d.push(99); // 100
        d.extend_from_slice(&[4, 5, 6]);
        d.push(27); // 28
        d.push(2);
        d.extend_from_slice(&[7, 7, 7, 9, 9, 9]);
        d.push(0b0100_0000); // row 0: 0, 1
        d.push(0b1000_0000); // row 1: 1, 0
        let out = decode_zrle(&d, 66, 2).unwrap();
        let px = |x: usize, y: usize| &out[(y * 66 + x) * 4..(y * 66 + x) * 4 + 4];
        assert_eq!(px(0, 0), &[1, 2, 3, 0]);
        assert_eq!(px(35, 1), &[1, 2, 3, 0]); // pixel 99 of the tile
        assert_eq!(px(36, 1), &[4, 5, 6, 0]); // pixel 100
        assert_eq!(px(64, 0), &[7, 7, 7, 0]);
        assert_eq!(px(65, 0), &[9, 9, 9, 0]);
        assert_eq!(px(64, 1), &[9, 9, 9, 0]);
        // Palette RLE with a run, and a solid tile, and raw.
        let d = vec![130, 1, 1, 1, 2, 2, 2, 0x81, 2, 0x00];
        let out = decode_zrle(&d, 2, 2).unwrap();
        assert_eq!(&out[0..12], &[2, 2, 2, 0, 2, 2, 2, 0, 2, 2, 2, 0]);
        assert_eq!(&out[12..16], &[1, 1, 1, 0]);
        assert!(decode_zrle(&[1, 5, 5], 2, 2).is_err());
        assert!(decode_zrle(&[17], 2, 2).is_err());
    }
}
