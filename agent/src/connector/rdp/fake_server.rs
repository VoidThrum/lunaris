//! An RDP server inside the test process, for this connector's tests. The
//! connection sequence is IronRDP's own server side (ironrdp-acceptor 0.12:
//! X.224, TLS, CredSSP with NTLM, the rest of the sequence); what it sends
//! afterwards is laid out as IronRDP's server does it (github.com/Devolutions/
//! IronRDP, crates/ironrdp-server at abfc97b: `capabilities.rs` for the
//! capability sets, `encoder/bitmap.rs` for planar bitmaps in 64x64 tiles,
//! `encoder/fast_path.rs` for fast-path update fragments, `encoder/mod.rs`
//! `rgba_pointer` and `encoder/pointer_mask.rs` for pointers, `server.rs`
//! `handle_io_channel_data` for the client's Shutdown Request). Those parts
//! are copied from IronRDP (MIT OR Apache-2.0) and cut down to what the
//! tests need.

use std::sync::{Arc, Mutex};

use ironrdp_acceptor::{Acceptor, BeginResult};
use ironrdp_connector::{DesktopSize, ServerName};
use ironrdp_core::{decode, encode_vec, Encode as _, WriteCursor};
use ironrdp_graphics::rdp6::{BgrAChannels, BitmapStreamEncoder};
use ironrdp_pdu::bitmap::{BitmapData, BitmapUpdateData, CompressedDataHeader, Compression};
use ironrdp_pdu::fast_path::{EncryptionFlags, FastPathHeader, FastPathUpdatePdu, Fragmentation, UpdateCode};
use ironrdp_pdu::geometry::InclusiveRectangle;
use ironrdp_pdu::input::fast_path::{FastPathInput, FastPathInputEvent};
use ironrdp_pdu::nego::SecurityProtocol;
use ironrdp_pdu::pointer::{ColorPointerAttribute, Point16, PointerAttribute};
use ironrdp_pdu::rdp::capability_sets::{self, GeneralExtraFlags};
use ironrdp_pdu::rdp::client_info::Credentials;
use ironrdp_pdu::x224::X224;
use ironrdp_pdu::{mcs, Action};
use ironrdp_tokio::{split_tokio_framed, FramedWrite, TokioFramed};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

/// What the server is told to send.
pub enum Out {
    /// BGRX pixels.
    Bitmap { x: u16, y: u16, w: u16, h: u16, bgrx: Vec<u8> },
    /// RGBA pointer (as ironrdp-server `RGBAPointer`).
    Pointer { w: u16, h: u16, hot_x: u16, hot_y: u16, rgba: Vec<u8> },
    PointerPosition { x: u16, y: u16 },
    HidePointer,
    DefaultPointer,
    /// The server's clipboard text changes (it offers CF_UNICODETEXT).
    Clipboard(String),
}

#[derive(Default)]
pub struct Log {
    pub accepted: bool,
    pub failed: Option<String>,
    pub input: Vec<FastPathInputEvent>,
    pub shutdown_requested: bool,
    pub closed: bool,
    pub desktop: Option<(u16, u16)>,
    pub tls_connections: u32,
    /// The client's clipboard text, as the server fetched it.
    pub clipboard_from_client: Option<String>,
}

pub struct Config {
    pub security: SecurityProtocol,
    pub username: String,
    pub password: String,
    pub size: (u16, u16),
    /// How many connections to take (the set-up and a session are two).
    pub connections: usize,
    /// Offer the clipboard channel (CLIPRDR, ironrdp-cliprdr's server side).
    pub clipboard: bool,
}

pub struct FakeRdp {
    pub port: u16,
    pub cert_der: Vec<u8>,
    pub log: Arc<Mutex<Log>>,
    pub out: mpsc::UnboundedSender<Out>,
}

/// A certificate and key for the server (rcgen, as `direct.rs` makes its own).
fn certificate() -> (Vec<u8>, Vec<u8>) {
    let key = rcgen::KeyPair::generate().unwrap();
    let cert = rcgen::CertificateParams::new(vec!["localhost".to_string()]).unwrap().self_signed(&key).unwrap();
    (cert.der().to_vec(), key.serialize_der())
}

