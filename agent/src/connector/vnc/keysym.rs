//! Keys for RFB KeyEvent (RFC 6143 §7.5.4): X Window System keysyms.
//!
//! The values are those of X.Org's `include/X11/keysymdef.h` and
//! `XF86keysym.h` (xorgproto, MIT licence); RFC 6143 §7.5.4 lists the
//! non-character ones it expects and says the others are as in the X
//! protocol. A key is named by where it is (`Command::Key::position`, a
//! Linux input key code); the character keys are given the keysym of what
//! they type on a US keyboard, with or without Shift (§7.5.4: "the keysym
//! ... depends on the state of the modifier keys"), so that the server
//! types what that key typed.

/// (Linux input key code, keysym, keysym with Shift held).
#[rustfmt::skip]
const KEYS: &[(u32, u32, u32)] = &[
    (1, 0xff1b, 0xff1b),   // Escape
    (2, 0x31, 0x21), (3, 0x32, 0x40), (4, 0x33, 0x23), (5, 0x34, 0x24), (6, 0x35, 0x25),
    (7, 0x36, 0x5e), (8, 0x37, 0x26), (9, 0x38, 0x2a), (10, 0x39, 0x28), (11, 0x30, 0x29),
    (12, 0x2d, 0x5f),      // minus, underscore
    (13, 0x3d, 0x2b),      // equal, plus
    (14, 0xff08, 0xff08),  // BackSpace
    (15, 0xff09, 0xfe20),  // Tab, ISO_Left_Tab
    (16, 0x71, 0x51), (17, 0x77, 0x57), (18, 0x65, 0x45), (19, 0x72, 0x52), (20, 0x74, 0x54),
    (21, 0x79, 0x59), (22, 0x75, 0x55), (23, 0x69, 0x49), (24, 0x6f, 0x4f), (25, 0x70, 0x50),
    (26, 0x5b, 0x7b),      // bracketleft, braceleft
    (27, 0x5d, 0x7d),      // bracketright, braceright
    (28, 0xff0d, 0xff0d),  // Return
    (29, 0xffe3, 0xffe3),  // Control_L
    (30, 0x61, 0x41), (31, 0x73, 0x53), (32, 0x64, 0x44), (33, 0x66, 0x46), (34, 0x67, 0x47),
    (35, 0x68, 0x48), (36, 0x6a, 0x4a), (37, 0x6b, 0x4b), (38, 0x6c, 0x4c),
    (39, 0x3b, 0x3a),      // semicolon, colon
    (40, 0x27, 0x22),      // apostrophe, quotedbl
    (41, 0x60, 0x7e),      // grave, asciitilde
    (42, 0xffe1, 0xffe1),  // Shift_L
    (43, 0x5c, 0x7c),      // backslash, bar
    (44, 0x7a, 0x5a), (45, 0x78, 0x58), (46, 0x63, 0x43), (47, 0x76, 0x56), (48, 0x62, 0x42),
    (49, 0x6e, 0x4e), (50, 0x6d, 0x4d),
    (51, 0x2c, 0x3c),      // comma, less
    (52, 0x2e, 0x3e),      // period, greater
    (53, 0x2f, 0x3f),      // slash, question
    (54, 0xffe2, 0xffe2),  // Shift_R
    (55, 0xffaa, 0xffaa),  // KP_Multiply
    (56, 0xffe9, 0xffe9),  // Alt_L
    (57, 0x20, 0x20),      // space
    (58, 0xffe5, 0xffe5),  // Caps_Lock
    (59, 0xffbe, 0xffbe), (60, 0xffbf, 0xffbf), (61, 0xffc0, 0xffc0), (62, 0xffc1, 0xffc1),
    (63, 0xffc2, 0xffc2), (64, 0xffc3, 0xffc3), (65, 0xffc4, 0xffc4), (66, 0xffc5, 0xffc5),
    (67, 0xffc6, 0xffc6), (68, 0xffc7, 0xffc7), // F1 .. F10
    (69, 0xff7f, 0xff7f),  // Num_Lock
    (70, 0xff14, 0xff14),  // Scroll_Lock
    // The keypad as with Num Lock on (digits); its other meaning is the
    // server's business once it knows Num_Lock.
    (71, 0xffb7, 0xffb7), (72, 0xffb8, 0xffb8), (73, 0xffb9, 0xffb9), // KP_7 KP_8 KP_9
    (74, 0xffad, 0xffad),  // KP_Subtract
    (75, 0xffb4, 0xffb4), (76, 0xffb5, 0xffb5), (77, 0xffb6, 0xffb6), // KP_4 KP_5 KP_6
    (78, 0xffab, 0xffab),  // KP_Add
    (79, 0xffb1, 0xffb1), (80, 0xffb2, 0xffb2), (81, 0xffb3, 0xffb3), // KP_1 KP_2 KP_3
    (82, 0xffb0, 0xffb0),  // KP_0
    (83, 0xffae, 0xffae),  // KP_Decimal
    (86, 0x3c, 0x3e),      // the key between Shift_L and Z: less, greater
    (87, 0xffc8, 0xffc8), (88, 0xffc9, 0xffc9), // F11 F12
    (96, 0xff8d, 0xff8d),  // KP_Enter
    (97, 0xffe4, 0xffe4),  // Control_R
    (98, 0xffaf, 0xffaf),  // KP_Divide
    (99, 0xff61, 0xff61),  // Print
    (100, 0xffea, 0xffea), // Alt_R
    (102, 0xff50, 0xff50), // Home
    (103, 0xff52, 0xff52), // Up
    (104, 0xff55, 0xff55), // Prior (Page Up)
    (105, 0xff51, 0xff51), // Left
    (106, 0xff53, 0xff53), // Right
    (107, 0xff57, 0xff57), // End
    (108, 0xff54, 0xff54), // Down
    (109, 0xff56, 0xff56), // Next (Page Down)
    (110, 0xff63, 0xff63), // Insert
    (111, 0xffff, 0xffff), // Delete
    (113, 0x1008ff12, 0x1008ff12), // XF86AudioMute
    (114, 0x1008ff11, 0x1008ff11), // XF86AudioLowerVolume
    (115, 0x1008ff13, 0x1008ff13), // XF86AudioRaiseVolume
    (119, 0xff13, 0xff13), // Pause
    (125, 0xffeb, 0xffeb), // Super_L
    (126, 0xffec, 0xffec), // Super_R
    (127, 0xff67, 0xff67), // Menu
    (183, 0xffca, 0xffca), (184, 0xffcb, 0xffcb), (185, 0xffcc, 0xffcc), (186, 0xffcd, 0xffcd),
    (187, 0xffce, 0xffce), (188, 0xffcf, 0xffcf), (189, 0xffd0, 0xffd0), (190, 0xffd1, 0xffd1),
    (191, 0xffd2, 0xffd2), (192, 0xffd3, 0xffd3), (193, 0xffd4, 0xffd4), (194, 0xffd5, 0xffd5), // F13 .. F24
];

