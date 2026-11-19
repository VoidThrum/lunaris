//! The VNC connector against the fake server (fake_server.rs): handshake
//! and security, the picture in each encoding, cursor, resize, input,
//! clipboard and the end.

use std::collections::BTreeMap;
use std::time::Duration;

use super::fake_server::{self, ClientMessage, Config, FakeServer};
use super::rfb;
use super::VncConnector;
use crate::connector::raw_video::{EncoderChoice, Framebuffer};
use crate::connector::{Button, Codec, Command, Connector, DisplayInfo, Event, Session, Target, Viewer};

fn target(port: u16, password: &str, view_only: bool) -> Target {
    let mut values = BTreeMap::new();
    values.insert("host".to_string(), "127.0.0.1".to_string());
    values.insert("port".to_string(), port.to_string());
    values.insert("password".to_string(), password.to_string());
    values.insert("view_only".to_string(), if view_only { "on" } else { "off" }.to_string());
    Target { connector: "vnc".into(), values }
}

fn viewer() -> Viewer {
    Viewer { codecs: vec![Codec::H264], fps: 30, width: 0, height: 0, bitrate_kbps: 2000 }
}

fn connector() -> VncConnector {
    VncConnector { encoders: EncoderChoice::Software }
}

fn config(security: Vec<u8>, password: &str) -> Config {
    Config { version: b"RFB 003.008\n", security, password: password.into(), width: 320, height: 240 }
}

async fn next_any(session: &mut Session) -> Event {
    tokio::time::timeout(Duration::from_secs(10), session.events.recv()).await.expect("an event in time").expect("the session is open")
}

/// The next event that is not a delay.
async fn next(session: &mut Session) -> Event {
    loop {
        match next_any(session).await {
            Event::Latency { .. } => {}
            other => return other,
        }
    }
}

/// The next event that is not a picture or a delay.
async fn next_other(session: &mut Session) -> Event {
    loop {
        match next(session).await {
            Event::Video { .. } | Event::Latency { .. } => {}
            other => return other,
        }
    }
}

fn gradient(w: usize, h: usize) -> Vec<u8> {
    let mut px = Vec::with_capacity(w * h * 4);
    for y in 0..h {
        for x in 0..w {
            px.extend_from_slice(&[(x % 256) as u8, (y % 256) as u8, ((x / 7 + y / 5) % 256) as u8, 0]);
        }
    }
    px
}

#[tokio::test]
async fn description() {
    let d = connector().describe();
    assert_eq!(d.id, "vnc");
    let fields: Vec<(&str, &str, bool)> = d.fields.iter().map(|f| (f.key.as_str(), f.kind.as_str(), f.required)).collect();
    assert_eq!(fields, vec![("host", "text", true), ("port", "port", false), ("password", "secret", false), ("view_only", "choice", false)]);
    assert_eq!(d.fields[1].default, "5900");
    assert!(d.note.contains("does not encrypt"));
    assert!(d.capabilities.input && d.capabilities.clipboard && d.capabilities.cursor && !d.capabilities.audio);
    assert!(crate::connector::builtin().iter().any(|c| c.describe().id == "vnc"));
}

