//! Where a key is on the keyboard, for the three ways it is named here:
//! the Windows virtual-key code a Lunaris viewer sends (for the key at that
//! place on a US keyboard), the Linux input key code the connector
//! interface uses (`Command::Key::position`), and the Windows scan code.
//!
//! Generated from rdev, the keyboard library RustDesk's client converts
//! with (https://github.com/rustdesk-org/rdev, MIT):
//! `src/keycodes/windows.rs` (key, virtual-key code, scan code) joined with
//! `src/keycodes/linux.rs` (key, X keycode; the Linux input code is the X
//! keycode - 8). `rdev::linux_code_to_win_scancode`, which the client's
//! `keyboard.rs _map_keyboard_mode` calls for a Windows peer, is this join.

/// (Windows virtual-key code, Linux input key code, Windows scan code;
/// 0xE0xx is an extended key).
#[rustfmt::skip]
const KEYS: &[(u16, u32, u32)] = &[
    (0x00, 93, 0x0070), // KanaMode
    (0x08, 14, 0x000E), // Backspace
    (0x09, 15, 0x000F), // Tab
    (0x0D, 28, 0x001C), // Return
    (0x13, 119, 0x0000), // Pause
    (0x14, 58, 0x003A), // CapsLock
    (0x19, 123, 0x00F1), // Hanja
    (0x1B, 1, 0x0001), // Escape
    (0x1C, 92, 0x0079), // Convert
    (0x1D, 94, 0x007B), // NonConvert
    (0x20, 57, 0x0039), // Space
    (0x21, 104, 0xE049), // PageUp
    (0x22, 109, 0xE051), // PageDown
    (0x23, 107, 0xE04F), // End
    (0x24, 102, 0xE047), // Home
    (0x25, 105, 0xE04B), // LeftArrow
    (0x26, 103, 0xE048), // UpArrow
    (0x27, 106, 0xE04D), // RightArrow
    (0x28, 108, 0xE050), // DownArrow
    (0x2C, 99, 0xE037), // PrintScreen
    (0x2D, 110, 0xE052), // Insert
    (0x2E, 111, 0xE053), // Delete
    (0x30, 11, 0x000B), // Num0
    (0x31, 2, 0x0002), // Num1
    (0x32, 3, 0x0003), // Num2
    (0x33, 4, 0x0004), // Num3
    (0x34, 5, 0x0005), // Num4
    (0x35, 6, 0x0006), // Num5
    (0x36, 7, 0x0007), // Num6
    (0x37, 8, 0x0008), // Num7
    (0x38, 9, 0x0009), // Num8
    (0x39, 10, 0x000A), // Num9
    (0x41, 30, 0x001E), // KeyA
    (0x42, 48, 0x0030), // KeyB
    (0x43, 46, 0x002E), // KeyC
    (0x44, 32, 0x0020), // KeyD
    (0x45, 18, 0x0012), // KeyE
    (0x46, 33, 0x0021), // KeyF
    (0x47, 34, 0x0022), // KeyG
    (0x48, 35, 0x0023), // KeyH
    (0x49, 23, 0x0017), // KeyI
    (0x4A, 36, 0x0024), // KeyJ
    (0x4B, 37, 0x0025), // KeyK
    (0x4C, 38, 0x0026), // KeyL
    (0x4D, 50, 0x0032), // KeyM
    (0x4E, 49, 0x0031), // KeyN
    (0x4F, 24, 0x0018), // KeyO
    (0x50, 25, 0x0019), // KeyP
    (0x51, 16, 0x0010), // KeyQ
    (0x52, 19, 0x0013), // KeyR
    (0x53, 31, 0x001F), // KeyS
    (0x54, 20, 0x0014), // KeyT
    (0x55, 22, 0x0016), // KeyU
    (0x56, 47, 0x002F), // KeyV
    (0x57, 17, 0x0011), // KeyW
    (0x58, 45, 0x002D), // KeyX
    (0x59, 21, 0x0015), // KeyY
    (0x5A, 44, 0x002C), // KeyZ
    (0x5B, 125, 0xE05B), // MetaLeft
    (0x5C, 126, 0xE05C), // MetaRight
    (0x5D, 127, 0xE05D), // Apps
    (0x60, 82, 0x0052), // Kp0
    (0x61, 79, 0x004F), // Kp1
    (0x62, 80, 0x0050), // Kp2
    (0x63, 81, 0x0051), // Kp3
    (0x64, 75, 0x004B), // Kp4
    (0x65, 76, 0x004C), // Kp5
    (0x66, 77, 0x004D), // Kp6
    (0x67, 71, 0x0047), // Kp7
    (0x68, 72, 0x0048), // Kp8
    (0x69, 73, 0x0049), // Kp9
    (0x6A, 55, 0x0037), // KpMultiply
    (0x6B, 78, 0x004E), // KpPlus
    (0x6D, 74, 0x004A), // KpMinus
    (0x6E, 83, 0x0053), // KpDecimal
    (0x6F, 98, 0xE035), // KpDivide
    (0x70, 59, 0x003B), // F1
    (0x71, 60, 0x003C), // F2
    (0x72, 61, 0x003D), // F3
    (0x73, 62, 0x003E), // F4
    (0x74, 63, 0x003F), // F5
    (0x75, 64, 0x0040), // F6
    (0x76, 65, 0x0041), // F7
    (0x77, 66, 0x0042), // F8
    (0x78, 67, 0x0043), // F9
    (0x79, 68, 0x0044), // F10
    (0x7A, 87, 0x0057), // F11
    (0x7B, 88, 0x0058), // F12
    (0x7C, 183, 0x0064), // F13
    (0x7D, 184, 0x0065), // F14
    (0x7E, 185, 0x0066), // F15
    (0x7F, 186, 0x0067), // F16
    (0x80, 187, 0x0068), // F17
    (0x81, 188, 0x0069), // F18
    (0x82, 189, 0x006A), // F19
    (0x83, 190, 0x006B), // F20
    (0x84, 191, 0x006C), // F21
    (0x85, 192, 0x006D), // F22
    (0x86, 193, 0x006E), // F23
    (0x87, 194, 0x0076), // F24
    (0x90, 69, 0x0045), // NumLock
    (0x91, 70, 0x0046), // ScrollLock
    (0xA0, 42, 0x002A), // ShiftLeft
    (0xA1, 54, 0x0036), // ShiftRight
    (0xA2, 29, 0x001D), // ControlLeft
    (0xA3, 97, 0xE01D), // ControlRight
    (0xA4, 56, 0x0038), // Alt
    (0xA5, 100, 0xE038), // AltGr
    (0xAD, 113, 0xE020), // VolumeMute
    (0xAE, 114, 0xE02E), // VolumeDown
    (0xAF, 115, 0xE030), // VolumeUp
    (0xBA, 39, 0x0027), // SemiColon
    (0xBB, 13, 0x000D), // Equal
    (0xBC, 51, 0x0033), // Comma
    (0xBD, 12, 0x000C), // Minus
    (0xBE, 52, 0x0034), // Dot
    (0xBF, 53, 0x0035), // Slash
    (0xC0, 41, 0x0029), // BackQuote
    (0xDB, 26, 0x001A), // LeftBracket
    (0xDC, 43, 0x002B), // BackSlash
    (0xDD, 27, 0x001B), // RightBracket
    (0xDE, 40, 0x0028), // Quote
    (0xE2, 86, 0x0056), // IntlBackslash
];

