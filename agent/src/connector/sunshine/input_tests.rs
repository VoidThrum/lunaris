//! Input tests: every packet byte for byte against bytes written out by
//! hand from Input.h / InputStream.c, and read back by the port of
//! Sunshine's input.cpp in `fake_host.rs` (`read_input_packet`, which checks
//! sizes as `validate_input_packet` does); the keys against the host's own
//! tables (libvirtualhid, Linux and Windows).

use super::super::super::keys;
use super::super::fake_host::{linux_key_of_vk, read_input_packet, windows_key_of_vk, InputSeen};
use super::*;

fn read(packet: &[u8]) -> InputSeen {
    read_input_packet(packet).expect("a packet the host accepts")
}

#[test]
fn relative_mouse_movement() {
    // NV_REL_MOUSE_MOVE_PACKET: size 8 (BE), magic 7 (LE), deltaX, deltaY (BE).
    let packet = mouse_move_relative(5, -3);
    assert_eq!(packet, [0, 0, 0, 8, 7, 0, 0, 0, 0x00, 0x05, 0xFF, 0xFD]);
    assert_eq!(read(&packet), InputSeen::MoveRelative { dx: 5, dy: -3 });
    assert_eq!(mouse_move_relative(-32768, 32767), [0, 0, 0, 8, 7, 0, 0, 0, 0x80, 0x00, 0x7F, 0xFF]);

    // More than a packet holds: as many as it takes, the sum is the movement.
    let packets = mouse_move_relative_split(70_000, -40_000);
    let moves: Vec<InputSeen> = packets.iter().map(|p| read(p)).collect();
    assert_eq!(
        moves,
        vec![
            InputSeen::MoveRelative { dx: 32767, dy: -32768 },
            InputSeen::MoveRelative { dx: 32767, dy: -7232 },
            InputSeen::MoveRelative { dx: 4466, dy: 0 },
        ]
    );
    assert!(mouse_move_relative_split(0, 0).is_empty());
    assert_eq!(mouse_move_relative_split(1, 0).len(), 1);
}

#[test]
fn absolute_mouse_position() {
    // NV_ABS_MOUSE_MOVE_PACKET: size 14, magic 5, x, y, unused, width - 1, height - 1 (BE).
    let packet = mouse_position(960, 540, 1920, 1080);
    assert_eq!(
        packet,
        [0, 0, 0, 14, 5, 0, 0, 0, 0x03, 0xC0, 0x02, 0x1C, 0, 0, 0x07, 0x7F, 0x04, 0x37]
    );
    assert_eq!(read(&packet), InputSeen::MoveAbsolute { x: 960, y: 540, width: 1919, height: 1079 });
}

#[test]
fn mouse_buttons() {
    // NV_MOUSE_BUTTON_PACKET: size 5, magic 8 (down) / 9 (up), button.
    assert_eq!(mouse_button(BUTTON_LEFT, true), [0, 0, 0, 5, 8, 0, 0, 0, 1]);
    assert_eq!(mouse_button(BUTTON_LEFT, false), [0, 0, 0, 5, 9, 0, 0, 0, 1]);
    assert_eq!(mouse_button(BUTTON_X2, true), [0, 0, 0, 5, 8, 0, 0, 0, 5]);
    assert_eq!(read(&mouse_button(BUTTON_RIGHT, true)), InputSeen::Button { button: 3, release: false });
    assert_eq!(read(&mouse_button(BUTTON_MIDDLE, false)), InputSeen::Button { button: 2, release: true });
}

#[test]
fn wheels() {
    // NV_SCROLL_PACKET: size 10, magic 0x0A, scrollAmt1, scrollAmt2 (the same, BE), zero.
    assert_eq!(scroll(120), [0, 0, 0, 10, 0x0A, 0, 0, 0, 0x00, 0x78, 0x00, 0x78, 0, 0]);
    assert_eq!(scroll(-240), [0, 0, 0, 10, 0x0A, 0, 0, 0, 0xFF, 0x10, 0xFF, 0x10, 0, 0]);
    assert_eq!(read(&scroll(-240)), InputSeen::Scroll { amount: -240 });
    // SS_HSCROLL_PACKET: size 6, magic 0x55000001 (LE), scrollAmount (BE).
    assert_eq!(horizontal_scroll(120), [0, 0, 0, 6, 0x01, 0x00, 0x00, 0x55, 0x00, 0x78]);
    assert_eq!(read(&horizontal_scroll(-120)), InputSeen::HScroll { amount: -120 });
}

