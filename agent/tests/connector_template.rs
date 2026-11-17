//! The template connector, run by the agent's adapter for external
//! connectors, as a user would install it: a folder with `connector.json`
//! and the program.

use std::time::Duration;

use agent::connector::process::ProcessConnector;
use agent::connector::{Codec, Command, Connector, Event, Session, Target, Viewer};

/// The template's program, built once (it is another package of the
/// workspace: cargo builds it here, into the target folder of this test).
fn template_program() -> std::path::PathBuf {
    static BUILT: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| {
            let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../connectors/template/Cargo.toml");
            let target = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("connector-template");
            let status = std::process::Command::new(env!("CARGO"))
                .args(["build", "--manifest-path"])
                .arg(&manifest)
                .arg("--target-dir")
                .arg(&target)
                .status()
                .expect("cargo runs");
            assert!(status.success(), "the template connector builds");
            target.join("debug").join(if cfg!(windows) { "connector-template.exe" } else { "connector-template" })
        })
        .clone()
}

/// A connectors folder with the template installed in it.
fn install(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("lunaris-connector-test-{}-{}", std::process::id(), name));
    let folder = dir.join("template");
    std::fs::create_dir_all(&folder).unwrap();
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../connectors/template");
    let program = if cfg!(windows) { "connector-template.exe" } else { "connector-template" };
    std::fs::copy(template_program(), folder.join(program)).unwrap();
    std::fs::copy(source.join("pattern.ivf"), folder.join("pattern.ivf")).unwrap();
    let manifest = std::fs::read_to_string(source.join("connector.json")).unwrap().replace("\"connector-template\"", &format!("\"{}\"", program));
    std::fs::write(folder.join("connector.json"), manifest).unwrap();
    dir
}

