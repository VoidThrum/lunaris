//! The template connector, run by the agent's adapter for external
//! connectors, as a user would install it: a folder with `connector.json`
//! and the program.

use std::time::Duration;

use agent::connector::process::ProcessConnector;
use agent::connector::sources::{self, AddOutcome, SetupState};
use agent::connector::{Codec, Command, Connector, Event, Session, SetupStep, Target, Viewer};

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

    let viewer = Viewer { codecs: vec![Codec::H264, Codec::Vp8], fps: 60, width: 0, height: 0, bitrate_kbps: 0 };
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
    let mut session = connector.connect(&target(), &Viewer { codecs: vec![Codec::H264], fps: 60, width: 0, height: 0, bitrate_kbps: 0 }).await.unwrap();
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
    assert_eq!(ids, ["rustdesk", "sunshine", "rdp", "vnc", "template"]);
    let _ = std::fs::remove_dir_all(dir);
}

/// How many of the installed template's programs run (Linux: by their
/// program file, which is this test's own copy).
#[cfg(target_os = "linux")]
fn programs_running(dir: &std::path::Path) -> usize {
    let program = dir.join("template").join("connector-template").canonicalize().unwrap();
    std::fs::read_dir("/proc")
        .unwrap()
        .flatten()
        .filter(|entry| std::fs::read_link(entry.path().join("exe")).is_ok_and(|exe| exe == program))
        .count()
}