pub const KEY_LEFTSHIFT: u32 = 42;
pub const KEY_RIGHTSHIFT: u32 = 54;

/// The keysym of the key at `position`, with or without Shift held.
pub fn keysym_of(position: u32, shift: bool) -> Option<u32> {
    KEYS.iter().find(|k| k.0 == position).map(|k| if shift { k.2 } else { k.1 })
}

/// The keysym that types `c` (`Command::Text`): Return and Tab for line
/// ends and tabs; a Latin-1 character is its own keysym (keysymdef.h:
/// "the keysym values for Latin-1 are the same as the Unicode values");
/// any other character is 0x01000000 + its code point (keysymdef.h, on
/// Unicode keysyms). Control characters have none.
pub fn keysym_of_char(c: char) -> Option<u32> {
    let cp = u32::from(c);
    match c {
        '\n' | '\r' => Some(0xff0d),
        '\t' => Some(0xff09),
        _ if cp < 0x20 || (0x7f..0xa0).contains(&cp) => None,
        _ if cp < 0x100 => Some(cp),
        _ => Some(0x0100_0000 + cp),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_digits_and_others() {
        assert_eq!(keysym_of(30, false), Some(0x61)); // a
        assert_eq!(keysym_of(30, true), Some(0x41)); // A
        assert_eq!(keysym_of(2, true), Some(0x21)); // !
        assert_eq!(keysym_of(28, true), Some(0xff0d));
        assert_eq!(keysym_of(103, false), Some(0xff52));
        assert_eq!(keysym_of(500, false), None);
        assert_eq!(keysym_of_char('é'), Some(0xe9));
        assert_eq!(keysym_of_char('ệ'), Some(0x0100_1ec7));
        assert_eq!(keysym_of_char('\n'), Some(0xff0d));
        assert_eq!(keysym_of_char('\u{7}'), None);
    }

    #[test]
    fn every_key_of_the_viewer_has_a_keysym() {
        // The positions a Lunaris viewer can send (keys.rs) all have one,
        // except the Kana / Hanja / Convert keys of Asian keyboards.
        for vk in 0u16..=0xff {
            if let Some(position) = crate::connector::keys::position_of_vk(vk) {
                if matches!(position, 92..=94 | 123) {
                    continue;
                }
                assert!(keysym_of(position, false).is_some(), "position {position}");
            }
        }
    }

    #[test]
    fn no_position_is_listed_twice() {
        for (i, a) in KEYS.iter().enumerate() {
            assert!(KEYS[i + 1..].iter().all(|b| b.0 != a.0), "{}", a.0);
        }
    }
}