/// ironrdp-server `capabilities.rs`, with its options' defaults.
fn capabilities(size: (u16, u16)) -> Vec<capability_sets::CapabilitySet> {
    vec![
        capability_sets::CapabilitySet::General(capability_sets::General {
            extra_flags: GeneralExtraFlags::FASTPATH_OUTPUT_SUPPORTED,
            refresh_rect_support: true,
            suppress_output_support: true,
            ..Default::default()
        }),
        capability_sets::CapabilitySet::Bitmap(capability_sets::Bitmap {
            pref_bits_per_pix: 32,
            desktop_width: size.0,
            desktop_height: size.1,
            desktop_resize_flag: true,
            drawing_flags: capability_sets::BitmapDrawingFlags::empty(),
        }),
        capability_sets::CapabilitySet::Order(capability_sets::Order::new(
            capability_sets::OrderFlags::empty(),
            capability_sets::OrderSupportExFlags::empty(),
            2048,
            224,
        )),
        capability_sets::CapabilitySet::SurfaceCommands(capability_sets::SurfaceCommands { flags: capability_sets::CmdFlags::all() }),
        capability_sets::CapabilitySet::Pointer(capability_sets::Pointer { color_pointer_cache_size: 2048, pointer_cache_size: 2048 }),
        capability_sets::CapabilitySet::Input(capability_sets::Input {
            input_flags: capability_sets::InputFlags::SCANCODES
                | capability_sets::InputFlags::MOUSEX
                | capability_sets::InputFlags::FASTPATH_INPUT
                | capability_sets::InputFlags::UNICODE
                | capability_sets::InputFlags::FASTPATH_INPUT_2
                | capability_sets::InputFlags::TS_MOUSE_HWHEEL,
            keyboard_layout: 0,
            keyboard_type: None,
            keyboard_subtype: 0,
            keyboard_function_key: 128,
            keyboard_ime_filename: "".into(),
        }),
        capability_sets::CapabilitySet::VirtualChannel(capability_sets::VirtualChannel {
            flags: capability_sets::VirtualChannelFlags::NO_COMPRESSION,
            chunk_size: None,
        }),
        capability_sets::CapabilitySet::MultiFragmentUpdate(capability_sets::MultifragmentUpdate { max_request_size: 8 * 1024 * 1024 }),
        capability_sets::CapabilitySet::BitmapCodecs(capability_sets::BitmapCodecs(Vec::new())),
    ]
}

/// ironrdp-server `encoder/fast_path.rs` `UpdateFragmenter`: the update in
/// fast-path PDUs of at most 16 374 bytes of data.
fn fast_path_update(code: UpdateCode, data: &[u8]) -> Vec<Vec<u8>> {
    const MAX: usize = 16_374;
    let chunks: Vec<&[u8]> = if data.is_empty() { vec![&[][..]] } else { data.chunks(MAX).collect() };
    let n = chunks.len();
    chunks
        .into_iter()
        .enumerate()
        .map(|(i, chunk)| {
            let fragmentation = match (n, i) {
                (1, _) => Fragmentation::Single,
                (_, 0) => Fragmentation::First,
                (_, i) if i == n - 1 => Fragmentation::Last,
                _ => Fragmentation::Next,
            };
            let update = FastPathUpdatePdu { fragmentation, update_code: code, compression_flags: None, compression_type: None, data: chunk };
            let header = FastPathHeader::new(EncryptionFlags::empty(), update.size());
            let mut buf = vec![0u8; header.size() + update.size()];
            let mut cursor = WriteCursor::new(&mut buf);
            header.encode(&mut cursor).unwrap();
            update.encode(&mut cursor).unwrap();
            let len = cursor.pos();
            buf.truncate(len);
            buf
        })
        .collect()
}

