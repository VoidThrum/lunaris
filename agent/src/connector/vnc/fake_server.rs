//! A VNC server inside the test process (RFC 6143, server side), for this
//! connector's tests: the handshake with a chosen version and security
//! types, VNC Authentication checked with the same DES rule, updates sent
//! from a script in answer to the client's requests, and every message of
//! the client recorded.

use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

use super::rfb;

/// A message the client sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClientMessage {
    SetPixelFormat(rfb::PixelFormat),
    SetEncodings(Vec<i32>),
    UpdateRequest { incremental: bool, x: u16, y: u16, w: u16, h: u16 },
    Key { down: bool, keysym: u32 },
    Pointer { mask: u8, x: u16, y: u16 },
    CutText(Vec<u8>),
}

#[derive(Clone)]
pub struct Config {
    pub version: &'static [u8; 12],
    pub security: Vec<u8>,
    pub password: String,
    pub width: u16,
    pub height: u16,
}

#[derive(Default)]
pub struct Log {
    pub messages: Vec<ClientMessage>,
    pub auth_ok: Option<bool>,
    pub shared_flag: Option<u8>,
    pub closed: bool,
}

pub struct FakeServer {
    pub port: u16,
    pub log: Arc<Mutex<Log>>,
    /// Updates (whole FramebufferUpdate messages) to send, one per request.
    pub updates: mpsc::UnboundedSender<Vec<u8>>,
    /// Bytes to send at once (ServerCutText, an unasked update).
    pub push: mpsc::UnboundedSender<Vec<u8>>,
}

impl FakeServer {
    pub async fn start(config: Config) -> FakeServer {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let log = Arc::new(Mutex::new(Log::default()));
        let (updates, updates_rx) = mpsc::unbounded_channel();
        let (push, push_rx) = mpsc::unbounded_channel();
        let task_log = log.clone();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            serve(stream, config, task_log, updates_rx, push_rx).await;
        });
        FakeServer { port, log, updates, push }
    }

    pub fn messages(&self) -> Vec<ClientMessage> {
        self.log.lock().unwrap().messages.clone()
    }

    /// Waits (up to 10 s) until `check` holds for the log.
    pub async fn wait(&self, what: &str, check: impl Fn(&Log) -> bool) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if check(&self.log.lock().unwrap()) {
                return;
            }
            if tokio::time::Instant::now() > deadline {
                panic!("fake VNC server: no {what}; messages: {:?}", self.messages());
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }
}

async fn serve(
    mut s: TcpStream,
    config: Config,
    log: Arc<Mutex<Log>>,
    mut updates: mpsc::UnboundedReceiver<Vec<u8>>,
    mut push: mpsc::UnboundedReceiver<Vec<u8>>,
) {
    let result: std::io::Result<()> = async {
        s.write_all(config.version).await?;
        let mut client_version = [0u8; 12];
        s.read_exact(&mut client_version).await?;
        let v38 = &client_version == b"RFB 003.008\n";
        let v33 = &client_version == b"RFB 003.003\n";
        let chosen = if v33 {
            let t = config.security[0];
            s.write_u32(t as u32).await?;
            if t == 0 {
                s.write_u32(7).await?;
                s.write_all(b"go away").await?;
                return Ok(());
            }
            t
        } else {
            s.write_u8(config.security.len() as u8).await?;
            if config.security.is_empty() {
                s.write_u32(7).await?;
                s.write_all(b"go away").await?;
                return Ok(());
            }
            s.write_all(&config.security).await?;
            s.read_u8().await?
        };
        let mut ok = true;
        if chosen == rfb::SECURITY_VNC_AUTH {
            let challenge: [u8; 16] = std::array::from_fn(|i| (i as u8).wrapping_mul(37).wrapping_add(11));
            s.write_all(&challenge).await?;
            let mut response = [0u8; 16];
            s.read_exact(&mut response).await?;
            ok = response == rfb::vnc_auth_response(&challenge, &config.password);
        }
        log.lock().unwrap().auth_ok = Some(ok);
        if chosen == rfb::SECURITY_VNC_AUTH || v38 {
            if ok {
                s.write_u32(0).await?;
            } else {
                s.write_u32(1).await?;
                if v38 {
                    s.write_u32(14).await?;
                    s.write_all(b"Authentication").await?;
                }
                return Ok(());
            }
        }
        let shared = s.read_u8().await?;
        log.lock().unwrap().shared_flag = Some(shared);
        // ServerInit with a pixel format of its own (the client sets its own).
        s.write_u16(config.width).await?;
        s.write_u16(config.height).await?;
        let mut pf = rfb::PixelFormat::BGRX;
        pf.red_shift = 0;
        pf.blue_shift = 16;
        s.write_all(&pf.to_bytes()).await?;
        s.write_u32(4).await?;
        s.write_all(b"fake").await?;

        let (mut r, mut w) = s.into_split();
        let (msgs_tx, mut msgs_rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            loop {
                match read_client_message(&mut r).await {
                    Ok(m) => {
                        if msgs_tx.send(m).is_err() {
                            return;
                        }
                    }
                    Err(_) => return,
                }
            }
        });
        let mut wanted = 0u32;
        loop {
            tokio::select! {
                m = msgs_rx.recv() => {
                    let Some(m) = m else { break };
                    if let ClientMessage::UpdateRequest { .. } = m {
                        wanted += 1;
                    }
                    log.lock().unwrap().messages.push(m);
                }
                bytes = push.recv() => {
                    if let Some(bytes) = bytes { w.write_all(&bytes).await?; }
                }
                update = updates.recv(), if wanted > 0 => {
                    if let Some(update) = update {
                        wanted -= 1;
                        w.write_all(&update).await?;
                    }
                }
            }
        }
        Ok(())
    }
    .await;
    let _ = result;
    log.lock().unwrap().closed = true;
}