/// Every encoding decodes into the picture the server drew (the same
/// path the session's reading task takes, without the encoder).
#[tokio::test]
async fn every_encoding_draws_the_right_pixels() {
    let (w, h) = (130u16, 70u16);
    let picture = gradient(w as usize, h as usize);
    let mut z = flate2::Compress::new(flate2::Compression::default(), true);
    let mut bytes = fake_server::update(&[
        fake_server::raw(0, 0, w, h / 2, &picture[..w as usize * (h as usize / 2) * 4]),
        fake_server::zrle(&mut z, 0, h / 2, w, h - h / 2, &picture[w as usize * (h as usize / 2) * 4..]),
    ]);
    // A second ZRLE rectangle on the same zlib stream, then a CopyRect.
    let solid = vec![9u8, 8, 7, 0].repeat(64 * 3);
    bytes.extend(fake_server::update(&[fake_server::zrle(&mut z, 0, 0, 64, 3, &solid), fake_server::copy_rect(64, 0, 10, 3, 0, 0)]));
    let mut fb = Framebuffer::new_for_tests(w as u32, h as u32);
    let mut reader = std::io::Cursor::new(bytes);
    let mut zlib = flate2::Decompress::new(true);
    for _ in 0..2 {
        let rfb::ServerMessage::Update(rects) = rfb::read_message(&mut reader, &mut zlib).await.unwrap() else { panic!() };
        for rect in rects {
            match rect {
                rfb::Rect::Pixels { x, y, w, h, bgrx } => fb.put_bgra(x as u32, y as u32, w as u32, h as u32, &bgrx),
                rfb::Rect::CopyRect { x, y, w, h, src_x, src_y } => fb.copy_rect(src_x as u32, src_y as u32, x as u32, y as u32, w as u32, h as u32),
                other => panic!("{other:?}"),
            }
        }
    }
    let mut expected = picture.clone();
    for row in 0..3usize {
        for col in 0..74usize {
            let i = (row * w as usize + col) * 4;
            expected[i..i + 4].copy_from_slice(&[9, 8, 7, 0]);
        }
    }
    assert!(fb.data == expected, "the picture differs");
}