/// ironrdp-server `encoder/bitmap.rs` `BitmapEncoder::encode` for BGRX.
fn bitmap_update(x: u16, y: u16, width: u16, height: u16, bgrx: &[u8]) -> Vec<u8> {
    const TILE: u16 = 64;
    let stride = width as usize * 4;
    let tiles_x = width.div_ceil(TILE);
    let tiles_y = height.div_ceil(TILE);
    let mut out = vec![0u8; bgrx.len() * 2 + 4096];
    let mut cursor = WriteCursor::new(&mut out);
    BitmapUpdateData::encode_header((tiles_x * tiles_y) as u16, &mut cursor).unwrap();
    let mut buffer = vec![0u8; u16::MAX as usize];
    for ty in 0..tiles_y {
        let top = ty * TILE;
        let tile_h = TILE.min(height - top);
        for tx in 0..tiles_x {
            let left = tx * TILE;
            let tile_w = TILE.min(width - left);
            let enc_w = tile_w.next_multiple_of(4);
            let row_len = tile_w as usize * 4;
            let col = left as usize * 4;
            let pad = (enc_w - tile_w) as usize;
            // Planar bitmaps are bottom-up.
            let pixels = (top as usize..(top + tile_h) as usize).rev().flat_map(|y| {
                let row = &bgrx[y * stride + col..y * stride + col + row_len];
                let last = &row[row_len - 4..];
                row.chunks(4).chain(std::iter::repeat_n(last, pad))
            });
            let len = BitmapStreamEncoder::new(enc_w as usize, tile_h as usize)
                .encode_pixels_stream::<_, BgrAChannels>(pixels, &mut buffer, true)
                .unwrap();
            let data = BitmapData {
                rectangle: InclusiveRectangle { left: x + left, top: y + top, right: x + left + tile_w - 1, bottom: y + top + tile_h - 1 },
                width: enc_w,
                height: tile_h,
                bits_per_pixel: 32,
                compression_flags: Compression::BITMAP_COMPRESSION,
                compressed_data_header: Some(CompressedDataHeader {
                    main_body_size: len as u16,
                    scan_width: enc_w,
                    uncompressed_size: (tile_h as usize * enc_w as usize * 4) as u16,
                }),
                bitmap_data: &buffer[..len],
            };
            data.encode(&mut cursor).unwrap();
        }
    }
    let len = cursor.pos();
    out.truncate(len);
    out
}

/// ironrdp-server `encoder/mod.rs` `rgba_pointer` with
/// `pointer_mask.rs` `and_mask_from_alpha`.
fn pointer_update(w: u16, h: u16, hot_x: u16, hot_y: u16, rgba: &[u8]) -> Vec<u8> {
    let stride = (w as usize).div_ceil(16) * 2;
    let mut and_mask = vec![0u8; stride * h as usize];
    for (row_idx, row) in rgba.chunks_exact(w as usize * 4).take(h as usize).enumerate() {
        for (col_idx, pixel) in row.chunks_exact(4).enumerate() {
            if pixel[3] == 0 {
                and_mask[row_idx * stride + col_idx / 8] |= 0x80 >> (col_idx % 8);
            }
        }
    }
    let ptr = PointerAttribute {
        xor_bpp: 32,
        color_pointer: ColorPointerAttribute { cache_index: 0, hot_spot: Point16 { x: hot_x, y: hot_y }, width: w, height: h, xor_mask: rgba, and_mask: &and_mask },
    };
    encode_vec(&ptr).unwrap()
}