async fn read_client_message(r: &mut tokio::net::tcp::OwnedReadHalf) -> std::io::Result<ClientMessage> {
    let kind = r.read_u8().await?;
    Ok(match kind {
        0 => {
            let mut b = [0u8; 19];
            r.read_exact(&mut b).await?;
            let pf: [u8; 16] = b[3..19].try_into().unwrap();
            ClientMessage::SetPixelFormat(rfb::PixelFormat::from_bytes(&pf))
        }
        2 => {
            r.read_u8().await?;
            let n = r.read_u16().await?;
            let mut list = Vec::new();
            for _ in 0..n {
                list.push(r.read_i32().await?);
            }
            ClientMessage::SetEncodings(list)
        }
        3 => {
            let incremental = r.read_u8().await? != 0;
            ClientMessage::UpdateRequest { incremental, x: r.read_u16().await?, y: r.read_u16().await?, w: r.read_u16().await?, h: r.read_u16().await? }
        }
        4 => {
            let down = r.read_u8().await? != 0;
            r.read_u16().await?;
            ClientMessage::Key { down, keysym: r.read_u32().await? }
        }
        5 => {
            let mask = r.read_u8().await?;
            ClientMessage::Pointer { mask, x: r.read_u16().await?, y: r.read_u16().await? }
        }
        6 => {
            let mut pad = [0u8; 3];
            r.read_exact(&mut pad).await?;
            let n = r.read_u32().await?;
            let mut text = vec![0u8; n as usize];
            r.read_exact(&mut text).await?;
            ClientMessage::CutText(text)
        }
        other => return Err(std::io::Error::other(format!("client message {other}"))),
    })
}

// --- what the server sends (§7.6, §7.7, §7.8) ---

/// A FramebufferUpdate of these rectangles (each already laid out: its
/// header and data).
pub fn update(rects: &[Vec<u8>]) -> Vec<u8> {
    let mut m = vec![0u8, 0];
    m.extend_from_slice(&(rects.len() as u16).to_be_bytes());
    for r in rects {
        m.extend_from_slice(r);
    }
    m
}

fn header(x: u16, y: u16, w: u16, h: u16, encoding: i32) -> Vec<u8> {
    let mut m = Vec::new();
    for v in [x, y, w, h] {
        m.extend_from_slice(&v.to_be_bytes());
    }
    m.extend_from_slice(&encoding.to_be_bytes());
    m
}

/// §7.7.1 Raw, the pixels BGRX.
pub fn raw(x: u16, y: u16, w: u16, h: u16, bgrx: &[u8]) -> Vec<u8> {
    let mut m = header(x, y, w, h, rfb::ENCODING_RAW);
    m.extend_from_slice(bgrx);
    m
}