fn wait_until(what: &str, done: impl Fn() -> bool) {
    for _ in 0..500 {
        if done() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("{}", what);
}

/// An agent's config file in `dir`, with no sources.
fn config_in(dir: &std::path::Path) -> String {
    let path = dir.join("agent_config.json").to_string_lossy().into_owned();
    agent::pairing::save_config(&agent::pairing::AgentConfig::default(), &path).unwrap();
    path
}

/// The step with the PIN (the template says "Starting" first).
fn wait_for_pin(setup: &str) -> SetupStep {
    for _ in 0..500 {
        match sources::setup_state(setup) {
            Some(SetupState::Pending { step }) if step.code.is_some() => return step,
            Some(SetupState::Pending { .. }) => std::thread::sleep(Duration::from_millis(20)),
            other => panic!("the set-up ended: {:?}", other),
        }
    }
    panic!("no PIN was shown");
}

fn wait_for_end(setup: &str) -> Option<SetupState> {
    for _ in 0..500 {
        match sources::setup_state(setup) {
            Some(SetupState::Pending { .. }) => std::thread::sleep(Duration::from_millis(20)),
            other => return other,
        }
    }
    panic!("the set-up did not end");
}

/// The template's pretend pairing through the adapter alone: its steps,
/// then the values to keep.
#[tokio::test(flavor = "multi_thread")]
async fn the_demo_set_up_passes_through_the_adapter() {
    let dir = install("setup-adapter");
    let connector = ProcessConnector::load(&dir, "template").unwrap();
    assert!(connector.describe().capabilities.setup);
    // Its note for the "Add computer" dialog comes from its manifest.
    assert!(connector.describe().note.starts_with("Shows a recorded test pattern."));
    assert_eq!(agent::connector::find("rustdesk", &dir).unwrap().describe().note, "");
    assert!(connector.describe().fields.iter().any(|f| f.key == "pair"));
    let target: Target = serde_json::from_str(r#"{"connector":"template","pair":"1"}"#).unwrap();

    let (steps_tx, mut steps) = tokio::sync::mpsc::channel(8);
    let (connector, for_setup) = (std::sync::Arc::new(connector), target.clone());
    let running = {
        let connector = connector.clone();
        tokio::spawn(async move { connector.setup(&for_setup, steps_tx).await })
    };
    let first = tokio::time::timeout(Duration::from_secs(10), steps.recv()).await.unwrap().unwrap();
    assert_eq!(first, SetupStep { message: "Starting the demo pairing".into(), code: None });
    let second = tokio::time::timeout(Duration::from_secs(10), steps.recv()).await.unwrap().unwrap();
    assert_eq!(second.code.as_deref(), Some("0000"));
    assert!(second.message.starts_with("Enter this PIN at the other computer."), "{}", second.message);
    // It waits for the person.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!running.is_finished());
    std::fs::write(dir.join("template").join("paired"), b"").unwrap();
    let values = tokio::time::timeout(Duration::from_secs(10), running).await.unwrap().unwrap().unwrap();
    assert_eq!(values.get("paired_key").map(String::as_str), Some("demo-key-not-a-secret"));
    assert_eq!(values.len(), 2);
    #[cfg(target_os = "linux")]
    assert_eq!(programs_running(&dir), 0, "the program is gone when the set-up is over");

    // A target that needs none: done at once, nothing to keep, no step.
    let plain: Target = serde_json::from_str(r#"{"connector":"template"}"#).unwrap();
    let (steps_tx, mut steps) = tokio::sync::mpsc::channel(8);
    assert!(connector.setup(&plain, steps_tx).await.unwrap().is_empty());
    assert!(steps.recv().await.is_none());
    let _ = std::fs::remove_dir_all(dir);
}

/// Adding a computer of the template's kind, as the agent does it for a
/// server, a link and the app's own computers (`sources::begin_add`).
#[test]
fn adding_with_the_demo_set_up() {
    let dir = install("setup-add");
    let config = config_in(&dir);
    let paired = dir.join("template").join("paired");
    let refused = dir.join("template").join("refused");
    let saved = || agent::pairing::load_config(&config).unwrap().sources;
    let begin = |name: &str, values: &str| sources::begin_add(&config, &dir, name, "template", values);

    // Without the demo pairing: added at once (the program is asked and has nothing to do).
    let outcome = begin("Plain", r#"{"fps": "30"}"#).unwrap();
    assert!(matches!(&outcome, AddOutcome::Added { id } if id.starts_with("plain-")), "{:?}", outcome);
    assert_eq!(saved().len(), 1);
    assert_eq!(saved()[0].target.values.len(), 1);
    // The template has no address field: its computers are never taken for one another.
    assert_eq!(saved()[0].group, None);
    // What is wrong with the form is said before anything is started.
    assert_eq!(begin(" ", "{}").unwrap_err(), "Give the computer a name");
    assert_eq!(sources::begin_add(&config, &dir, "X", "nope", "{}").unwrap_err(), "That connector is not installed");

    // With it: a step to show, nothing saved yet.
    let AddOutcome::Setup { setup, .. } = begin("Paired PC", r#"{"pair": "1", "fps": "30"}"#).unwrap() else { panic!("no set-up") };
    let step = wait_for_pin(&setup);
    assert_eq!(step.code.as_deref(), Some("0000"));
    assert_eq!(saved().len(), 1);
    // The "other computer" accepts: the source is saved with what the pairing returned.
    std::fs::write(&paired, b"").unwrap();
    let Some(SetupState::Done { id }) = wait_for_end(&setup) else { panic!("not done") };
    let sources_now = saved();
    let source = sources_now.iter().find(|s| s.id == id).expect("saved");
    assert_eq!((source.name.as_str(), source.target.get("pair"), source.target.get("paired_key")), ("Paired PC", "1", "demo-key-not-a-secret"));
    assert_eq!(source.target.get("paired_id"), "template-demo");
    assert!(sources_now.iter().all(|s| s.group.is_none()), "no address, no group");
    // Said to be one computer with another: the group is kept with it, through its set-up too.
    // (The "other computer" still accepts: it may be done before its first step is shown.)
    let grouped_id = match sources::begin_add_in_group(&config, &dir, "Paired again", "template", r#"{"pair": "1"}"#, Some("host:h1")).unwrap() {
        AddOutcome::Added { id } => id,
        AddOutcome::Setup { setup, .. } => match wait_for_end(&setup) {
            Some(SetupState::Done { id }) => id,
            other => panic!("not done: {:?}", other),
        },
    };
    assert_eq!(saved().iter().find(|s| s.id == grouped_id).and_then(|s| s.group.clone()).as_deref(), Some("host:h1"));
    assert_eq!(sources::begin_add_in_group(&config, &dir, "Bad", "template", "{}", Some("no good")).unwrap_err(), sources::GROUP_NOT_VALID);
    let removed = common::SourceChange::Remove { id: grouped_id };
    assert!(sources::answer_at(&config, &dir, &removed).error.is_none());
    // Listed without it, and kept when the computer is edited.
    let all = agent::connector::available(&dir);
    let views = sources::views_json(&sources_now, &all);
    assert!(views.contains(r#""pair":"1""#) && !views.contains("demo-key") && !views.contains("paired_"), "{}", views);
    assert!(!format!("{:?}", sources_now).contains("demo-key"));
    let update = common::SourceChange::Update { id: id.clone(), name: "Paired".into(), values: [("fps".to_string(), "60".to_string())].into(), group: None };
    let answer = sources::answer_at(&config, &dir, &update);
    assert_eq!(answer.id.as_deref(), Some(id.as_str()));
    let source = saved().into_iter().find(|s| s.id == id).unwrap();
    assert_eq!((source.name.as_str(), source.target.get("fps"), source.target.get("paired_key")), ("Paired", "60", "demo-key-not-a-secret"));
    std::fs::remove_file(&paired).unwrap();

    // The "other computer" refuses: its reason, nothing saved.
    let AddOutcome::Setup { setup, .. } = begin("Refused PC", r#"{"pair": "1"}"#).unwrap() else { panic!("no set-up") };
    wait_for_pin(&setup);
    std::fs::write(&refused, b"").unwrap();
    assert_eq!(wait_for_end(&setup), Some(SetupState::Failed { error: "The other computer refused the pairing".into() }));
    assert_eq!(saved().len(), 2);
    std::fs::remove_file(&refused).unwrap();

    // As a server asks: add, ask about it, cancel. The program is stopped, nothing is saved.
    let add = common::SourceChange::Add { name: "Server PC".into(), connector: "template".into(), values: [("pair".to_string(), "1".to_string())].into(), group: None };
    let answer = sources::answer_at(&config, &dir, &add);
    let info = answer.setup.expect("a set-up to show");
    assert_eq!((answer.id, answer.error), (None, None));
    wait_for_pin(&info.setup);
    let asked = sources::answer_at(&config, &dir, &common::SourceChange::SetupState { setup: info.setup.clone() });
    assert_eq!(asked.setup.as_ref().and_then(|s| s.code.as_deref()), Some("0000"));
    #[cfg(target_os = "linux")]
    assert_eq!(programs_running(&dir), 1);
    let cancelled = sources::answer_at(&config, &dir, &common::SourceChange::SetupCancel { setup: info.setup.clone() });
    assert!(cancelled.error.is_none());
    assert_eq!(sources::setup_state(&info.setup), None);
    #[cfg(target_os = "linux")]
    wait_until("the cancelled set-up's program is stopped", || programs_running(&dir) == 0);
    std::fs::write(&paired, b"").unwrap();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(saved().len(), 2, "a cancelled set-up adds nothing");
    std::fs::remove_file(&paired).unwrap();

    // The agent ends: every set-up that runs is stopped.
    let AddOutcome::Setup { setup: first, .. } = begin("One", r#"{"pair": "1"}"#).unwrap() else { panic!("no set-up") };
    let AddOutcome::Setup { setup: second, .. } = begin("Two", r#"{"pair": "1"}"#).unwrap() else { panic!("no set-up") };
    wait_for_pin(&first);
    wait_for_pin(&second);
    sources::cancel_all_setups();
    assert_eq!((sources::setup_state(&first), sources::setup_state(&second)), (None, None));
    #[cfg(target_os = "linux")]
    wait_until("the programs are stopped", || programs_running(&dir) == 0);
    assert_eq!(saved().len(), 2);
    let _ = std::fs::remove_dir_all(dir);
}