#[tokio::test]
async fn a_whole_session() {
    let server = FakeServer::start(config(vec![rfb::SECURITY_NONE, rfb::SECURITY_VNC_AUTH], "s3cret")).await;
    let (w, h) = (320u16, 240u16);
    let mut z = flate2::Compress::new(flate2::Compression::default(), true);
    let cursor_px = vec![0u8, 0, 255, 0].repeat(4 * 2);
    server
        .updates
        .send(fake_server::update(&[
            fake_server::raw(0, 0, w, h, &gradient(w as usize, h as usize)),
            fake_server::cursor(1, 0, 4, 2, &cursor_px, &[0b1010_0000, 0b0101_0000]),
        ]))
        .unwrap();

    let mut session = connector().connect(&target(server.port, "s3cret", false), &viewer()).await.unwrap();
    assert_eq!(server.log.lock().unwrap().auth_ok, Some(true));
    assert_eq!(server.log.lock().unwrap().shared_flag, Some(1));
    // The display, the cursor shape (from the first update) and the first
    // picture, a key frame (the cursor may come before the display).
    let (mut saw_displays, mut saw_key, mut saw_cursor) = (false, false, false);
    while !(saw_displays && saw_key && saw_cursor) {
        match next(&mut session).await {
            Event::Displays { displays, current: 0 } => {
                assert_eq!(displays, vec![DisplayInfo { name: "Display".into(), width: 320, height: 240 }]);
                saw_displays = true;
            }
            Event::Video { codec: Codec::H264, key: true, data, .. } => {
                assert!(saw_displays && data.len() > 100);
                saw_key = true;
            }
            Event::CursorShape { width: 4, height: 2, hot_x: 1, hot_y: 0, rgba } => {
                // BGRX 0,0,255 = red; the mask's first row 1010: shown, hidden, shown, hidden.
                assert_eq!(&rgba[0..8], &[255, 0, 0, 255, 255, 0, 0, 0]);
                assert_eq!(rgba[4 * 4 + 3], 0);
                assert_eq!(rgba[5 * 4 + 3], 255);
                saw_cursor = true;
            }
            Event::Latency { .. } => {}
            other => panic!("{other:?}"),
        }
    }
    let first = server.messages();
    assert_eq!(first[0], ClientMessage::SetPixelFormat(rfb::PixelFormat::BGRX));
    assert_eq!(first[1], ClientMessage::SetEncodings(rfb::ENCODINGS.to_vec()));
    assert_eq!(first[2], ClientMessage::UpdateRequest { incremental: false, x: 0, y: 0, w: 320, h: 240 });
    // Then incremental requests, paced (one after each update).
    server.wait("an incremental request", |log| log.messages.iter().any(|m| matches!(m, ClientMessage::UpdateRequest { incremental: true, .. }))).await;

    // A change in ZRLE: another picture follows.
    server.updates.send(fake_server::update(&[fake_server::zrle(&mut z, 10, 10, 100, 80, &gradient(100, 80))])).unwrap();
    loop {
        if let Event::Video { key: false, .. } = next(&mut session).await {
            break;
        }
    }

    // Input.
    let commands = session.commands.clone();
    for command in [
        Command::Key { position: 30, down: true },
        Command::Key { position: 30, down: false },
        Command::Key { position: 42, down: true },
        Command::Key { position: 30, down: true },
        Command::Key { position: 42, down: false },
        Command::Key { position: 30, down: false },
        Command::Text { text: "é".into() },
        Command::PointerMove { x: 10, y: 20 },
        Command::PointerButton { button: Button::Left, down: true },
        Command::PointerMoveRelative { dx: 5, dy: -30 },
        Command::PointerButton { button: Button::Left, down: false },
        Command::Wheel { dx: 0, dy: 1 },
        Command::Wheel { dx: -1, dy: 0 },
        Command::PointerMove { x: 5000, y: 5000 },
        Command::Clipboard { text: "Café".into() },
    ] {
        commands.send(command).await.unwrap();
    }
    server.wait("the clipboard text", |log| log.messages.iter().any(|m| matches!(m, ClientMessage::CutText(_)))).await;
    let input: Vec<ClientMessage> =
        server.messages().into_iter().filter(|m| !matches!(m, ClientMessage::UpdateRequest { .. } | ClientMessage::SetEncodings(_) | ClientMessage::SetPixelFormat(_))).collect();
    let key = |down, keysym| ClientMessage::Key { down, keysym };
    let ptr = |mask, x, y| ClientMessage::Pointer { mask, x, y };
    assert_eq!(
        input,
        vec![
            key(true, 0x61),
            key(false, 0x61),
            key(true, 0xffe1),
            key(true, 0x41),
            key(false, 0xffe1),
            // Released as it was pressed.
            key(false, 0x41),
            key(true, 0xe9),
            key(false, 0xe9),
            ptr(0, 10, 20),
            ptr(1, 10, 20),
            ptr(1, 15, 0),
            ptr(0, 15, 0),
            ptr(8, 15, 0),
            ptr(0, 15, 0),
            ptr(32, 15, 0),
            ptr(0, 15, 0),
            ptr(0, 319, 239),
            ClientMessage::CutText(vec![b'C', b'a', b'f', 0xe9]),
        ]
    );
    // Text Latin-1 cannot carry is not sent, and the viewer hears why.
    commands.send(Command::Clipboard { text: "Tiếng Việt".into() }).await.unwrap();
    assert!(matches!(next_other(&mut session).await, Event::ClipboardError { text } if text.contains("Latin-1")));

    // The server's clipboard.
    server.push.send(fake_server::server_cut_text(&[b'h', b'\xe9'])).unwrap();
    assert_eq!(next_other(&mut session).await, Event::Clipboard { text: "hé".into() });

    // A new size: the viewer hears of the display, then a key frame of it.
    server.updates.send(fake_server::update(&[fake_server::desktop_size(200, 100)])).unwrap();
    assert_eq!(next_other(&mut session).await, Event::Displays { displays: vec![DisplayInfo { name: "Display".into(), width: 200, height: 100 }], current: 0 });
    loop {
        if let Event::Video { key, .. } = next(&mut session).await {
            assert!(key);
            break;
        }
    }
    server.updates.send(fake_server::update(&[fake_server::extended_desktop_size(0, 0, 256, 128)])).unwrap();
    assert!(matches!(next_other(&mut session).await, Event::Displays { displays, .. } if displays[0].width == 256 && displays[0].height == 128));

    // A key held at the end is let go.
    commands.send(Command::Key { position: 29, down: true }).await.unwrap();
    commands.send(Command::Close).await.unwrap();
    loop {
        if let Event::Closed { reason } = next(&mut session).await {
            assert_eq!(reason, "Closed");
            break;
        }
    }
    server.wait("the end", |log| log.closed).await;
    let last: Vec<ClientMessage> = server.messages().into_iter().rev().filter(|m| matches!(m, ClientMessage::Key { .. })).take(2).collect();
    assert_eq!(last, vec![key(false, 0xffe3), key(true, 0xffe3)]);
}