#[test]
fn keys_down_and_up() {
    // NV_KEYBOARD_PACKET: size 10, magic 3 (down) / 4 (up), flags, keyCode
    // (LE, 0x80 in the high byte), modifiers, zero.
    assert_eq!(keyboard(0x41, true, 0, 0), [0, 0, 0, 10, 3, 0, 0, 0, 0, 0x41, 0x80, 0, 0, 0]);
    assert_eq!(keyboard(0x41, false, MODIFIER_SHIFT, 0), [0, 0, 0, 10, 4, 0, 0, 0, 0, 0x41, 0x80, 1, 0, 0]);
    assert_eq!(
        keyboard(0xAF, true, MODIFIER_CTRL | MODIFIER_ALT, 1),
        [0, 0, 0, 10, 3, 0, 0, 0, 1, 0xAF, 0x80, 6, 0, 0]
    );
    assert_eq!(read(&keyboard(0x41, true, 0, 0)), InputSeen::Key { vk: 0x41, release: false, modifiers: 0, flags: 0 });
    assert_eq!(
        read(&keyboard(0xA3, false, MODIFIER_META, 0)),
        InputSeen::Key { vk: 0xA3, release: true, modifiers: 8, flags: 0 }
    );
}

#[test]
fn text() {
    // NV_UNICODE_PACKET: size 4 + bytes of text, magic 0x17, the text.
    assert_eq!(utf8_text("hi"), vec![vec![0, 0, 0, 6, 0x17, 0, 0, 0, b'h', b'i']]);
    assert_eq!(read(&utf8_text("hi")[0]), InputSeen::Text("hi".into()));
    assert!(utf8_text("").is_empty());

    // At most 32 bytes per packet, and no character cut in two: 31 one-byte
    // letters and then a two-byte one make two packets.
    let long = format!("{}é and more text that goes on for a while: 日本語のテキスト 🙂🙂🙂", "a".repeat(31));
    let packets = utf8_text(&long);
    assert_eq!(packets[0].len(), 8 + 31);
    let mut joined = String::new();
    for packet in &packets {
        assert!(packet.len() - 8 <= UTF8_TEXT_EVENT_MAX_COUNT);
        match read(packet) {
            InputSeen::Text(part) => {
                // A whole number of characters: nothing became U+FFFD.
                assert!(!part.contains('\u{FFFD}'));
                joined.push_str(&part);
            }
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(joined, long);
    // Exactly 32 bytes fit in one.
    assert_eq!(utf8_text(&"b".repeat(32)).len(), 1);
    assert_eq!(utf8_text(&"b".repeat(33)).len(), 2);
}

#[test]
fn the_host_refuses_wrong_sizes() {
    // input.cpp validate_fixed_input_packet: the size field must be the
    // structure's, and the bytes must be there.
    let good = mouse_button(BUTTON_LEFT, true);
    let mut wrong_size = good.clone();
    wrong_size[3] = 6;
    wrong_size.push(0);
    assert!(read_input_packet(&wrong_size).is_err());
    assert!(read_input_packet(&good[..8]).is_err());
    assert!(read_input_packet(&keyboard(0x41, true, 0, 0)[..13]).is_err());
    let mut long_text = utf8_text(&"c".repeat(32)).remove(0);
    long_text.push(b'c');
    long_text[3] += 1;
    assert!(read_input_packet(&long_text).is_err());
}

fn encode_all(encoder: &mut InputEncoder, commands: &[Command]) -> Vec<(u8, InputSeen)> {
    commands.iter().flat_map(|c| encoder.encode(c)).map(|p| (p.channel, read(&p.data))).collect()
}

#[test]
fn commands_become_packets_on_their_channels() {
    const MOUSE: u8 = 3; // CTRL_CHANNEL_MOUSE
    const KEYBOARD: u8 = 2; // CTRL_CHANNEL_KEYBOARD
    let mut encoder = InputEncoder::new(1920, 1080);
    let seen = encode_all(
        &mut encoder,
        &[
            Command::PointerMove { x: 100, y: 200 },
            // Outside the picture: kept inside.
            Command::PointerMove { x: -5, y: 5000 },
            Command::PointerMoveRelative { dx: 7, dy: -9 },
            Command::PointerMoveRelative { dx: 0, dy: 0 },
            Command::PointerButton { button: Button::Left, down: true },
            Command::PointerButton { button: Button::Left, down: false },
            Command::PointerButton { button: Button::Middle, down: true },
            Command::PointerButton { button: Button::Right, down: true },
            Command::PointerButton { button: Button::Back, down: true },
            Command::PointerButton { button: Button::Forward, down: false },
            // Wheel notches: dy > 0 up, dx > 0 right; 120 a notch.
            Command::Wheel { dx: 0, dy: 1 },
            Command::Wheel { dx: -2, dy: -3 },
            Command::Wheel { dx: 0, dy: 0 },
            Command::Wheel { dx: 0, dy: 100_000 },
            Command::Text { text: "ô".into() },
        ],
    );
    assert_eq!(
        seen,
        vec![
            (MOUSE, InputSeen::MoveAbsolute { x: 100, y: 200, width: 1919, height: 1079 }),
            (MOUSE, InputSeen::MoveAbsolute { x: 0, y: 1079, width: 1919, height: 1079 }),
            (MOUSE, InputSeen::MoveRelative { dx: 7, dy: -9 }),
            (MOUSE, InputSeen::Button { button: 1, release: false }),
            (MOUSE, InputSeen::Button { button: 1, release: true }),
            (MOUSE, InputSeen::Button { button: 2, release: false }),
            (MOUSE, InputSeen::Button { button: 3, release: false }),
            (MOUSE, InputSeen::Button { button: 4, release: false }),
            (MOUSE, InputSeen::Button { button: 5, release: true }),
            (MOUSE, InputSeen::Scroll { amount: 120 }),
            (MOUSE, InputSeen::Scroll { amount: -360 }),
            (MOUSE, InputSeen::HScroll { amount: -240 }),
            (MOUSE, InputSeen::Scroll { amount: 32767 }),
            (KEYBOARD, InputSeen::Text("ô".into())),
        ]
    );
    // What is not input makes no packet.
    for command in [
        Command::RequestKeyFrame,
        Command::Close,
        Command::Quality { fps: 30, bitrate_kbps: 1000 },
        Command::SwitchDisplay { index: 1 },
        Command::Clipboard { text: "x".into() },
    ] {
        assert!(!is_input(&command));
        assert!(encoder.encode(&command).is_empty());
    }
    assert!(is_input(&Command::Key { position: 30, down: true }));
}

#[test]
fn modifiers_name_what_is_held() {
    const KEY_A: u32 = 30;
    const KEY_LEFTSHIFT: u32 = 42;
    const KEY_RIGHTSHIFT: u32 = 54;
    const KEY_LEFTCTRL: u32 = 29;
    const KEY_RIGHTALT: u32 = 100;
    const KEY_LEFTMETA: u32 = 125;
    let mut encoder = InputEncoder::new(1280, 720);
    let key = |position, down| Command::Key { position, down };
    let seen: Vec<InputSeen> = encode_all(
        &mut encoder,
        &[
            key(KEY_A, true),
            key(KEY_A, false),
            key(KEY_LEFTSHIFT, true),
            key(KEY_RIGHTSHIFT, true),
            key(KEY_A, true),
            // One Shift let go: the other still holds.
            key(KEY_LEFTSHIFT, false),
            key(KEY_A, false),
            key(KEY_RIGHTSHIFT, false),
            key(KEY_LEFTCTRL, true),
            key(KEY_RIGHTALT, true),
            key(KEY_LEFTMETA, true),
            key(KEY_A, true),
            key(KEY_LEFTMETA, false),
            key(KEY_RIGHTALT, false),
            key(KEY_LEFTCTRL, false),
            key(KEY_A, false),
            // A key GameStream has no name for: nothing.
            key(240, true),
        ],
    )
    .into_iter()
    .map(|(channel, seen)| {
        assert_eq!(channel, 2);
        seen
    })
    .collect();
    let k = |vk, release, modifiers| InputSeen::Key { vk, release, modifiers, flags: 0 };
    assert_eq!(
        seen,
        vec![
            k(0x41, false, 0),
            k(0x41, true, 0),
            k(0xA0, false, 1),
            k(0xA1, false, 1),
            k(0x41, false, 1),
            k(0xA0, true, 1),
            k(0x41, true, 1),
            k(0xA1, true, 0),
            k(0xA2, false, 2),
            k(0xA5, false, 6),
            k(0x5B, false, 14),
            k(0x41, false, 14),
            k(0x5B, true, 6),
            k(0xA5, true, 2),
            k(0xA2, true, 0),
            k(0x41, true, 0),
        ]
    );
}

#[test]
fn the_key_that_arrives_is_the_key_at_the_same_place() {
    // Keys a Linux host of Sunshine 2026.914 has no entry for (libvirtualhid
    // key_code_to_linux): Henkan, Muhenkan, Pause, Hanja, the menu key, and
    // the three volume keys.
    const UNKNOWN_ON_LINUX: [u32; 8] = [92, 94, 113, 114, 115, 119, 123, 127];
    const KEY_KPENTER: u32 = 96;
    let mut keys_checked = 0;
    for position in 1..256u32 {
        let Some(vk) = keymap::vk_of_position(position) else { continue };
        keys_checked += 1;
        let flags = keymap::flags_of_vk(vk);

        // A Linux host: virtual key -> KEY_*.
        match linux_key_of_vk(vk) {
            Some(key) if position == KEY_KPENTER => assert_eq!(key, 28, "keypad Enter is Enter there"),
            Some(key) => assert_eq!(key, position, "position {position} (vk {vk:#x}) on a Linux host"),
            None => assert!(UNKNOWN_ON_LINUX.contains(&position), "position {position} (vk {vk:#x}) is lost on a Linux host"),
        }

        // A Windows host: the scan code it sends against the scan code of
        // that position (keys.rs: rdev's table), or the virtual key itself.
        let (scan, extended, by_virtual_key) = windows_key_of_vk(vk, flags & keymap::SS_KBE_FLAG_NON_NORMALIZED == 0);
        if position == KEY_KPENTER {
            assert_eq!((scan, by_virtual_key), (0x1C, false));
            continue;
        }
        if by_virtual_key {
            // Pressed as the virtual key: right for the keys that are the
            // same on every layout, which are the only ones allowed here.
            assert!(
                matches!(vk, 0x13 | 0x19 | 0x1C | 0x1D | 0x2C | 0xAD | 0xAE | 0xAF),
                "position {position} (vk {vk:#x}) would follow the host's layout"
            );
            continue;
        }
        let want = keys::win_scancode_of(position).unwrap();
        assert_eq!(scan as u32, want & 0xFF, "position {position} (vk {vk:#x}): scan code on a Windows host");
        assert_eq!(extended, want >> 8 == 0xE0, "position {position} (vk {vk:#x}): extended on a Windows host");
    }
    // Letters, digits, punctuation, function keys, keypad, navigation, modifiers.
    assert!(keys_checked > 100, "{keys_checked}");

    // The keys sent "not normalized" and why: normalized, that host's table
    // would press something else.
    for (vk, wrong_scan) in [(0x2Cu8, 0x54u8), (0xAD, 0x20), (0xAE, 0x2E), (0xAF, 0x30)] {
        assert_eq!(keymap::flags_of_vk(vk), 1);
        assert_eq!(windows_key_of_vk(vk, true), (wrong_scan, false, false));
        assert_eq!(windows_key_of_vk(vk, false), (0, false, true));
    }
    let mut encoder = InputEncoder::new(1280, 720);
    let volume_up = encoder.encode(&Command::Key { position: 115, down: true });
    assert_eq!(read(&volume_up[0].data), InputSeen::Key { vk: 0xAF, release: false, modifiers: 0, flags: 1 });
}

#[test]
fn movements_that_came_at_once_become_fewer() {
    let commands = vec![
        Command::PointerMove { x: 1, y: 1 },
        Command::PointerMove { x: 2, y: 2 },
        Command::PointerMove { x: 3, y: 4 },
        Command::PointerButton { button: Button::Left, down: true },
        Command::PointerMove { x: 5, y: 6 },
        Command::PointerMoveRelative { dx: 1, dy: 1 },
        Command::PointerMoveRelative { dx: 2, dy: -5 },
        Command::Key { position: 30, down: true },
        Command::PointerMoveRelative { dx: 1, dy: 1 },
        Command::RequestKeyFrame,
        Command::PointerMoveRelative { dx: 1, dy: 1 },
    ];
    assert_eq!(
        coalesce(commands),
        vec![
            Command::PointerMove { x: 3, y: 4 },
            Command::PointerButton { button: Button::Left, down: true },
            Command::PointerMove { x: 5, y: 6 },
            Command::PointerMoveRelative { dx: 3, dy: -4 },
            Command::Key { position: 30, down: true },
            Command::PointerMoveRelative { dx: 1, dy: 1 },
            Command::RequestKeyFrame,
            Command::PointerMoveRelative { dx: 1, dy: 1 },
        ]
    );
    assert!(coalesce(Vec::new()).is_empty());
}