fn target() -> Target {
    // As a source is written in the agent's config.
    let source: agent::connector::SourceConfig =
        serde_json::from_str(r#"{"id":"pattern","name":"Test pattern","connector":"template","fps":"60"}"#).unwrap();
    assert_eq!((source.id.as_str(), source.name.as_str()), ("pattern", "Test pattern"));
    source.target
}

async fn next(session: &mut Session) -> Event {
    tokio::time::timeout(Duration::from_secs(5), session.events.recv()).await.expect("an event in time").expect("the session is open")
}

/// The next event that `want` picks, skipping the others.
async fn until<T>(session: &mut Session, mut want: impl FnMut(&Event) -> Option<T>) -> T {
    loop {
        let event = next(session).await;
        if let Some(found) = want(&event) {
            return found;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn every_message_passes_through_the_adapter() {
    let dir = install("all");
    let connector = ProcessConnector::load(&dir, "template").unwrap();
    let description = connector.describe();
    assert_eq!(description.id, "template");
    assert!(description.capabilities.video.contains(&Codec::Vp8));
    assert!(description.capabilities.audio);
    assert!(description.fields.iter().any(|f| f.key == "file"));

    let viewer = Viewer { codecs: vec![Codec::H264, Codec::Vp8], fps: 60 };
    let mut session = connector.connect(&target(), &viewer).await.unwrap();

    // What there is to see comes first.
    match next(&mut session).await {
        Event::Displays { displays, current } => {
            assert_eq!(current, 0);
            assert_eq!((displays[0].width, displays[0].height), (320, 180));
        }
        other => panic!("displays first, not {:?}", other),
    }
    match next(&mut session).await {
        Event::CursorShape { width, height, rgba, .. } => assert_eq!(rgba.len() as u32, width * height * 4),
        other => panic!("the cursor's shape second, not {:?}", other),
    }

    // The picture: a key frame first, then frames that are not, with times that move on.
    let mut times = Vec::new();
    let mut keys = Vec::new();
    let mut sound = 0;
    let mut cursor_moves = 0;
    while times.len() < 40 {
        match next(&mut session).await {
            Event::Video { codec, key, capture_time_ms, data } => {
                assert_eq!(codec, Codec::Vp8);
                assert!(!data.is_empty());
                // RFC 6386: the low bit of the first byte is 0 in a key frame.
                assert_eq!(data[0] & 1 == 0, key);
                times.push(capture_time_ms.expect("the template gives capture times"));
                keys.push(key);
            }
            Event::Audio { channels, data } => {
                assert_eq!(channels, 1);
                assert_eq!(agent::connector::media::opus_duration(&data), Some(Duration::from_millis(20)));
                sound += 1;
            }
            Event::CursorPosition { visible, .. } => {
                assert!(visible);
                cursor_moves += 1;
            }
            other => panic!("unexpected {:?}", other),
        }
    }
    assert!(keys[0] && keys[1..].iter().all(|key| !key), "one key frame, the first: {:?}", keys);
    assert!(times.windows(2).all(|w| w[0] <= w[1]));
    // 60 frames a second were asked for: 39 intervals take about 650 ms.
    let span = times[39] - times[0];
    assert!((500..=900).contains(&span), "40 frames at 60 a second took {} ms", span);
    assert!(sound >= 20, "sound every 20 ms: {} packets", sound);
    assert!(cursor_moves >= 1);

    // A key frame when asked for.
    session.commands.send(Command::RequestKeyFrame).await.unwrap();
    let mut frames_until_key = 0;
    until(&mut session, |event| match event {
        Event::Video { key: true, .. } => Some(()),
        Event::Video { .. } => {
            frames_until_key += 1;
            None
        }
        _ => None,
    })
    .await;
    assert!(frames_until_key <= 3, "the key frame came {} frames after the request", frames_until_key);

    // The clipboard goes there and (this connector echoes) comes back; so does the pointer.
    session.commands.send(Command::Clipboard { text: "xin chào".into() }).await.unwrap();
    let text = until(&mut session, |event| match event {
        Event::Clipboard { text } => Some(text.clone()),
        _ => None,
    })
    .await;
    assert_eq!(text, "xin chào");
    session.commands.send(Command::PointerMove { x: 7, y: 9 }).await.unwrap();
    until(&mut session, |event| matches!(event, Event::CursorPosition { x: 7, y: 9, .. }).then_some(())).await;

    // The rest is taken without breaking the stream.
    for command in [
        Command::PointerButton { button: agent::connector::Button::Left, down: true },
        Command::PointerButton { button: agent::connector::Button::Left, down: false },
        Command::Wheel { dx: 0, dy: -1 },
        Command::PointerMoveRelative { dx: 3, dy: -4 },
        Command::Key { position: 30, down: true },
        Command::Key { position: 30, down: false },
        Command::Text { text: "ă".into() },
        Command::SwitchDisplay { index: 0 },
        Command::Quality { fps: 30, bitrate_kbps: 0 },
    ] {
        session.commands.send(command).await.unwrap();
    }
    until(&mut session, |event| matches!(event, Event::Video { .. }).then_some(())).await;

    // Close: the connector says so and ends.
    session.commands.send(Command::Close).await.unwrap();
    let reason = until(&mut session, |event| match event {
        Event::Closed { reason } => Some(reason.clone()),
        _ => None,
    })
    .await;
    assert_eq!(reason, "Closed");
    assert!(session.events.recv().await.is_none(), "nothing after closed");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_viewer_that_does_not_take_the_codec_is_told() {
    let dir = install("codec");
    let connector = ProcessConnector::load(&dir, "template").unwrap();
    let mut session = connector.connect(&target(), &Viewer { codecs: vec![Codec::H264], fps: 60 }).await.unwrap();
    let reason = until(&mut session, |event| match event {
        Event::Closed { reason } => Some(reason.clone()),
        _ => None,
    })
    .await;
    assert!(reason.contains("does not take vp8"), "{}", reason);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn only_what_is_installed_can_be_started() {
    let dir = install("install");
    // Not a folder's name, not installed, and a manifest that points outside its folder.
    assert!(ProcessConnector::load(&dir, "../template").is_err());
    assert!(ProcessConnector::load(&dir, "other").is_err());
    let outside = dir.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    let program = if cfg!(windows) { "connector-template.exe" } else { "connector-template" };
    std::fs::write(
        outside.join("connector.json"),
        format!(r#"{{"id":"outside","name":"x","command":"../template/{}"}}"#, program),
    )
    .unwrap();
    let error = ProcessConnector::load(&dir, "outside").err().expect("refused").to_string();
    assert!(error.contains("its own folder"), "{}", error);
    // The built-in ones are found by name without a folder.
    assert_eq!(agent::connector::find("rustdesk", &dir).unwrap().describe().id, "rustdesk");
    assert!(agent::connector::find("template", &dir).is_ok());
    // What there is to choose from: the built-in ones, then the installed ones
    // (the folder without a usable program is not one).
    let ids: Vec<String> = agent::connector::available(&dir).into_iter().map(|d| d.id).collect();
    assert_eq!(ids, ["rustdesk", "template"]);
    let _ = std::fs::remove_dir_all(dir);
}