#[tokio::test]
async fn view_only_sends_no_input() {
    let server = FakeServer::start(config(vec![rfb::SECURITY_NONE], "")).await;
    server.updates.send(fake_server::update(&[fake_server::raw(0, 0, 2, 2, &[0; 16])])).unwrap();
    let mut session = connector().connect(&target(server.port, "", true), &viewer()).await.unwrap();
    // The kernel's round trip of the connection is told (the loopback's:
    // well under a millisecond, never negative).
    loop {
        if let Event::Latency { ms } = next_any(&mut session).await {
            assert!((0.0..50.0).contains(&ms), "{ms}");
            break;
        }
    }
    for command in [Command::Key { position: 30, down: true }, Command::PointerMove { x: 1, y: 1 }, Command::Clipboard { text: "x".into() }] {
        session.commands.send(command).await.unwrap();
    }
    session.commands.send(Command::Close).await.unwrap();
    while !matches!(next(&mut session).await, Event::Closed { .. }) {}
    assert!(server.messages().iter().all(|m| !matches!(m, ClientMessage::Key { .. } | ClientMessage::Pointer { .. } | ClientMessage::CutText(_))));
}

#[tokio::test]
async fn security_errors_are_clear() {
    // Wrong password (3.8: with the server's reason).
    let server = FakeServer::start(config(vec![rfb::SECURITY_VNC_AUTH], "right")).await;
    let error = connector().connect(&target(server.port, "wrong", false), &viewer()).await.err().unwrap().to_string();
    assert_eq!(error, "The VNC password is not correct (Authentication)");
    // No password typed.
    let server = FakeServer::start(config(vec![rfb::SECURITY_VNC_AUTH], "right")).await;
    let error = connector().connect(&target(server.port, "", false), &viewer()).await.err().unwrap().to_string();
    assert!(error.contains("asks for a password"), "{error}");
    // Only security types this client does not have.
    let server = FakeServer::start(config(vec![19, 6], "")).await;
    let error = connector().connect(&target(server.port, "", false), &viewer()).await.err().unwrap().to_string();
    assert!(error.contains("VeNCrypt, RA2ne"), "{error}");
    // The server refuses (no types, a reason).
    let server = FakeServer::start(config(vec![], "")).await;
    let error = connector().connect(&target(server.port, "", false), &viewer()).await.err().unwrap().to_string();
    assert_eq!(error, "The VNC server refused the connection: go away");
}

#[tokio::test]
async fn older_versions() {
    for version in [b"RFB 003.003\n", b"RFB 003.007\n"] {
        let server = FakeServer::start(Config { version, ..config(vec![rfb::SECURITY_VNC_AUTH], "pw") }).await;
        server.updates.send(fake_server::update(&[fake_server::raw(0, 0, 2, 2, &[0; 16])])).unwrap();
        let mut session = connector().connect(&target(server.port, "pw", false), &viewer()).await.unwrap();
        assert!(matches!(next(&mut session).await, Event::Displays { .. }));
        assert!(matches!(next(&mut session).await, Event::Video { key: true, .. }));
        assert_eq!(server.log.lock().unwrap().auth_ok, Some(true));
    }
}