/// §7.7.2 CopyRect.
pub fn copy_rect(x: u16, y: u16, w: u16, h: u16, src_x: u16, src_y: u16) -> Vec<u8> {
    let mut m = header(x, y, w, h, rfb::ENCODING_COPY_RECT);
    m.extend_from_slice(&src_x.to_be_bytes());
    m.extend_from_slice(&src_y.to_be_bytes());
    m
}

/// §7.7.6 ZRLE through the connection's zlib stream `z`: each tile as one
/// colour (1) when it is, else plain RLE (128).
pub fn zrle(z: &mut flate2::Compress, x: u16, y: u16, w: u16, h: u16, bgrx: &[u8]) -> Vec<u8> {
    let (w_, h_) = (w as usize, h as usize);
    let mut raw_tiles = Vec::new();
    let mut ty = 0;
    while ty < h_ {
        let th = (h_ - ty).min(64);
        let mut tx = 0;
        while tx < w_ {
            let tw = (w_ - tx).min(64);
            let mut px = Vec::with_capacity(tw * th);
            for row in 0..th {
                for col in 0..tw {
                    let i = ((ty + row) * w_ + tx + col) * 4;
                    px.push([bgrx[i], bgrx[i + 1], bgrx[i + 2]]);
                }
            }
            if px.iter().all(|p| *p == px[0]) {
                raw_tiles.push(1);
                raw_tiles.extend_from_slice(&px[0]);
            } else {
                raw_tiles.push(128);
                let mut i = 0;
                while i < px.len() {
                    let mut run = 1;
                    while i + run < px.len() && px[i + run] == px[i] {
                        run += 1;
                    }
                    raw_tiles.extend_from_slice(&px[i]);
                    let mut left = run - 1;
                    while left >= 255 {
                        raw_tiles.push(255);
                        left -= 255;
                    }
                    raw_tiles.push(left as u8);
                    i += run;
                }
            }
            tx += 64;
        }
        ty += 64;
    }
    let mut compressed = Vec::with_capacity(raw_tiles.len() + 1024);
    z.compress_vec(&raw_tiles, &mut compressed, flate2::FlushCompress::Sync).unwrap();
    while compressed.len() == compressed.capacity() {
        compressed.reserve(64 * 1024);
        z.compress_vec(&[], &mut compressed, flate2::FlushCompress::Sync).unwrap();
    }
    let mut m = header(x, y, w, h, rfb::ENCODING_ZRLE);
    m.extend_from_slice(&(compressed.len() as u32).to_be_bytes());
    m.extend_from_slice(&compressed);
    m
}

/// §7.8.1 Cursor: BGRX pixels and the mask (1 = shown).
pub fn cursor(hot_x: u16, hot_y: u16, w: u16, h: u16, bgrx: &[u8], mask: &[u8]) -> Vec<u8> {
    let mut m = header(hot_x, hot_y, w, h, rfb::ENCODING_CURSOR);
    m.extend_from_slice(bgrx);
    m.extend_from_slice(mask);
    m
}

/// §7.8.2 DesktopSize.
pub fn desktop_size(w: u16, h: u16) -> Vec<u8> {
    header(0, 0, w, h, rfb::ENCODING_DESKTOP_SIZE)
}

/// ExtendedDesktopSize with one screen (reason `x`, status `y`).
pub fn extended_desktop_size(reason: u16, status: u16, w: u16, h: u16) -> Vec<u8> {
    let mut m = header(reason, status, w, h, rfb::ENCODING_EXTENDED_DESKTOP_SIZE);
    m.extend_from_slice(&[1, 0, 0, 0]);
    m.extend_from_slice(&1u32.to_be_bytes());
    for v in [0u16, 0, w, h] {
        m.extend_from_slice(&v.to_be_bytes());
    }
    m.extend_from_slice(&0u32.to_be_bytes());
    m
}

/// §7.6.4 ServerCutText.
pub fn server_cut_text(latin1: &[u8]) -> Vec<u8> {
    let mut m = vec![3u8, 0, 0, 0];
    m.extend_from_slice(&(latin1.len() as u32).to_be_bytes());
    m.extend_from_slice(latin1);
    m
}
