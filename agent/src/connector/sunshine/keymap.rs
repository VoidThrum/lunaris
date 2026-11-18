//! From where a key is (`Command::Key::position`, the Linux input key code)
//! to what GameStream names keys by: the Windows virtual-key code of the key
//! at that place on a US keyboard.
//!
//! Why that arrives as the key at the same place, read in the host:
//! Sunshine `src/input.cpp` passthrough(PNV_KEYBOARD_PACKET) takes
//! `keyCode & 0x00FF` and hands it on (tag v2026.914 and master:
//! `src/platform/virtualhid_input.cpp` keyboard_event, to libvirtualhid,
//! pinned at 53e1a94 by the tag):
//! - a Linux host turns it into a `KEY_*` code with a fixed table
//!   (libvirtualhid `src/platform/linux/uhid_backend.cpp` key_code_to_linux):
//!   VK_A is KEY_A whatever layout is set there;
//! - a Windows host, when the packet's flags lack
//!   `SS_KBE_FLAG_NON_NORMALIZED` (we send 0), takes the code as "normalized
//!   to the Windows US English keyboard layout" and looks the scan code up
//!   in a US table (libvirtualhid `src/platform/windows/keylayout.hpp`,
//!   `windows_backend.cpp` WindowsKeyboard::submit: SendInput with
//!   KEYEVENTF_SCANCODE, extended keys decided there by the code).
//! So both want the US-layout code of the position, which is the join that
//! `connector/keys.rs` already holds (`position_of_vk`); this file only
//! turns it round and adds what that table lacks.
//!
//! Keys a Linux host of that version does not know (its table has no
//! entry): Pause, the menu key, the volume keys, the Japanese and Korean
//! input keys. They are sent all the same; a Windows host takes them.
//!
//! Four keys go with `SS_KBE_FLAG_NON_NORMALIZED` ([`flags_of_vk`]): Print
//! Screen and the three volume keys. They are the same key on every layout,
//! and the US table of that libvirtualhid gives them the scan codes 0x54,
//! 0x20, 0x2E and 0x30 without the extended bit (extended_key() does not
//! list them), which on Windows are Alt+PrintScreen's code and the keys D, C
//! and B. With the flag a Windows host presses them by their virtual-key
//! code; a Linux host does not look at the flag.

use std::sync::OnceLock;

use super::super::keys;

/// `KEY_KPENTER`: Windows has no code of its own for it (VK_RETURN with the
/// extended bit); it goes as Return.
const KEY_KPENTER: u32 = 96;
const VK_RETURN: u8 = 0x0D;

/// Limelight.h SS_KBE_FLAG_NON_NORMALIZED: the code is not to be read as a
/// place on the US layout.
pub const SS_KBE_FLAG_NON_NORMALIZED: u8 = 0x01;
const VK_SNAPSHOT: u8 = 0x2C;
const VK_VOLUME_MUTE: u8 = 0xAD;
const VK_VOLUME_DOWN: u8 = 0xAE;
const VK_VOLUME_UP: u8 = 0xAF;

/// The `flags` byte of the keyboard packet for a key (see the top of this file).
pub fn flags_of_vk(vk: u8) -> u8 {
    match vk {
        VK_SNAPSHOT | VK_VOLUME_MUTE | VK_VOLUME_DOWN | VK_VOLUME_UP => SS_KBE_FLAG_NON_NORMALIZED,
        _ => 0,
    }
}

pub const VK_LWIN: u8 = 0x5B;
pub const VK_RWIN: u8 = 0x5C;
pub const VK_LSHIFT: u8 = 0xA0;
pub const VK_RSHIFT: u8 = 0xA1;
pub const VK_LCONTROL: u8 = 0xA2;
pub const VK_RCONTROL: u8 = 0xA3;
pub const VK_LMENU: u8 = 0xA4;
pub const VK_RMENU: u8 = 0xA5;

fn table() -> &'static [u8; 256] {
    static TABLE: OnceLock<[u8; 256]> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut table = [0u8; 256];
        for vk in 1..=0xFFu16 {
            // The side-less Shift, Control and Alt are the left ones in
            // keys.rs; the codes with a side are the ones to send.
            if matches!(vk, 0x10..=0x12) {
                continue;
            }
            if let Some(position) = keys::position_of_vk(vk) {
                if position < 256 && table[position as usize] == 0 {
                    table[position as usize] = vk as u8;
                }
            }
        }
        table[KEY_KPENTER as usize] = VK_RETURN;
        table
    })
}

/// The Windows virtual-key code to send for the key at `position`.
pub fn vk_of_position(position: u32) -> Option<u8> {
    match table().get(position as usize) {
        Some(0) | None => None,
        Some(vk) => Some(*vk),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positions_to_virtual_keys() {
        assert_eq!(vk_of_position(30), Some(0x41)); // KEY_A
        assert_eq!(vk_of_position(16), Some(0x51)); // KEY_Q: the key, not the letter of some layout
        assert_eq!(vk_of_position(42), Some(VK_LSHIFT));
        assert_eq!(vk_of_position(54), Some(VK_RSHIFT));
        assert_eq!(vk_of_position(29), Some(VK_LCONTROL));
        assert_eq!(vk_of_position(97), Some(VK_RCONTROL));
        assert_eq!(vk_of_position(56), Some(VK_LMENU));
        assert_eq!(vk_of_position(100), Some(VK_RMENU));
        assert_eq!(vk_of_position(125), Some(VK_LWIN));
        assert_eq!(vk_of_position(28), Some(0x0D));
        assert_eq!(vk_of_position(96), Some(0x0D));
        assert_eq!(vk_of_position(103), Some(0x26)); // KEY_UP
        assert_eq!(vk_of_position(86), Some(0xE2)); // KEY_102ND
        assert_eq!(vk_of_position(0), None);
        assert_eq!(vk_of_position(255), None);
        assert_eq!(vk_of_position(4000), None);
    }

    #[test]
    fn every_key_of_the_table_goes_there_and_back() {
        for position in 1..256u32 {
            if let Some(vk) = vk_of_position(position) {
                if position != KEY_KPENTER {
                    assert_eq!(keys::position_of_vk(vk as u16), Some(position), "position {position}");
                }
            }
        }
    }
}