impl FakeRdp {
    pub async fn start(config: Config) -> FakeRdp {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let (cert_der, key_der) = certificate();
        let log = Arc::new(Mutex::new(Log::default()));
        let (out, out_rx) = mpsc::unbounded_channel();
        let task_log = log.clone();
        let cert = cert_der.clone();
        // ironrdp-acceptor's finalization is not `Send` (its handlers are
        // `?Send`): the server runs on a thread with a runtime of its own.
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            rt.block_on(async move {
                let listener = TcpListener::from_std(listener).unwrap();
                let mut out_rx = out_rx;
                for _ in 0..config.connections {
                    let Ok((stream, _)) = listener.accept().await else { return };
                    let result = serve(stream, &config, &cert, &key_der, &task_log, &mut out_rx).await;
                    if let Err(e) = result {
                        task_log.lock().unwrap().failed = Some(e.to_string());
                    }
                }
                task_log.lock().unwrap().closed = true;
            });
        });
        FakeRdp { port, cert_der, log, out }
    }

    pub async fn wait(&self, what: &str, check: impl Fn(&Log) -> bool) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            if check(&self.log.lock().unwrap()) {
                return;
            }
            if tokio::time::Instant::now() > deadline {
                let log = self.log.lock().unwrap();
                panic!("fake RDP server: no {what}; failed: {:?}; input: {:?}", log.failed, log.input);
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }
}