/// The Linux input key code of the key a viewer names by `vk`.
pub fn position_of_vk(vk: u16) -> Option<u32> {
    // A viewer may name a modifier without its side: the left one.
    let vk = match vk {
        0x10 => 0xA0,
        0x11 => 0xA2,
        0x12 => 0xA4,
        other => other,
    };
    KEYS.iter().find(|key| key.0 == vk).map(|key| key.1)
}

/// rdev `linux_code_to_win_scancode`, from the Linux input key code.
pub fn win_scancode_of(position: u32) -> Option<u32> {
    KEYS.iter().find(|key| key.1 == position).map(|key| key.2)
}

pub const KEY_CAPSLOCK: u32 = 58;
pub const KEY_NUMLOCK: u32 = 69;

/// rdev `is_letter_rdev_key`: the keys a client sends its CapsLock state with.
pub fn is_letter(position: u32) -> bool {
    matches!(position, 16..=25 | 30..=38 | 44..=50)
}

/// rdev `is_numpad_rdev_key`: the keys a client sends its NumLock state with.
pub fn is_numpad(position: u32) -> bool {
    matches!(position, 71..=83)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn well_known_keys() {
        // VK_A -> KEY_A (30) -> scan code 0x1E.
        assert_eq!(position_of_vk(0x41), Some(30));
        assert_eq!(win_scancode_of(30), Some(0x1E));
        // The arrows and the right-hand modifiers are extended keys.
        assert_eq!(position_of_vk(0x26), Some(103));
        assert_eq!(win_scancode_of(103), Some(0xE048));
        assert_eq!(position_of_vk(0xA3).and_then(win_scancode_of), Some(0xE01D));
        // Shift without a side is the left one.
        assert_eq!(position_of_vk(0x10), Some(42));
        assert_eq!(position_of_vk(0x14), Some(KEY_CAPSLOCK));
        assert_eq!(position_of_vk(0x90), Some(KEY_NUMLOCK));
        assert!(is_letter(position_of_vk(0x5A).unwrap()) && !is_letter(position_of_vk(0x31).unwrap()));
        assert!(is_numpad(position_of_vk(0x60).unwrap()));
        assert_eq!(position_of_vk(0x07), None);
    }

    #[test]
    fn no_position_is_listed_twice() {
        for (i, a) in KEYS.iter().enumerate() {
            assert!(KEYS[i + 1..].iter().all(|b| b.0 != a.0), "vk {:#x}", a.0);
        }
    }
}