async fn serve(
    stream: tokio::net::TcpStream,
    config: &Config,
    cert_der: &[u8],
    key_der: &[u8],
    log: &Arc<Mutex<Log>>,
    out: &mut mpsc::UnboundedReceiver<Out>,
) -> anyhow::Result<()> {
    let size = DesktopSize { width: config.size.0, height: config.size.1 };
    let creds = Credentials { username: config.username.clone(), password: config.password.clone(), domain: None };
    let mut acceptor = Acceptor::new(config.security, size, capabilities(config.size), Some(creds));
    let tasks: super::clipboard::Tasks = Default::default();
    if config.clipboard {
        acceptor.attach_static_channel(ironrdp_cliprdr::CliprdrServer::new(Box::new(super::clipboard::TextBackend::new(tasks.clone()))));
    }
    let mut server_text = String::new();
    let framed = TokioFramed::new(stream);
    let BeginResult::ShouldUpgrade(stream) = ironrdp_acceptor::accept_begin(framed, &mut acceptor).await? else {
        anyhow::bail!("no TLS upgrade");
    };
    let tls_config = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(
            vec![rustls::pki_types::CertificateDer::from(cert_der.to_vec())],
            rustls::pki_types::PrivateKeyDer::Pkcs8(key_der.to_vec().into()),
        )?;
    let tls = match tokio_rustls::TlsAcceptor::from(Arc::new(tls_config)).accept(stream).await {
        Ok(tls) => tls,
        // The set-up, or a client that refused the certificate.
        Err(e) => anyhow::bail!("TLS: {e}"),
    };
    log.lock().unwrap().tls_connections += 1;
    let mut framed = TokioFramed::new(tls);
    acceptor.mark_security_upgrade_as_done();
    if acceptor.should_perform_credssp() {
        let public_key = super::tls::public_key_of(cert_der).unwrap();
        ironrdp_acceptor::accept_credssp(&mut framed, &mut acceptor, &mut super::session::NoKerberos, ServerName::new("client"), public_key, None).await?;
    }
    let (framed, result) = ironrdp_acceptor::accept_finalize(framed, &mut acceptor).await?;
    {
        let mut l = log.lock().unwrap();
        l.accepted = true;
        l.desktop = Some(config.size);
    }
    let io_channel_id = result.io_channel_id;
    let user_channel_id = result.user_channel_id;
    let mut channels = result.static_channels;
    let (mut reader, mut writer) = split_tokio_framed(framed);
    // server.rs: every joined static channel is started.
    for (_, channel, id) in channels.iter_mut() {
        if let Some(id) = id {
            let messages = channel.start()?;
            writer.write_all(&ironrdp_svc::server_encode_svc_messages(messages, id, user_channel_id)?).await?;
        }
    }
    loop {
        tokio::select! {
            frame = reader.read_pdu() => {
                let (action, bytes) = match frame {
                    Ok(f) => f,
                    Err(_) => return Ok(()),
                };
                match action {
                    Action::FastPath => {
                        let input: FastPathInput = decode(&bytes)?;
                        log.lock().unwrap().input.extend(input.input_events().iter().cloned());
                    }
                    Action::X224 => {
                        let Ok(message) = decode::<X224<mcs::McsMessage<'_>>>(&bytes) else { continue };
                        match message.0 {
                            mcs::McsMessage::SendDataRequest(data) if data.channel_id == io_channel_id => {
                                let Ok(control) = decode::<ironrdp_pdu::rdp::headers::ShareControlHeader>(data.user_data.as_ref()) else { continue };
                                if let ironrdp_pdu::rdp::headers::ShareControlPdu::Data(header) = control.share_control_pdu {
                                    if let ironrdp_pdu::rdp::headers::ShareDataPdu::ShutdownRequest = header.share_data_pdu {
                                        // server.rs: a Shutdown Request ends the connection.
                                        log.lock().unwrap().shutdown_requested = true;
                                        return Ok(());
                                    }
                                }
                            }
                            mcs::McsMessage::SendDataRequest(data) => {
                                let id = data.channel_id;
                                let Some(channel) = channels.get_by_channel_id_mut(id) else { continue };
                                let messages = channel.process(data.user_data.as_ref())?;
                                writer.write_all(&ironrdp_svc::server_encode_svc_messages(messages, id, user_channel_id)?).await?;
                                // What the clipboard backend asked for.
                                loop {
                                    let task = tasks.lock().unwrap().pop_front();
                                    let Some(task) = task else { break };
                                    let Some(channel) = channels.get_by_channel_id_mut(id) else { break };
                                    let Some(cliprdr) = channel.channel_processor_downcast_mut::<ironrdp_cliprdr::CliprdrServer>() else { break };
                                    let messages: Vec<ironrdp_svc::SvcMessage> = match task {
                                        super::clipboard::ClipboardTask::FetchRemoteText => {
                                            cliprdr.initiate_paste(ironrdp_cliprdr::pdu::ClipboardFormatId::CF_UNICODETEXT)?.into()
                                        }
                                        super::clipboard::ClipboardTask::SendLocalText => {
                                            cliprdr.submit_format_data(ironrdp_cliprdr::pdu::FormatDataResponse::new_unicode_string(&server_text))?.into()
                                        }
                                        super::clipboard::ClipboardTask::RemoteText(text) => {
                                            log.lock().unwrap().clipboard_from_client = Some(text);
                                            continue;
                                        }
                                        _ => continue,
                                    };
                                    writer.write_all(&ironrdp_svc::server_encode_svc_messages(messages, id, user_channel_id)?).await?;
                                }
                            }
                            mcs::McsMessage::DisconnectProviderUltimatum(_) => return Ok(()),
                            _ => {}
                        }
                    }
                }
            }
            what = out.recv() => {
                let Some(what) = what else { return Ok(()) };
                let frames = match what {
                    Out::Bitmap { x, y, w, h, bgrx } => fast_path_update(UpdateCode::Bitmap, &bitmap_update(x, y, w, h, &bgrx)),
                    Out::Pointer { w, h, hot_x, hot_y, rgba } => fast_path_update(UpdateCode::NewPointer, &pointer_update(w, h, hot_x, hot_y, &rgba)),
                    Out::PointerPosition { x, y } => fast_path_update(UpdateCode::PositionPointer, &encode_vec(&Point16 { x, y })?),
                    Out::HidePointer => fast_path_update(UpdateCode::HiddenPointer, &[]),
                    Out::DefaultPointer => fast_path_update(UpdateCode::DefaultPointer, &[]),
                    Out::Clipboard(text) => {
                        server_text = text;
                        let Some(id) = channels.get_channel_id_by_type::<ironrdp_cliprdr::CliprdrServer>() else { continue };
                        let channel = channels.get_by_channel_id_mut(id).unwrap();
                        let cliprdr = channel.channel_processor_downcast_mut::<ironrdp_cliprdr::CliprdrServer>().unwrap();
                        let messages: Vec<ironrdp_svc::SvcMessage> = cliprdr.initiate_copy(&[super::clipboard::unicode_text_format()])?.into();
                        vec![ironrdp_svc::server_encode_svc_messages(messages, id, user_channel_id)?]
                    }
                };
                for f in frames {
                    writer.write_all(&f).await?;
                }
            }
        }
    }
}
