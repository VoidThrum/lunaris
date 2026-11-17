//! Mouse and keyboard of a RustDesk session: the host side of RustDesk's
//! input handling, ported from rustdesk/src/server/input_service.rs and the
//! `MouseEvent` / `KeyEvent` branches of rustdesk/src/server/connection.rs,
//! onto Lunaris's injector (lunaris-media `InputInjector`: absolute and
//! relative pointer moves, X11-numbered buttons, Windows virtual-key codes,
//! text).
//!
//! Where RustDesk asks the operating system which modifiers are down
//! (`get_modifier_state`), this keeps the keys the session itself holds: the
//! injector cannot be asked. The lock keys it can be asked about
//! (`Injector::lock_state`), as far as the platform tells: Caps Lock and Num
//! Lock follow the client as in RustDesk's `LockModesHandler`.

use super::proto::{
    key_event, pointer_device_event, touch_event, ControlKey, KeyEvent, KeyboardMode, MouseEvent, PointerDeviceEvent,
};

// rustdesk/src/common.rs `mod input`: `MouseEvent.mask` is a type in the low
// three bits and a button above them.
const MOUSE_TYPE_MOVE: i32 = 0;
const MOUSE_TYPE_DOWN: i32 = 1;
const MOUSE_TYPE_UP: i32 = 2;
const MOUSE_TYPE_WHEEL: i32 = 3;
const MOUSE_TYPE_TRACKPAD: i32 = 4;
const MOUSE_TYPE_MOVE_RELATIVE: i32 = 5;
const MOUSE_TYPE_MASK: i32 = 0x7;
const MOUSE_BUTTON_LEFT: i32 = 0x01;
const MOUSE_BUTTON_RIGHT: i32 = 0x02;
const MOUSE_BUTTON_WHEEL: i32 = 0x04;
const MOUSE_BUTTON_BACK: i32 = 0x08;
const MOUSE_BUTTON_FORWARD: i32 = 0x10;

/// rustdesk/src/server/input_service.rs `handle_mouse_simulation_`:
/// `MAX_RELATIVE_MOUSE_DELTA`.
const MAX_RELATIVE_MOUSE_DELTA: i32 = 10000;
/// Windows `WHEEL_DELTA`: one notch of the wheel.
const WHEEL_DELTA: i32 = 120;
/// Wheel notches injected for one event at most (RustDesk has no limit; a
/// client cannot make this host loop for long).
const MAX_WHEEL_CLICKS: i32 = 60;

// Buttons as lunaris-media's `InputInjector::mouse_button` numbers them (X11).
const BUTTON_LEFT: u32 = 1;
const BUTTON_MIDDLE: u32 = 2;
const BUTTON_RIGHT: u32 = 3;
const BUTTON_WHEEL_UP: u32 = 4;
const BUTTON_WHEEL_DOWN: u32 = 5;
const BUTTON_WHEEL_LEFT: u32 = 6;
const BUTTON_WHEEL_RIGHT: u32 = 7;
const BUTTON_BACK: u32 = 8;
const BUTTON_FORWARD: u32 = 9;

// Windows virtual-key codes (what `InputInjector::keyboard_key` takes).
const VK_RETURN: u16 = 0x0D;
const VK_SHIFT: u16 = 0x10;
const VK_CONTROL: u16 = 0x11;
const VK_MENU: u16 = 0x12;
const VK_LWIN: u16 = 0x5B;
const VK_RWIN: u16 = 0x5C;
const VK_LSHIFT: u16 = 0xA0;
const VK_RSHIFT: u16 = 0xA1;
const VK_LCONTROL: u16 = 0xA2;
const VK_RCONTROL: u16 = 0xA3;
const VK_LMENU: u16 = 0xA4;
const VK_RMENU: u16 = 0xA5;

/// What the session's input ends in: the real injector, or a recorder.
pub(crate) trait Injector: Send {
    fn move_absolute(&mut self, x: i32, y: i32, width: i32, height: i32);
    fn move_relative(&mut self, dx: i32, dy: i32);
    fn button(&mut self, button: u32, press: bool);
    /// A key by its Windows virtual-key code.
    fn key(&mut self, vk: u16, press: bool);
    /// The key at a physical position, with nothing translated: the
    /// kernel's key code on Linux, the scan code (extended keys as 0xE0xx)
    /// on Windows. False when the code cannot be injected.
    fn key_position(&mut self, code: u32, press: bool) -> bool;
    fn text(&mut self, text: &str);
    fn flush(&mut self);
    /// Whether a lock key is on at this computer; `None` when that cannot
    /// be known (then it is left alone).
    fn lock_state(&mut self, _lock: Lock) -> Option<bool> {
        None
    }
    /// Whether a lock key changed for one key event is put back after it.
    /// RustDesk does not on Wayland with uinput ("there must be a delay to
    /// ensure the lock state is applied").
    fn restores_locks(&self) -> bool {
        true
    }
    /// Locks this computer's screen (the client's "Lock" button).
    fn lock_screen(&mut self) {}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Lock {
    Caps,
    Num,
}

/// Lunaris's injector, as bridge.rs `handle_inbound_packet` drives it.
pub(crate) struct RealInjector {
    injector: lunaris_media::input::InputInjector,
    /// The clipboard text is pasted from where the injector cannot type it;
    /// kept, as on Wayland the owner of the clipboard must stay alive.
    paste_clipboard: Option<arboard::Clipboard>,
}

/// rustdesk/src/server/input_service.rs `CLIPBOARD_SYNC_DELAY_MS`: how long
/// the clipboard is given to reach the compositor before the paste keys.
const CLIPBOARD_SYNC_DELAY: std::time::Duration = std::time::Duration::from_millis(50);
/// `input_text_via_clipboard_server`: "Brief delay to allow the target
/// application to process the paste event."
const PASTE_SETTLE_DELAY: std::time::Duration = std::time::Duration::from_millis(20);
// linux/input-event-codes.h
const KEY_LEFTSHIFT: u32 = 42;
const KEY_INSERT: u32 = 110;

impl RealInjector {
    pub(crate) fn new() -> Option<Self> {
        lunaris_media::input::InputInjector::new().map(|injector| RealInjector { injector, paste_clipboard: None })
    }

    /// rustdesk/src/server/input_service.rs `input_text_via_clipboard_server`
    /// (what RustDesk does on Wayland with uinput, which has keys but no way
    /// to type a character the layout lacks): the text is put on the
    /// clipboard, given a moment, and pasted with Shift+Insert. As there,
    /// the clipboard is not put back afterwards (a late restore could be
    /// pasted by the next keystroke). Blocking, on the input thread: what
    /// the client typed after this text waits behind it.
    fn paste(&mut self, text: &str) -> bool {
        if self.paste_clipboard.is_none() {
            self.paste_clipboard = arboard::Clipboard::new().ok();
        }
        let Some(clipboard) = self.paste_clipboard.as_mut() else { return false };
        // Not a change of this computer's clipboard to tell the client
        // (RustDesk: `record_wayland_clipboard_input_for_sync_filter`).
        crate::share::note_own_clipboard(text);
        if clipboard.set_text(text.to_string()).is_err() {
            return false;
        }
        std::thread::sleep(CLIPBOARD_SYNC_DELAY);
        self.injector.key_position(KEY_LEFTSHIFT, true);
        self.injector.key_position(KEY_INSERT, true);
        self.injector.key_position(KEY_INSERT, false);
        self.injector.key_position(KEY_LEFTSHIFT, false);
        self.injector.flush();
        std::thread::sleep(PASTE_SETTLE_DELAY);
        true
    }
}

impl Injector for RealInjector {
    fn move_absolute(&mut self, x: i32, y: i32, width: i32, height: i32) {
        self.injector.move_mouse_absolute(x, y, width, height);
    }

    fn move_relative(&mut self, dx: i32, dy: i32) {
        self.injector.move_mouse_relative(dx, dy);
    }

    fn button(&mut self, button: u32, press: bool) {
        self.injector.mouse_button(button, press);
    }

    fn key(&mut self, vk: u16, press: bool) {
        self.injector.keyboard_key(vk, press);
    }

    fn key_position(&mut self, code: u32, press: bool) -> bool {
        self.injector.key_position(code, press)
    }

    /// Text that arrives as text (a phone's keyboard, translate mode). Typed
    /// directly where the injector can type any character: SendInput with
    /// KEYEVENTF_UNICODE on Windows, XTest with a borrowed keycode on X11
    /// (RustDesk: `en.key_sequence`). The uinput injector of a Wayland
    /// session only has keys: printable ASCII is typed with them, anything
    /// else pasted, which is RustDesk's rule there (`process_seq`:
    /// "pure ASCII printable sequence: input via key sequence; any non-ASCII
    /// present: input whole sequence via clipboard to preserve order").
    fn text(&mut self, text: &str) {
        let printable_ascii = text.chars().all(|c| (0x20..=0x7E).contains(&(c as u32)));
        if self.injector.types_unicode() || printable_ascii || !self.paste(text) {
            self.injector.type_text(text);
        }
    }

    fn flush(&mut self) {
        self.injector.flush();
    }

    fn lock_state(&mut self, lock: Lock) -> Option<bool> {
        self.injector.lock_state(match lock {
            Lock::Caps => lunaris_media::input::LockKey::Caps,
            Lock::Num => lunaris_media::input::LockKey::Num,
        })
    }

    fn restores_locks(&self) -> bool {
        // The uinput injector is the one that cannot type Unicode.
        self.injector.types_unicode()
    }

    /// rustdesk/src/platform/linux.rs `lock_screen` (`xdg-screensaver
    /// lock`) and rustdesk/src/platform/windows.rs `lock_screen`
    /// (`LockWorkStation`).
    fn lock_screen(&mut self) {
        #[cfg(target_os = "linux")]
        {
            let _ = std::process::Command::new("xdg-screensaver").arg("lock").spawn();
        }
        #[cfg(target_os = "windows")]
        unsafe {
            let _ = windows::Win32::System::Shutdown::LockWorkStation();
        }
    }
}

/// Writes each call as a line instead of injecting it (tests, and
/// `LUNARIS_RUSTDESK_RECORD_INPUT_TO`).
pub(crate) struct RecordingInjector(pub(crate) Box<dyn FnMut(String) + Send>);

impl Injector for RecordingInjector {
    fn move_absolute(&mut self, x: i32, y: i32, width: i32, height: i32) {
        (self.0)(format!("move_abs {} {} of {}x{}", x, y, width, height));
    }

    fn move_relative(&mut self, dx: i32, dy: i32) {
        (self.0)(format!("move_rel {} {}", dx, dy));
    }

    fn button(&mut self, button: u32, press: bool) {
        (self.0)(format!("button {} {}", button, if press { "down" } else { "up" }));
    }

    fn key(&mut self, vk: u16, press: bool) {
        (self.0)(format!("key 0x{:02X} {}", vk, if press { "down" } else { "up" }));
    }

    fn key_position(&mut self, code: u32, press: bool) -> bool {
        (self.0)(format!("position {} {}", code, if press { "down" } else { "up" }));
        true
    }

    fn lock_screen(&mut self) {
        (self.0)("lock_screen".to_string());
    }

    fn text(&mut self, text: &str) {
        (self.0)(format!("text {:?}", text));
    }

    fn flush(&mut self) {}
}

/// The keycode space of map mode depends on the host's platform, which the
/// client learns from `PeerInfo.platform` (rustdesk/src/keyboard.rs
/// `_map_keyboard_mode`): a Linux host gets X keycodes, a Windows host scan
/// codes (message.proto `KeyEvent.chr`: "win: scancode, linux: key code").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HostOs {
    Linux,
    Windows,
}

impl HostOs {
    pub(crate) fn current() -> Self {
        if cfg!(target_os = "windows") {
            HostOs::Windows
        } else {
            HostOs::Linux
        }
    }
}

/// rustdesk/src/server/input_service.rs `KEY_MAP` (ControlKey -> enigo key),
/// then the virtual-key code enigo presses for that key on Windows
/// (rustdesk/libs/enigo/src/win/win_impl.rs `key_to_keycode`).
fn control_key_to_vk(ck: ControlKey) -> Option<u16> {
    use ControlKey::*;
    Some(match ck {
        Alt | Option | Menu => VK_MENU,
        Backspace => 0x08,
        CapsLock => 0x14,
        Control => VK_CONTROL,
        Delete => 0x2E,
        DownArrow => 0x28,
        End => 0x23,
        Escape => 0x1B,
        F1 => 0x70,
        F2 => 0x71,
        F3 => 0x72,
        F4 => 0x73,
        F5 => 0x74,
        F6 => 0x75,
        F7 => 0x76,
        F8 => 0x77,
        F9 => 0x78,
        F10 => 0x79,
        F11 => 0x7A,
        F12 => 0x7B,
        Home => 0x24,
        LeftArrow => 0x25,
        Meta => VK_LWIN,
        PageDown => 0x22,
        PageUp => 0x21,
        Return | NumpadEnter => VK_RETURN,
        RightArrow => 0x27,
        Shift => VK_SHIFT,
        Space => 0x20,
        Tab => 0x09,
        UpArrow => 0x26,
        Numpad0 => 0x60,
        Numpad1 => 0x61,
        Numpad2 => 0x62,
        Numpad3 => 0x63,
        Numpad4 => 0x64,
        Numpad5 => 0x65,
        Numpad6 => 0x66,
        Numpad7 => 0x67,
        Numpad8 => 0x68,
        Numpad9 => 0x69,
        Cancel => 0x03,
        Clear => 0x0C,
        Pause => 0x13,
        Kana | Hangul => 0x15,
        Junja => 0x17,
        Final => 0x18,
        Hanja | Kanji => 0x19,
        Convert => 0x1C,
        Select => 0x29,
        Print => 0x2A,
        Execute => 0x2B,
        Snapshot => 0x2C,
        Insert => 0x2D,
        Help => 0x2F,
        Sleep => 0x5F,
        Separator => 0x6C,
        Scroll => 0x91,
        NumLock => 0x90,
        RWin => VK_RWIN,
        Apps => 0x5D,
        Multiply => 0x6A,
        Add => 0x6B,
        Subtract => 0x6D,
        Decimal => 0x6E,
        Divide => 0x6F,
        // enigo presses the key of '='.
        Equals => 0xBB,
        RAlt => VK_RMENU,
        RControl => VK_RCONTROL,
        RShift => VK_RSHIFT,
        // Not in KEY_MAP.
        Unknown | VolumeMute | VolumeUp | VolumeDown | Power | CtrlAltDel | LockScreen => return None,
    })
}

/// The key of an ASCII character on a US keyboard, for hot keys (Ctrl+C
/// arrives as the character 'c' with Control among the modifiers). RustDesk
/// presses `Key::Layout(chr)` there, which enigo resolves through the host's
/// layout; the injector has no such lookup.
fn char_to_vk(c: char) -> Option<u16> {
    Some(match c {
        'a'..='z' => c.to_ascii_uppercase() as u16,
        'A'..='Z' | '0'..='9' => c as u16,
        ' ' => 0x20,
        '\r' | '\n' => VK_RETURN,
        '\t' => 0x09,
        ')' => 0x30,
        '!' => 0x31,
        '@' => 0x32,
        '#' => 0x33,
        '$' => 0x34,
        '%' => 0x35,
        '^' => 0x36,
        '&' => 0x37,
        '*' => 0x38,
        '(' => 0x39,
        ';' | ':' => 0xBA,
        '=' | '+' => 0xBB,
        ',' | '<' => 0xBC,
        '-' | '_' => 0xBD,
        '.' | '>' => 0xBE,
        '/' | '?' => 0xBF,
        '`' | '~' => 0xC0,
        '[' | '{' => 0xDB,
        '\\' | '|' => 0xDC,
        ']' | '}' => 0xDD,
        '\'' | '"' => 0xDE,
        _ => return None,
    })
}

/// The unsided virtual keys are the left keys.
fn normalize_vk(vk: u16) -> u16 {
    match vk {
        VK_SHIFT => VK_LSHIFT,
        VK_CONTROL => VK_LCONTROL,
        VK_MENU => VK_LMENU,
        other => other,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Modifier {
    Shift,
    Control,
    Alt,
    Meta,
}

/// A key this session holds down: pressed by its virtual-key code (legacy
/// mode) or at its position (map and translate mode; the code the injector
/// takes: the kernel's key code on Linux, the scan code on Windows).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Held {
    Vk(u16),
    Position(u32),
}

impl Held {
    /// Which modifier this key is, and whether it is the right-hand one
    /// (rustdesk/src/keyboard.rs `is_modifier`, on the key at a position).
    fn modifier(self, os: HostOs) -> Option<(Modifier, bool)> {
        Some(match (self, os) {
            (Held::Vk(VK_LSHIFT), _) => (Modifier::Shift, false),
            (Held::Vk(VK_RSHIFT), _) => (Modifier::Shift, true),
            (Held::Vk(VK_LCONTROL), _) => (Modifier::Control, false),
            (Held::Vk(VK_RCONTROL), _) => (Modifier::Control, true),
            (Held::Vk(VK_LMENU), _) => (Modifier::Alt, false),
            (Held::Vk(VK_RMENU), _) => (Modifier::Alt, true),
            (Held::Vk(VK_LWIN), _) => (Modifier::Meta, false),
            (Held::Vk(VK_RWIN), _) => (Modifier::Meta, true),
            // linux/input-event-codes.h: KEY_LEFTSHIFT 42, KEY_RIGHTSHIFT 54,
            // KEY_LEFTCTRL 29, KEY_RIGHTCTRL 97, KEY_LEFTALT 56,
            // KEY_RIGHTALT 100, KEY_LEFTMETA 125, KEY_RIGHTMETA 126.
            (Held::Position(42), HostOs::Linux) => (Modifier::Shift, false),
            (Held::Position(54), HostOs::Linux) => (Modifier::Shift, true),
            (Held::Position(29), HostOs::Linux) => (Modifier::Control, false),
            (Held::Position(97), HostOs::Linux) => (Modifier::Control, true),
            (Held::Position(56), HostOs::Linux) => (Modifier::Alt, false),
            (Held::Position(100), HostOs::Linux) => (Modifier::Alt, true),
            (Held::Position(125), HostOs::Linux) => (Modifier::Meta, false),
            (Held::Position(126), HostOs::Linux) => (Modifier::Meta, true),
            // PC scan codes (set 1): Shift 2A/36, Ctrl 1D/E01D, Alt 38/E038,
            // Windows keys E05B/E05C.
            (Held::Position(0x2A), HostOs::Windows) => (Modifier::Shift, false),
            (Held::Position(0x36), HostOs::Windows) => (Modifier::Shift, true),
            (Held::Position(0x1D), HostOs::Windows) => (Modifier::Control, false),
            (Held::Position(0xE01D), HostOs::Windows) => (Modifier::Control, true),
            (Held::Position(0x38), HostOs::Windows) => (Modifier::Alt, false),
            (Held::Position(0xE038), HostOs::Windows) => (Modifier::Alt, true),
            (Held::Position(0xE05B), HostOs::Windows) => (Modifier::Meta, false),
            (Held::Position(0xE05C), HostOs::Windows) => (Modifier::Meta, true),
            _ => return None,
        })
    }
}

/// The modifier a control key of `KeyEvent.modifiers` names, and its side.
fn control_key_modifier(ck: ControlKey) -> Option<(Modifier, bool)> {
    Some(match ck {
        ControlKey::Shift => (Modifier::Shift, false),
        ControlKey::RShift => (Modifier::Shift, true),
        ControlKey::Control => (Modifier::Control, false),
        ControlKey::RControl => (Modifier::Control, true),
        ControlKey::Alt => (Modifier::Alt, false),
        ControlKey::RAlt => (Modifier::Alt, true),
        ControlKey::Meta => (Modifier::Meta, false),
        ControlKey::RWin => (Modifier::Meta, true),
        _ => return None,
    })
}

/// rustdesk/src/common.rs `is_modifier`: a legacy key event for a modifier.
fn is_modifier_event(evt: &KeyEvent) -> bool {
    match evt.union {
        Some(key_event::Union::ControlKey(ck)) => {
            ControlKey::try_from(ck).ok().and_then(control_key_modifier).is_some()
        }
        _ => false,
    }
}

fn mode_of(evt: &KeyEvent) -> KeyboardMode {
    KeyboardMode::try_from(evt.mode).unwrap_or(KeyboardMode::Legacy)
}

fn has_modifier(modifiers: &[i32], ck: ControlKey) -> bool {
    modifiers.contains(&(ck as i32))
}

/// What became of the last key event, for the log: kinds only, never the
/// key, the character or the text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    None,
    PositionKey,
    VirtualKey,
    HotKey,
    Text,
    NotAvailable,
    Dropped,
}

/// What a session's input came to, for the log.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct InputStats {
    pub mouse_events: u64,
    pub key_events: u64,
    /// Key events with a code that has no key here.
    pub dropped_keys: u64,
}

/// The input of one session.
pub(crate) struct Input {
    injector: Box<dyn Injector>,
    os: HostOs,
    /// Size of the display the client was told (`PeerInfo.displays[0]`,
    /// at 0,0): the space of absolute pointer positions.
    width: i32,
    height: i32,
    /// Keys this session holds down.
    down: std::collections::BTreeSet<Held>,
    /// Windows, trackpad scrolling: what did not make a whole notch yet.
    wheel_rest: (i32, i32),
    action: Action,
    stats: InputStats,
}

impl Input {
    pub(crate) fn new(injector: Box<dyn Injector>, os: HostOs, width: u32, height: u32) -> Self {
        Input {
            injector,
            os,
            width: width as i32,
            height: height as i32,
            down: Default::default(),
            wheel_rest: (0, 0),
            action: Action::None,
            stats: InputStats::default(),
        }
    }

    /// The display shown changed: absolute positions are within its size.
    pub(crate) fn set_display(&mut self, width: u32, height: u32) {
        self.width = width as i32;
        self.height = height as i32;
    }

    pub(crate) fn stats(&self) -> InputStats {
        self.stats
    }

    /// A key by its virtual-key code (legacy mode's control keys and hot
    /// keys).
    fn key(&mut self, vk: u16, press: bool) {
        let vk = normalize_vk(vk);
        if press {
            self.down.insert(Held::Vk(vk));
        } else {
            self.down.remove(&Held::Vk(vk));
        }
        self.injector.key(vk, press);
        self.action = Action::VirtualKey;
    }

    fn click(&mut self, vk: u16) {
        self.key(vk, true);
        self.key(vk, false);
    }

    fn release_held(&mut self, held: Held) {
        self.down.remove(&held);
        match held {
            Held::Vk(vk) => self.injector.key(vk, false),
            Held::Position(code) => {
                self.injector.key_position(code, false);
            }
        }
    }

    /// rustdesk/src/server/input_service.rs `get_modifier_state`: either
    /// side of a modifier counts for both.
    fn modifier_state(&self, modifier: Modifier) -> bool {
        self.down.iter().any(|held| held.modifier(self.os).is_some_and(|(m, _)| m == modifier))
    }

    /// `is_hotkey_modifier_pressed`: Ctrl, Alt or Meta is held (Shift alone
    /// makes another character, not a hot key).
    fn hotkey_modifier_pressed(&self) -> bool {
        self.modifier_state(Modifier::Control) || self.modifier_state(Modifier::Alt) || self.modifier_state(Modifier::Meta)
    }

    /// `fix_modifiers`: a modifier that is down here but not among the
    /// event's modifiers is released, so both sides agree. `except` is the
    /// control key the event itself is about.
    fn fix_modifiers(&mut self, modifiers: &[i32], except: Option<ControlKey>) {
        let except = except.and_then(control_key_modifier);
        let held: Vec<Held> = self.down.iter().copied().collect();
        for key in held {
            let Some((modifier, right)) = key.modifier(self.os) else { continue };
            if except == Some((modifier, right)) {
                continue;
            }
            let generic = match modifier {
                Modifier::Shift => ControlKey::Shift,
                Modifier::Control => ControlKey::Control,
                Modifier::Alt => ControlKey::Alt,
                Modifier::Meta => ControlKey::Meta,
            };
            if has_modifier(modifiers, generic) {
                continue;
            }
            // `fix_modifier`, Windows: Ctrl held with Alt is AltGr.
            if self.os == HostOs::Windows && modifier == Modifier::Control && self.modifier_state(Modifier::Alt) {
                continue;
            }
            self.release_held(key);
        }
    }

    /// `press_modifiers` (and the same loop in `handle_mouse_simulation_`):
    /// the event's modifiers that are not down are pressed for its duration;
    /// returns them to release after. Caps Lock and Num Lock in the list are
    /// lock states, not keys to hold.
    fn press_modifiers(&mut self, modifiers: &[i32]) -> Vec<u16> {
        let mut to_release = Vec::new();
        for ck in modifiers.iter().filter_map(|m| ControlKey::try_from(*m).ok()) {
            let Some((modifier, _)) = control_key_modifier(ck) else { continue };
            let Some(vk) = control_key_to_vk(ck) else { continue };
            if !self.modifier_state(modifier) {
                self.key(vk, true);
                to_release.push(vk);
            }
        }
        to_release
    }

    fn release(&mut self, keys: Vec<u16>) {
        for vk in keys {
            self.key(vk, false);
        }
    }

    fn scroll(&mut self, button: u32, clicks: i32) {
        for _ in 0..clicks.min(MAX_WHEEL_CLICKS) {
            self.injector.button(button, true);
            self.injector.button(button, false);
        }
    }

    /// rustdesk/src/server/input_service.rs `handle_mouse_simulation_`.
    pub(crate) fn mouse(&mut self, evt: &MouseEvent) {
        self.stats.mouse_events += 1;
        let buttons = evt.mask >> 3;
        let evt_type = evt.mask & MOUSE_TYPE_MASK;
        let mut to_release = Vec::new();
        if evt_type == MOUSE_TYPE_DOWN {
            self.fix_modifiers(&evt.modifiers, None);
            to_release = self.press_modifiers(&evt.modifiers);
        }
        match evt_type {
            MOUSE_TYPE_MOVE => self.injector.move_absolute(evt.x, evt.y, self.width, self.height),
            MOUSE_TYPE_MOVE_RELATIVE => {
                let dx = evt.x.clamp(-MAX_RELATIVE_MOUSE_DELTA, MAX_RELATIVE_MOUSE_DELTA);
                let dy = evt.y.clamp(-MAX_RELATIVE_MOUSE_DELTA, MAX_RELATIVE_MOUSE_DELTA);
                self.injector.move_relative(dx, dy);
            }
            MOUSE_TYPE_DOWN | MOUSE_TYPE_UP => {
                let button = match buttons {
                    MOUSE_BUTTON_LEFT => Some(BUTTON_LEFT),
                    MOUSE_BUTTON_RIGHT => Some(BUTTON_RIGHT),
                    MOUSE_BUTTON_WHEEL => Some(BUTTON_MIDDLE),
                    MOUSE_BUTTON_BACK => Some(BUTTON_BACK),
                    MOUSE_BUTTON_FORWARD => Some(BUTTON_FORWARD),
                    _ => None,
                };
                if let Some(button) = button {
                    self.injector.button(button, evt_type == MOUSE_TYPE_DOWN);
                }
            }
            MOUSE_TYPE_WHEEL | MOUSE_TYPE_TRACKPAD => {
                // RustDesk: `x = -evt.x`, `y = evt.y`, off Windows `y = -y`;
                // then enigo: on Linux |y| clicks, up when y < 0, and |x|
                // clicks, left when x < 0; on Windows MOUSEEVENTF_WHEEL with
                // y (times WHEEL_DELTA unless a trackpad sent it), up when
                // positive, and MOUSEEVENTF_HWHEEL with x, right when
                // positive. Either way: evt.y > 0 scrolls up, evt.x > 0 left.
                let (up, left) = if self.os == HostOs::Windows && evt_type == MOUSE_TYPE_TRACKPAD {
                    // The injector scrolls by whole notches.
                    self.wheel_rest.0 += evt.x;
                    self.wheel_rest.1 += evt.y;
                    let notches = (self.wheel_rest.1 / WHEEL_DELTA, self.wheel_rest.0 / WHEEL_DELTA);
                    self.wheel_rest.0 %= WHEEL_DELTA;
                    self.wheel_rest.1 %= WHEEL_DELTA;
                    notches
                } else {
                    (evt.y, evt.x)
                };
                if up != 0 {
                    self.scroll(if up > 0 { BUTTON_WHEEL_UP } else { BUTTON_WHEEL_DOWN }, up.abs());
                }
                if left != 0 {
                    self.scroll(if left > 0 { BUTTON_WHEEL_LEFT } else { BUTTON_WHEEL_RIGHT }, left.abs());
                }
            }
            _ => {}
        }
        self.release(to_release);
        self.injector.flush();
    }

    /// rustdesk/src/server/input_service.rs `handle_pointer_`: of the touch
    /// events only the pinch does something on a desktop host, and only on
    /// Windows (`handle_scale`: Ctrl held while the wheel turns by `scale`,
    /// released at 0).
    pub(crate) fn pointer(&mut self, evt: &PointerDeviceEvent) {
        let Some(pointer_device_event::Union::TouchEvent(touch)) = &evt.union else { return };
        let Some(touch_event::Union::ScaleUpdate(update)) = &touch.union else { return };
        if self.os != HostOs::Windows {
            return;
        }
        if update.scale == 0 {
            self.key(VK_CONTROL, false);
        } else {
            self.key(VK_CONTROL, true);
            self.wheel_rest.1 += update.scale;
            let notches = self.wheel_rest.1 / WHEEL_DELTA;
            self.wheel_rest.1 %= WHEEL_DELTA;
            if notches != 0 {
                self.scroll(if notches > 0 { BUTTON_WHEEL_UP } else { BUTTON_WHEEL_DOWN }, notches.abs());
            }
        }
        self.injector.flush();
    }

    /// The injector's code for the key at a map-mode key code: on a Linux
    /// host the client sent the X keycode, which is the kernel's key code
    /// plus 8; on a Windows host the scan code, extended keys as 0xE0xx
    /// (rustdesk/src/server/input_service.rs `sim_rdev_rawkey_position`:
    /// `RawKey::LinuxXorgKeycode` / `RawKey::ScanCode`).
    fn position_of(&self, code: u32) -> Option<u32> {
        match self.os {
            HostOs::Linux => code.checked_sub(8).filter(|evdev| (1..=255).contains(evdev)),
            HostOs::Windows => ((1..=0xFF).contains(&code) || code >> 8 == 0xE0).then_some(code),
        }
    }

    /// Whether a map or translate mode key code is a modifier
    /// (rustdesk/src/keyboard.rs `keycode_to_rdev_key` then `is_modifier`).
    fn keycode_is_modifier(&self, code: u32) -> bool {
        self.position_of(code).is_some_and(|position| Held::Position(position).modifier(self.os).is_some())
    }

    /// One `KeyEvent` of the client: the `KeyEvent` branch of
    /// rustdesk/src/server/connection.rs `on_message`, then `handle_input`
    /// (`MessageInput::Key`) and input_service.rs `handle_key_`.
    pub(crate) fn key_event(&mut self, mut evt: KeyEvent) {
        self.stats.key_events += 1;
        self.action = Action::None;
        let key_is_modifier = match (mode_of(&evt), &evt.union) {
            (KeyboardMode::Map, Some(key_event::Union::Chr(code))) => self.keycode_is_modifier(*code),
            (KeyboardMode::Translate, Some(key_event::Union::Chr(code))) => self.keycode_is_modifier(*code & 0xFFFF),
            _ => false,
        };
        // "handle all down as press": on a Linux host RustDesk makes every
        // key-down but a modifier's a press and a release at once, in every
        // mode (rustdesk issue 6793: no key can stay down and repeat when
        // its key-up is late or lost). Off Linux only `press` is.
        //
        // NOT RustDesk's rule, on purpose: a map mode key is left out of
        // this. In map mode the client sends the keys as they are typed, a
        // key-down and a key-up each at its own time, and here they go to
        // the host one for one. A press and a release in the same instant is
        // not what a keyboard does, and an input method on the host that
        // edits the text by timing (Vietnamese Telex without a preedit:
        // backspaces, then the new letters) made wrong words of it. What
        // RustDesk's rule guards against is covered otherwise: every key
        // still down is released when the session ends (`release_all`), and
        // the client's auto-repeat does not press a held key again
        // (`raw_key`), the host repeats it.
        let map_key = mode_of(&evt) == KeyboardMode::Map && matches!(evt.union, Some(key_event::Union::Chr(_)));
        let is_press = if self.os == HostOs::Linux {
            (evt.press || evt.down) && !(is_modifier_event(&evt) || key_is_modifier || map_key)
        } else {
            evt.press
        };
        let press = is_press && !matches!(evt.union, Some(key_event::Union::Unicode(_) | key_event::Union::Seq(_)));
        if tracing::enabled!(tracing::Level::DEBUG) {
            // Kinds only: which key, character or text is never logged.
            let kind = match &evt.union {
                Some(key_event::Union::ControlKey(_)) => "control key",
                Some(key_event::Union::Chr(_)) => "key code",
                Some(key_event::Union::Unicode(_)) => "unicode",
                Some(key_event::Union::Seq(_)) => "text",
                Some(key_event::Union::Win2winHotkey(_)) => "win2win hot key",
                None => "nothing",
            };
            let (mode, down, with_modifiers, modifier) =
                (mode_of(&evt), evt.down, !evt.modifiers.is_empty(), is_modifier_event(&evt) || key_is_modifier);
            // `handle_input`: `press` is a key down followed by a key up.
            evt.press = false;
            if press {
                evt.down = true;
            }
            self.handle_key(&evt);
            let action = self.action;
            if press {
                evt.down = false;
                self.handle_key(&evt);
            }
            tracing::debug!(
                "RustDesk key event: mode={:?} kind={} down={} click={} modifier_key={} with_modifiers={} -> {:?}",
                mode,
                kind,
                down,
                press,
                modifier,
                with_modifiers,
                action
            );
        } else {
            evt.press = false;
            if press {
                evt.down = true;
            }
            self.handle_key(&evt);
            if press {
                evt.down = false;
                self.handle_key(&evt);
            }
        }
        self.injector.flush();
    }

    /// input_service.rs `handle_key_`.
    fn handle_key(&mut self, evt: &KeyEvent) {
        let changed = self.sync_locks(evt);
        match mode_of(evt) {
            KeyboardMode::Map => self.map_keyboard_mode(evt),
            KeyboardMode::Translate => self.translate_keyboard_mode(evt),
            _ => self.legacy_keyboard_mode(evt),
        }
        // `impl Drop for LockModesHandler`: the lock keys go back to what
        // they were, except on Wayland with uinput.
        if self.injector.restores_locks() {
            let action = self.action;
            for lock in changed.into_iter().flatten() {
                self.click_lock(lock);
            }
            self.action = action;
        }
    }

    /// Whether a key code is one the lock keys are not synchronised for
    /// (`skip_led_sync_rdev_key`: the modifiers, Tab and Return; a lone Meta
    /// must not be followed by a lock key click), and whether it is on the
    /// numeric keypad (rustdesk/src/keyboard.rs `is_numpad_rdev_key`).
    fn lock_class_of(&self, code: u32) -> (bool, bool) {
        let Some(position) = self.position_of(code) else { return (true, false) };
        if Held::Position(position).modifier(self.os).is_some() {
            return (true, false);
        }
        match self.os {
            // linux/input-event-codes.h: KEY_TAB 15, KEY_ENTER 28; the
            // keypad: KEY_KPASTERISK 55, KEY_KP7 71 .. KEY_KPDOT 83,
            // KEY_KPSLASH 98.
            HostOs::Linux => (matches!(position, 15 | 28), matches!(position, 55 | 71..=83 | 98)),
            // Scan codes: Tab 0F, Enter 1C; the keypad: 37, 47..53, E035.
            HostOs::Windows => (matches!(position, 0x0F | 0x1C), matches!(position, 0x37 | 0x47..=0x53 | 0xE035)),
        }
    }

    /// One press of a lock key.
    fn click_lock(&mut self, lock: Lock) {
        match (self.os, lock) {
            // KEY_CAPSLOCK 58, KEY_NUMLOCK 69.
            (HostOs::Linux, Lock::Caps) => {
                self.injector.key_position(58, true);
                self.injector.key_position(58, false);
            }
            (HostOs::Linux, Lock::Num) => {
                self.injector.key_position(69, true);
                self.injector.key_position(69, false);
            }
            // VK_CAPITAL, VK_NUMLOCK.
            (HostOs::Windows, Lock::Caps) => {
                self.injector.key(0x14, true);
                self.injector.key(0x14, false);
            }
            (HostOs::Windows, Lock::Num) => {
                self.injector.key(0x90, true);
                self.injector.key(0x90, false);
            }
        }
        self.injector.flush();
    }

    /// `LockModesHandler::sleep_to_ensure_locked`: on Wayland with uinput
    /// the new state takes a moment; at most 500 ms.
    fn wait_for_lock(&mut self, lock: Lock, wanted: bool) {
        if self.os != HostOs::Linux || self.injector.restores_locks() {
            return;
        }
        for _ in 0..50 {
            std::thread::sleep(std::time::Duration::from_millis(10));
            if self.injector.lock_state(lock) == Some(wanted) {
                break;
            }
        }
    }

    /// `LockModesHandler::new`: before a key is handled, Caps Lock here is
    /// made what the event says the client's is, and Num Lock for a keypad
    /// key (the client names the lock states among `KeyEvent.modifiers`).
    /// Returns the locks it changed. A state that cannot be read here is
    /// left alone.
    fn sync_locks(&mut self, evt: &KeyEvent) -> [Option<Lock>; 2] {
        let legacy = mode_of(evt) == KeyboardMode::Legacy;
        // `handle_key_`: for which events the handler is made, and whether
        // the key is a keypad key.
        let is_numpad_key = match &evt.union {
            Some(key_event::Union::Unicode(_) | key_event::Union::Seq(_)) => false,
            Some(key_event::Union::ControlKey(ck)) => {
                let ck = ControlKey::try_from(*ck).unwrap_or(ControlKey::Unknown);
                // `skip_led_sync_control_key`.
                if control_key_modifier(ck).is_some() || matches!(ck, ControlKey::Tab | ControlKey::Return) {
                    return [None, None];
                }
                // `is_numpad_control_key`.
                matches!(
                    ck,
                    ControlKey::Numpad0
                        | ControlKey::Numpad1
                        | ControlKey::Numpad2
                        | ControlKey::Numpad3
                        | ControlKey::Numpad4
                        | ControlKey::Numpad5
                        | ControlKey::Numpad6
                        | ControlKey::Numpad7
                        | ControlKey::Numpad8
                        | ControlKey::Numpad9
                        | ControlKey::NumpadEnter
                )
            }
            Some(key_event::Union::Chr(_)) if legacy => false,
            Some(key_event::Union::Chr(code)) => {
                let (skip, numpad) = self.lock_class_of(*code);
                if skip {
                    return [None, None];
                }
                numpad
            }
            _ => return [None, None],
        };
        let mut changed = [None, None];
        let event_caps = has_modifier(&evt.modifiers, ControlKey::CapsLock);
        if let Some(local_caps) = self.injector.lock_state(Lock::Caps) {
            if local_caps != event_caps {
                self.click_lock(Lock::Caps);
                self.wait_for_lock(Lock::Caps, event_caps);
                changed[0] = Some(Lock::Caps);
            }
        }
        if is_numpad_key {
            let event_num = has_modifier(&evt.modifiers, ControlKey::NumLock);
            if let Some(local_num) = self.injector.lock_state(Lock::Num) {
                if local_num != event_num {
                    self.click_lock(Lock::Num);
                    self.wait_for_lock(Lock::Num, event_num);
                    changed[1] = Some(Lock::Num);
                }
            }
        } else if legacy && self.os == HostOs::Windows {
            // `should_disable_numlock`: Home, the arrows and the like as
            // legacy control keys would arrive as keypad digits with Num
            // Lock on (`NUMPAD_KEY_MAP`).
            let navigation = matches!(
                evt.union,
                Some(key_event::Union::ControlKey(ck)) if matches!(
                    ControlKey::try_from(ck),
                    Ok(ControlKey::Home
                        | ControlKey::UpArrow
                        | ControlKey::PageUp
                        | ControlKey::LeftArrow
                        | ControlKey::RightArrow
                        | ControlKey::End
                        | ControlKey::DownArrow
                        | ControlKey::PageDown
                        | ControlKey::Insert
                        | ControlKey::Delete)
                )
            );
            if navigation && self.injector.lock_state(Lock::Num) == Some(true) {
                self.click_lock(Lock::Num);
                changed[1] = Some(Lock::Num);
            }
        }
        changed
    }

    /// `map_keyboard_mode` / `sim_rdev_rawkey_position`: the key at this
    /// position goes down or up, and nothing else: no layout, no virtual
    /// key, no text. An input method on the host sees the keys as typed.
    fn map_keyboard_mode(&mut self, evt: &KeyEvent) {
        let Some(key_event::Union::Chr(code)) = evt.union else { return };
        self.raw_key(code, evt.down);
    }

    fn raw_key(&mut self, code: u32, down: bool) {
        let Some(position) = self.position_of(code) else {
            self.dropped(down);
            return;
        };
        // A key-down for a key that is down is the client's auto-repeat
        // (Windows and X11 clients send it). A Linux host repeats a held key
        // itself, at its own rate; a Windows host does not repeat an
        // injected key, there every key-down is passed on.
        if down && self.os == HostOs::Linux && self.down.contains(&Held::Position(position)) {
            self.action = Action::PositionKey;
            return;
        }
        if !self.injector.key_position(position, down) {
            self.dropped(down);
            return;
        }
        if down {
            self.down.insert(Held::Position(position));
        } else {
            self.down.remove(&Held::Position(position));
        }
        self.action = Action::PositionKey;
    }

    fn dropped(&mut self, down: bool) {
        if down {
            self.stats.dropped_keys += 1;
        }
        self.action = Action::Dropped;
    }

    fn text(&mut self, text: &str) {
        self.injector.text(text);
        self.action = Action::Text;
    }

    /// `release_shift_for_char_input`: the client already applied Shift to
    /// the character it sends, so Shift is let go here (and not pressed
    /// again: the client's own key-up follows), unless a hot key is held.
    fn release_shift_for_char_input(&mut self) {
        if self.hotkey_modifier_pressed() {
            return;
        }
        let shifts: Vec<Held> =
            self.down.iter().copied().filter(|h| h.modifier(self.os).is_some_and(|(m, _)| m == Modifier::Shift)).collect();
        for held in shifts {
            self.release_held(held);
        }
    }

    /// `translate_keyboard_mode`.
    fn translate_keyboard_mode(&mut self, evt: &KeyEvent) {
        match &evt.union {
            Some(key_event::Union::Seq(seq)) => {
                // Text the client's layout made. With Ctrl/Alt/Meta held it
                // is a hot key: the keys of its characters are clicked
                // (Linux: `en.key_click(Key::Layout(chr))`, Windows:
                // `rdev::simulate_char`).
                if self.hotkey_modifier_pressed() {
                    for chr in seq.chars() {
                        match char_to_vk(chr) {
                            Some(vk) => self.click(vk),
                            None => self.injector.text(&chr.to_string()),
                        }
                    }
                    self.action = Action::HotKey;
                    return;
                }
                self.release_shift_for_char_input();
                self.text(seq);
            }
            Some(key_event::Union::Chr(code)) => match self.os {
                // `translate_process_code`: a scan code, or a virtual-key
                // code in the high word.
                HostOs::Windows => match code >> 16 {
                    0 => self.raw_key(*code, evt.down),
                    vk => self.key(vk as u16, evt.down),
                },
                HostOs::Linux => self.raw_key(*code, evt.down),
            },
            // `simulate_win2win_hotkey`: the virtual-key code is in the
            // high word (the low word is the character it makes).
            Some(key_event::Union::Win2winHotkey(code)) if self.os == HostOs::Windows => {
                self.key(((code >> 16) & 0xFFFF) as u16, evt.down);
            }
            // "Do not handle unicode for now", and nothing else is sent.
            _ => {}
        }
    }

    /// `legacy_keyboard_mode`.
    fn legacy_keyboard_mode(&mut self, evt: &KeyEvent) {
        let control_key = match evt.union {
            Some(key_event::Union::ControlKey(ck)) => ControlKey::try_from(ck).ok(),
            _ => None,
        };
        // `sync_modifiers`.
        let mut to_release = Vec::new();
        if evt.down {
            self.fix_modifiers(&evt.modifiers, control_key);
            to_release = self.press_modifiers(&evt.modifiers);
        }
        match &evt.union {
            Some(key_event::Union::ControlKey(_)) => match control_key {
                // `is_function_key`: these are not keys. `LockScreen` locks
                // the screen (`lock_screen_2`).
                Some(ControlKey::LockScreen) => {
                    if evt.down {
                        self.injector.lock_screen();
                    }
                    self.action = Action::VirtualKey;
                }
                // `CtrlAltDel` (what a client sends a Windows host): RustDesk
                // has its service send the secure attention sequence
                // (`send_sas`, which also edits the SoftwareSASGeneration
                // policy). This host does not; nothing is pressed, as on a
                // RustDesk host other than Windows.
                Some(ControlKey::CtrlAltDel) => {
                    if evt.down {
                        tracing::info!("RustDesk session: Ctrl+Alt+Del asked; not available on this host");
                    }
                    self.action = Action::NotAvailable;
                }
                // `process_control_key`.
                Some(ck) => match control_key_to_vk(ck) {
                    Some(vk) => self.key(vk, evt.down),
                    None => self.dropped(evt.down),
                },
                None => self.dropped(evt.down),
            },
            Some(key_event::Union::Chr(chr)) => {
                if self.os == HostOs::Linux {
                    self.release_shift_for_char_input();
                }
                self.process_chr(*chr, evt.down, has_hotkey_modifiers(&evt.modifiers));
            }
            Some(key_event::Union::Unicode(chr)) => {
                if self.os == HostOs::Linux {
                    self.release_shift_for_char_input();
                }
                // `process_unicode`.
                if let Some(c) = char::from_u32(*chr) {
                    self.text(&c.to_string());
                }
            }
            // `process_seq`.
            Some(key_event::Union::Seq(seq)) => self.text(seq),
            _ => {}
        }
        let action = self.action;
        self.release(to_release);
        self.action = action;
    }

    /// `process_chr`: a character key. As part of a hot key its key goes
    /// down and up; otherwise the character is typed when it goes down
    /// (RustDesk's own way on Windows, macOS and Wayland; on X11 it holds
    /// the character's key instead).
    fn process_chr(&mut self, chr: u32, down: bool, hotkey: bool) {
        let Some(c) = char::from_u32(chr) else { return };
        let hotkey = match self.os {
            HostOs::Linux => self.hotkey_modifier_pressed(),
            HostOs::Windows => hotkey,
        };
        if !down {
            // The key-up of a character whose key went down for a hot key
            // (the modifier may be up again by now).
            if let Some(vk) = char_to_vk(c).filter(|vk| self.down.contains(&Held::Vk(normalize_vk(*vk)))) {
                self.key(vk, false);
                self.action = Action::HotKey;
            }
            return;
        }
        match char_to_vk(c).filter(|_| hotkey) {
            Some(vk) => {
                self.key(vk, true);
                self.action = Action::HotKey;
            }
            None => self.text(&c.to_string()),
        }
    }

    /// Lets go of every key the session still holds (RustDesk:
    /// connection.rs `release_pressed_modifiers` and input_service.rs
    /// `fix_key_down_timeout` when a peer leaves keys down).
    pub(crate) fn release_all(&mut self) {
        let held: Vec<Held> = self.down.iter().copied().collect();
        for key in held {
            self.release_held(key);
        }
        self.injector.flush();
    }
}

/// input_service.rs `has_hotkey_modifiers`: Ctrl, Meta or Alt among the
/// event's modifiers.
fn has_hotkey_modifiers(modifiers: &[i32]) -> bool {
    [
        ControlKey::Control,
        ControlKey::RControl,
        ControlKey::Meta,
        ControlKey::RWin,
        ControlKey::Alt,
        ControlKey::RAlt,
    ]
    .iter()
    .any(|ck| has_modifier(modifiers, *ck))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::rustdesk::proto::{TouchEvent, TouchScaleUpdate};
    use std::sync::{Arc, Mutex};

    pub(crate) type Record = Arc<Mutex<Vec<String>>>;

    pub(crate) fn recorder() -> (Box<dyn Injector>, Record) {
        let record: Record = Arc::default();
        let sink = record.clone();
        (Box::new(RecordingInjector(Box::new(move |line| sink.lock().unwrap().push(line)))), record)
    }

    fn new_input(os: HostOs) -> (Input, Record) {
        let (injector, record) = recorder();
        (Input::new(injector, os, 1920, 1080), record)
    }

    fn take(record: &Record) -> Vec<String> {
        std::mem::take(&mut *record.lock().unwrap())
    }

    fn mouse(mask: i32, x: i32, y: i32, modifiers: &[ControlKey]) -> MouseEvent {
        MouseEvent { mask, x, y, modifiers: modifiers.iter().map(|m| *m as i32).collect() }
    }

    /// `mask` as the client builds it (rustdesk/src/ui_session_interface.rs
    /// `send_mouse`: type | button << 3).
    fn mask(evt_type: i32, button: i32) -> i32 {
        evt_type | (button << 3)
    }

    fn map_key(code: u32, down: bool) -> KeyEvent {
        KeyEvent {
            down,
            mode: KeyboardMode::Map as i32,
            union: Some(key_event::Union::Chr(code)),
            ..Default::default()
        }
    }

    fn legacy(union: key_event::Union, down: bool, press: bool, modifiers: &[ControlKey]) -> KeyEvent {
        KeyEvent {
            down,
            press,
            modifiers: modifiers.iter().map(|m| *m as i32).collect(),
            mode: KeyboardMode::Legacy as i32,
            union: Some(union),
        }
    }

    fn lines(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn left_click_and_drag() {
        let (mut input, record) = new_input(HostOs::Linux);
        input.mouse(&mouse(mask(MOUSE_TYPE_MOVE, 0), 100, 200, &[]));
        input.mouse(&mouse(mask(MOUSE_TYPE_DOWN, MOUSE_BUTTON_LEFT), 100, 200, &[]));
        input.mouse(&mouse(mask(MOUSE_TYPE_MOVE, 0), 400, 300, &[]));
        input.mouse(&mouse(mask(MOUSE_TYPE_UP, MOUSE_BUTTON_LEFT), 400, 300, &[]));
        assert_eq!(
            take(&record),
            lines(&["move_abs 100 200 of 1920x1080", "button 1 down", "move_abs 400 300 of 1920x1080", "button 1 up"])
        );
    }

    #[test]
    fn every_button_and_relative_moves() {
        let (mut input, record) = new_input(HostOs::Windows);
        for (button, number) in [
            (MOUSE_BUTTON_RIGHT, 3),
            (MOUSE_BUTTON_WHEEL, 2),
            (MOUSE_BUTTON_BACK, 8),
            (MOUSE_BUTTON_FORWARD, 9),
        ] {
            input.mouse(&mouse(mask(MOUSE_TYPE_DOWN, button), 0, 0, &[]));
            input.mouse(&mouse(mask(MOUSE_TYPE_UP, button), 0, 0, &[]));
            assert_eq!(take(&record), lines(&[&format!("button {} down", number), &format!("button {} up", number)]));
        }
        input.mouse(&mouse(mask(MOUSE_TYPE_MOVE_RELATIVE, 0), -7, 20000, &[]));
        assert_eq!(take(&record), lines(&["move_rel -7 10000"]));
    }

    #[test]
    fn the_wheel_turns_the_same_way_on_both_hosts() {
        for os in [HostOs::Linux, HostOs::Windows] {
            let (mut input, record) = new_input(os);
            // evt.y > 0: up; evt.y < 0: down, a click each.
            input.mouse(&mouse(mask(MOUSE_TYPE_WHEEL, 0), 0, 1, &[]));
            assert_eq!(take(&record), lines(&["button 4 down", "button 4 up"]));
            input.mouse(&mouse(mask(MOUSE_TYPE_WHEEL, 0), 0, -2, &[]));
            assert_eq!(take(&record), lines(&["button 5 down", "button 5 up", "button 5 down", "button 5 up"]));
            // Horizontal: evt.x > 0 left, evt.x < 0 right.
            input.mouse(&mouse(mask(MOUSE_TYPE_WHEEL, 0), 1, 0, &[]));
            assert_eq!(take(&record), lines(&["button 6 down", "button 6 up"]));
            input.mouse(&mouse(mask(MOUSE_TYPE_WHEEL, 0), -1, 0, &[]));
            assert_eq!(take(&record), lines(&["button 7 down", "button 7 up"]));
        }
        // A trackpad on a Windows host sends fine steps: whole notches only.
        let (mut input, record) = new_input(HostOs::Windows);
        input.mouse(&mouse(mask(MOUSE_TYPE_TRACKPAD, 0), 0, 70, &[]));
        assert!(take(&record).is_empty());
        input.mouse(&mouse(mask(MOUSE_TYPE_TRACKPAD, 0), 0, 70, &[]));
        assert_eq!(take(&record), lines(&["button 4 down", "button 4 up"]));
        // A flood of notches is cut short.
        input.mouse(&mouse(mask(MOUSE_TYPE_WHEEL, 0), 0, 1_000_000, &[]));
        assert_eq!(take(&record).len(), 2 * MAX_WHEEL_CLICKS as usize);
    }

    #[test]
    fn a_click_with_modifiers_holds_them_for_the_click() {
        // Ctrl+click from a phone: the modifier comes with the event.
        let (mut input, record) = new_input(HostOs::Linux);
        input.mouse(&mouse(mask(MOUSE_TYPE_DOWN, MOUSE_BUTTON_LEFT), 5, 5, &[ControlKey::Control]));
        assert_eq!(take(&record), lines(&["key 0xA2 down", "button 1 down", "key 0xA2 up"]));
        // Shift held by key events and among the click's modifiers (what a
        // desktop client sends for Shift+click) stays down.
        input.key_event(map_key(50, true));
        take(&record);
        input.mouse(&mouse(mask(MOUSE_TYPE_DOWN, MOUSE_BUTTON_LEFT), 5, 5, &[ControlKey::Shift]));
        assert_eq!(take(&record), lines(&["button 1 down"]));
        // Not among them: let go first (`fix_modifiers`), by the way it went
        // down.
        input.mouse(&mouse(mask(MOUSE_TYPE_DOWN, MOUSE_BUTTON_LEFT), 5, 5, &[]));
        assert_eq!(take(&record), lines(&["position 42 up", "button 1 down"]));
    }

    #[test]
    fn pinch_zooms_with_ctrl_and_the_wheel_on_windows_only() {
        let scale = |scale| PointerDeviceEvent {
            union: Some(pointer_device_event::Union::TouchEvent(TouchEvent {
                union: Some(touch_event::Union::ScaleUpdate(TouchScaleUpdate { scale })),
            })),
            ..Default::default()
        };
        let (mut input, record) = new_input(HostOs::Windows);
        input.pointer(&scale(240));
        input.pointer(&scale(0));
        assert_eq!(
            take(&record),
            lines(&["key 0xA2 down", "button 4 down", "button 4 up", "button 4 down", "button 4 up", "key 0xA2 up"])
        );
        let (mut input, record) = new_input(HostOs::Linux);
        input.pointer(&scale(240));
        assert!(take(&record).is_empty());
    }

    // Map mode, Linux host: the client sends X keycodes (evdev + 8,
    // rustdesk/src/keyboard.rs `_map_keyboard_mode`); the kernel's key code
    // is injected as it is, with no table in between.
    #[test]
    fn map_mode_on_a_linux_host() {
        let (mut input, record) = new_input(HostOs::Linux);
        // Ctrl+C: Control_L (37) down, c (54) down, up, Control up: the
        // same four events, each when the client sent it.
        input.key_event(map_key(37, true));
        input.key_event(map_key(54, true));
        input.key_event(map_key(54, false));
        input.key_event(map_key(37, false));
        assert_eq!(
            take(&record),
            lines(&["position 29 down", "position 46 down", "position 46 up", "position 29 up"])
        );
        // Every key code from 9 to 255 has a position, the ones the old
        // virtual-key tables lacked included: Caps Lock (66), Print (107),
        // Pause (127), Menu (135), the 102nd key (94).
        for code in [66u32, 107, 127, 135, 94, 9, 255] {
            input.key_event(map_key(code, true));
            assert_eq!(take(&record), lines(&[&format!("position {} down", code - 8)]), "{}", code);
            input.key_event(map_key(code, false));
            assert_eq!(take(&record), lines(&[&format!("position {} up", code - 8)]), "{}", code);
        }
        // Modifiers too: Shift (50, 62), Ctrl (37, 105), Alt (64), AltGr
        // (108), Super (133, 134).
        for code in [50u32, 62, 37, 105, 64, 108, 133, 134] {
            input.key_event(map_key(code, true));
            assert_eq!(take(&record), lines(&[&format!("position {} down", code - 8)]), "{}", code);
            input.key_event(map_key(code, false));
            assert_eq!(take(&record), lines(&[&format!("position {} up", code - 8)]), "{}", code);
        }
        // What cannot be a key code is dropped and counted.
        input.key_event(map_key(7, true));
        input.key_event(map_key(300, true));
        assert!(take(&record).is_empty());
        assert_eq!(input.stats().dropped_keys, 2);
    }

    /// The injector calls for key positions only, as (position, down).
    fn positions(record: &Record) -> Vec<(u32, bool)> {
        take(record)
            .iter()
            .map(|line| {
                let mut parts = line.split(' ');
                assert_eq!(parts.next(), Some("position"), "only raw keys expected, got {:?}", line);
                (parts.next().unwrap().parse().unwrap(), parts.next() == Some("down"))
            })
            .collect()
    }

    /// The keys that went down, in order.
    fn downs(calls: &[(u32, bool)]) -> Vec<u32> {
        calls.iter().filter(|(_, down)| *down).map(|(code, _)| *code).collect()
    }

    /// No key is down after `calls`.
    fn all_released(calls: &[(u32, bool)]) -> bool {
        let mut down = std::collections::BTreeSet::new();
        for (code, press) in calls {
            if *press {
                down.insert(*code);
            } else {
                down.remove(code);
            }
        }
        down.is_empty()
    }

    // X keycodes of a US keyboard's letters, as an X11 client reports them.
    fn x_keycode(c: char) -> u32 {
        const ROWS: [(&str, u32); 3] = [("qwertyuiop", 24), ("asdfghjkl", 38), ("zxcvbnm", 52)];
        for (row, first) in ROWS {
            if let Some(i) = row.find(c) {
                return first + i as u32;
            }
        }
        match c {
            ' ' => 65,
            _ => panic!("no key for {:?}", c),
        }
    }

    #[test]
    fn a_held_letter_repeats_and_ends_released() {
        // The client's auto-repeat: key-down again and again, one key-up.
        // A Linux host gets one key-down and the key-up, and repeats the
        // held key itself.
        let (mut input, record) = new_input(HostOs::Linux);
        for _ in 0..4 {
            input.key_event(map_key(38, true));
            assert!(input.down.contains(&Held::Position(30)));
        }
        input.key_event(map_key(38, false));
        assert_eq!(positions(&record), [(30, true), (30, false)]);
        assert!(input.down.is_empty());
        // A Windows host gets what the client sent, one for one.
        let (mut input, record) = new_input(HostOs::Windows);
        for _ in 0..4 {
            input.key_event(map_key(0x1E, true));
        }
        input.key_event(map_key(0x1E, false));
        assert_eq!(
            positions(&record),
            [(0x1E, true), (0x1E, true), (0x1E, true), (0x1E, true), (0x1E, false)]
        );
        assert!(input.down.is_empty());
    }

    #[test]
    fn overlapping_keys_keep_their_order() {
        // Fast typing: a down, b down, a up, b up.
        let (mut input, record) = new_input(HostOs::Linux);
        for (c, down) in [('a', true), ('b', true), ('a', false), ('b', false)] {
            input.key_event(map_key(x_keycode(c), down));
        }
        assert_eq!(positions(&record), [(30, true), (48, true), (30, false), (48, false)]);
        assert!(input.down.is_empty());
        // Shift+letter with the Shift released before the letter.
        input.key_event(map_key(50, true));
        input.key_event(map_key(x_keycode('t'), true));
        input.key_event(map_key(50, false));
        input.key_event(map_key(x_keycode('t'), false));
        assert_eq!(positions(&record), [(42, true), (20, true), (42, false), (20, false)]);
        // Ctrl held across several letters.
        input.key_event(map_key(37, true));
        for c in ['a', 'c', 'v'] {
            input.key_event(map_key(x_keycode(c), true));
            input.key_event(map_key(x_keycode(c), false));
        }
        input.key_event(map_key(37, false));
        let calls = positions(&record);
        assert_eq!(calls.first(), Some(&(29, true)));
        assert_eq!(calls.last(), Some(&(29, false)));
        assert_eq!(downs(&calls), [29, 30, 46, 47]);
        assert!(all_released(&calls) && input.down.is_empty());
        // AltGr held for a character: the same keys, the host's layout
        // makes the character.
        input.key_event(map_key(108, true));
        input.key_event(map_key(x_keycode('e'), true));
        input.key_event(map_key(x_keycode('e'), false));
        input.key_event(map_key(108, false));
        assert_eq!(positions(&record), [(100, true), (18, true), (18, false), (100, false)]);
        assert!(input.down.is_empty());
    }

    // Telex typed fast in map mode: every key-down arrives before the
    // previous key's key-up, and capitals come with Shift. What the host's
    // input method needs is the same keys in the same order, as keys.
    #[test]
    fn telex_typed_fast_arrives_as_the_same_keys_in_order() {
        for os in [HostOs::Linux, HostOs::Windows] {
            let (mut input, record) = new_input(os);
            // The code the client sends for a letter, and the position the
            // injector must get for it.
            let code = |c: char| match os {
                HostOs::Linux => x_keycode(c),
                HostOs::Windows => x_keycode(c) - 8,
            };
            let shift = match os {
                HostOs::Linux => 50,
                HostOs::Windows => 0x2A,
            };
            let position = |code: u32| match os {
                HostOs::Linux => code - 8,
                HostOs::Windows => code,
            };
            let text = "Tieengs Vieetj";
            let mut sent = Vec::new();
            let mut previous: Option<u32> = None;
            for ch in text.chars() {
                let key = code(ch.to_ascii_lowercase());
                if ch.is_ascii_uppercase() {
                    input.key_event(map_key(shift, true));
                    sent.push(position(shift));
                }
                // Overlap: the previous key is released only after this
                // one went down; the same key twice (ee) has to come up
                // first, as on a keyboard.
                let before = previous.filter(|p| *p == key);
                if let Some(before) = before {
                    input.key_event(map_key(before, false));
                }
                input.key_event(map_key(key, true));
                sent.push(position(key));
                if let Some(previous) = previous.replace(key).filter(|_| before.is_none()) {
                    input.key_event(map_key(previous, false));
                }
                if ch.is_ascii_uppercase() {
                    input.key_event(map_key(shift, false));
                }
            }
            input.key_event(map_key(previous.unwrap(), false));
            let calls = positions(&record);
            assert_eq!(downs(&calls), sent, "{:?}", os);
            assert!(all_released(&calls), "{:?}", os);
            assert!(input.down.is_empty());
            // One for one: as many key-ups as key-downs, and the first
            // letter is still down when the second goes down.
            assert_eq!(calls.len(), 2 * sent.len(), "{:?}", os);
            let first_up = calls.iter().position(|c| *c == (position(code('t')), false)).unwrap();
            let second_down = calls.iter().position(|c| *c == (position(code('i')), true)).unwrap();
            assert!(second_down < first_up, "{:?}", os);
            // Shift went up before the letter after the capital went down.
            let shift_up = calls.iter().position(|c| *c == (position(shift), false)).unwrap();
            let second_letter = calls.iter().position(|c| *c == (position(code('i')), true)).unwrap();
            assert!(shift_up < second_letter);
            assert_eq!(input.stats().dropped_keys, 0);
        }
    }

    // A client-side input method in translate mode rewrites a word: text,
    // backspaces, text. And a phone: text chunks between keys.
    #[test]
    fn text_and_keys_keep_their_order() {
        let (mut input, record) = new_input(HostOs::Linux);
        let text = |s: &str| KeyEvent {
            down: true,
            mode: KeyboardMode::Translate as i32,
            union: Some(key_event::Union::Seq(s.into())),
            ..Default::default()
        };
        let backspace = |down| KeyEvent {
            down,
            mode: KeyboardMode::Translate as i32,
            union: Some(key_event::Union::Chr(22)),
            ..Default::default()
        };
        input.key_event(text("vie"));
        input.key_event(backspace(true));
        input.key_event(backspace(false));
        input.key_event(backspace(true));
        input.key_event(backspace(false));
        input.key_event(text("iệ"));
        input.key_event(text("t"));
        input.key_event(backspace(true));
        input.key_event(text("t "));
        assert_eq!(
            take(&record),
            lines(&[
                "text \"vie\"",
                "position 14 down",
                "position 14 up",
                "position 14 up",
                "position 14 down",
                "position 14 up",
                "position 14 up",
                "text \"iệ\"",
                "text \"t\"",
                "position 14 down",
                "position 14 up",
                "text \"t \"",
            ])
        );
        // Legacy mode (phones): text, Enter, text.
        input.key_event(legacy(key_event::Union::Seq("Tiếng".into()), false, false, &[]));
        input.key_event(legacy(key_event::Union::ControlKey(ControlKey::Backspace as i32), false, true, &[]));
        input.key_event(legacy(key_event::Union::Seq("g Việt".into()), false, false, &[]));
        input.key_event(legacy(key_event::Union::ControlKey(ControlKey::Return as i32), false, true, &[]));
        assert_eq!(
            take(&record),
            lines(&["text \"Tiếng\"", "key 0x08 down", "key 0x08 up", "text \"g Việt\"", "key 0x0D down", "key 0x0D up"])
        );
        assert!(input.down.is_empty());
    }

    // Map mode, Windows host: scan codes, E0-prefixed ones as 0xE0xx, passed
    // on as they are with real key-down and key-up (`is_press` is only
    // `press` off Linux).
    #[test]
    fn map_mode_on_a_windows_host() {
        let (mut input, record) = new_input(HostOs::Windows);
        input.key_event(map_key(0x1D, true)); // Ctrl
        input.key_event(map_key(0x2E, true)); // C
        input.key_event(map_key(0x2E, false));
        input.key_event(map_key(0x1D, false));
        assert_eq!(positions(&record), [(0x1D, true), (0x2E, true), (0x2E, false), (0x1D, false)]);
        // Extended keys keep their E0: arrows, Delete, right Ctrl, AltGr,
        // the Windows key.
        for code in [0xE048u32, 0xE04B, 0xE04D, 0xE050, 0xE053, 0xE01D, 0xE038, 0xE05B, 0x3A, 0x45] {
            input.key_event(map_key(code, true));
            input.key_event(map_key(code, false));
            assert_eq!(positions(&record), [(code, true), (code, false)], "{:#x}", code);
        }
        // What is neither a plain nor an E0 code is dropped.
        input.key_event(map_key(0xE11D, true));
        input.key_event(map_key(0, true));
        assert!(take(&record).is_empty());
        assert_eq!(input.stats().dropped_keys, 2);
    }

    // Legacy mode: what phones and the client's toolbar send
    // (rustdesk/src/ui_session_interface.rs `_input_key`, `input_string`).
    #[test]
    fn legacy_mode() {
        let (mut input, record) = new_input(HostOs::Linux);
        // A phone's Ctrl+C: the character with Control as a modifier, as
        // a press.
        input.key_event(legacy(key_event::Union::Chr('c' as u32), false, true, &[ControlKey::Control]));
        assert_eq!(take(&record), lines(&["key 0xA2 down", "key 0x43 down", "key 0xA2 up", "key 0x43 up"]));
        // A plain character is typed.
        input.key_event(legacy(key_event::Union::Chr('a' as u32), false, true, &[]));
        assert_eq!(take(&record), lines(&["text \"a\""]));
        // Text from a phone keyboard, Vietnamese included, goes through
        // whole.
        input.key_event(legacy(key_event::Union::Seq("Tiếng Việt".into()), false, false, &[]));
        assert_eq!(take(&record), lines(&["text \"Tiếng Việt\""]));
        input.key_event(legacy(key_event::Union::Unicode('ệ' as u32), true, false, &[]));
        assert_eq!(take(&record), lines(&["text \"ệ\""]));
        // Control keys: arrows, F5, Enter, the Windows key.
        for (ck, vk) in [
            (ControlKey::UpArrow, "0x26"),
            (ControlKey::LeftArrow, "0x25"),
            (ControlKey::F5, "0x74"),
            (ControlKey::Return, "0x0D"),
            (ControlKey::Backspace, "0x08"),
            (ControlKey::Escape, "0x1B"),
            (ControlKey::Tab, "0x09"),
        ] {
            input.key_event(legacy(key_event::Union::ControlKey(ck as i32), false, true, &[]));
            assert_eq!(take(&record), lines(&[&format!("key {} down", vk), &format!("key {} up", vk)]), "{:?}", ck);
        }
        // Modifier keys are held and released as the client says.
        input.key_event(legacy(key_event::Union::ControlKey(ControlKey::Meta as i32), true, false, &[]));
        assert_eq!(take(&record), lines(&["key 0x5B down"]));
        input.key_event(legacy(key_event::Union::ControlKey(ControlKey::Meta as i32), false, false, &[]));
        assert_eq!(take(&record), lines(&["key 0x5B up"]));
        // Shift held while a character arrives: the character already has
        // the Shift in it.
        input.key_event(legacy(key_event::Union::ControlKey(ControlKey::Shift as i32), true, false, &[]));
        input.key_event(legacy(key_event::Union::Chr('A' as u32), true, false, &[ControlKey::Shift]));
        assert_eq!(take(&record), lines(&["key 0xA0 down", "key 0xA0 up", "text \"A\""]));
        // Lock states among the modifiers are not keys to press.
        input.key_event(legacy(key_event::Union::ControlKey(ControlKey::F5 as i32), false, true, &[ControlKey::CapsLock, ControlKey::NumLock]));
        assert_eq!(take(&record), lines(&["key 0x74 down", "key 0x74 up"]));
    }

    // Ctrl+Alt+Del as the client sends it (rustdesk/src/keyboard.rs
    // `event_ctrl_alt_del`).
    #[test]
    fn ctrl_alt_del() {
        // To a Linux host: Delete as a press with Alt and Control.
        let (mut input, record) = new_input(HostOs::Linux);
        input.key_event(legacy(
            key_event::Union::ControlKey(ControlKey::Delete as i32),
            false,
            true,
            &[ControlKey::Alt, ControlKey::Control],
        ));
        assert_eq!(
            take(&record),
            lines(&["key 0xA4 down", "key 0xA2 down", "key 0x2E down", "key 0xA4 up", "key 0xA2 up", "key 0x2E up"])
        );
        // To a Windows host: the CtrlAltDel control key, which is the secure
        // attention sequence there; no keys are pressed for it.
        let (mut input, record) = new_input(HostOs::Windows);
        input.key_event(legacy(key_event::Union::ControlKey(ControlKey::CtrlAltDel as i32), true, false, &[]));
        assert!(take(&record).is_empty());
        // The client's "Lock" (rustdesk/src/keyboard.rs `event_lock_screen`).
        input.key_event(legacy(key_event::Union::ControlKey(ControlKey::LockScreen as i32), true, false, &[]));
        assert_eq!(take(&record), lines(&["lock_screen"]));
    }

    // Translate mode (rustdesk/src/keyboard.rs `translate_keyboard_mode`):
    // text as `seq`, everything else as in map mode.
    #[test]
    fn translate_mode() {
        let translate = |union, down| KeyEvent { down, mode: KeyboardMode::Translate as i32, union: Some(union), ..Default::default() };
        let (mut input, record) = new_input(HostOs::Linux);
        // AltGr text: the client sends the character it made.
        input.key_event(translate(key_event::Union::Seq("€".into()), true));
        assert_eq!(take(&record), lines(&["text \"€\""]));
        // Shift+a: Shift went down as a key, the text is already "A".
        input.key_event(translate(key_event::Union::Chr(50), true));
        input.key_event(translate(key_event::Union::Seq("A".into()), true));
        input.key_event(translate(key_event::Union::Chr(50), false));
        assert_eq!(take(&record), lines(&["position 42 down", "position 42 up", "text \"A\"", "position 42 up"]));
        // Ctrl+c: with Control held the text is a hot key.
        input.key_event(translate(key_event::Union::Chr(37), true));
        input.key_event(translate(key_event::Union::Seq("c".into()), true));
        input.key_event(translate(key_event::Union::Chr(37), false));
        assert_eq!(take(&record), lines(&["position 29 down", "key 0x43 down", "key 0x43 up", "position 29 up"]));
        // Keys without text come as key codes.
        input.key_event(translate(key_event::Union::Chr(111), true));
        assert_eq!(take(&record), lines(&["position 103 down", "position 103 up"]));
        input.key_event(translate(key_event::Union::Unicode('x' as u32), true));
        assert!(take(&record).is_empty());

        // Windows host: a scan code, a virtual-key code in the high word,
        // and the Windows-to-Windows hot key.
        let (mut input, record) = new_input(HostOs::Windows);
        input.key_event(translate(key_event::Union::Chr(0xE048), true));
        input.key_event(translate(key_event::Union::Chr(0x41 << 16), true));
        input.key_event(translate(key_event::Union::Win2winHotkey((0x43 << 16) | 'c' as u32), true));
        input.key_event(translate(key_event::Union::Win2winHotkey(0x43 << 16), false));
        assert_eq!(take(&record), lines(&["position 57416 down", "key 0x41 down", "key 0x43 down", "key 0x43 up"]));
    }

    /// An injector with lock keys: the lock key's own key toggles its state.
    struct LockingInjector {
        record: Record,
        locks: std::sync::Arc<Mutex<(Option<bool>, Option<bool>)>>,
        restores: bool,
    }

    impl Injector for LockingInjector {
        fn move_absolute(&mut self, _: i32, _: i32, _: i32, _: i32) {}
        fn move_relative(&mut self, _: i32, _: i32) {}
        fn button(&mut self, _: u32, _: bool) {}
        fn key(&mut self, vk: u16, press: bool) {
            let mut locks = self.locks.lock().unwrap();
            if press && vk == 0x14 {
                locks.0 = locks.0.map(|on| !on);
            }
            if press && vk == 0x90 {
                locks.1 = locks.1.map(|on| !on);
            }
            self.record.lock().unwrap().push(format!("key 0x{:02X} {}", vk, if press { "down" } else { "up" }));
        }
        fn key_position(&mut self, code: u32, press: bool) -> bool {
            let mut locks = self.locks.lock().unwrap();
            if press && code == 58 {
                locks.0 = locks.0.map(|on| !on);
            }
            if press && code == 69 {
                locks.1 = locks.1.map(|on| !on);
            }
            self.record.lock().unwrap().push(format!("position {} {}", code, if press { "down" } else { "up" }));
            true
        }
        fn text(&mut self, text: &str) {
            self.record.lock().unwrap().push(format!("text {:?}", text));
        }
        fn flush(&mut self) {}
        fn lock_state(&mut self, lock: Lock) -> Option<bool> {
            let locks = self.locks.lock().unwrap();
            match lock {
                Lock::Caps => locks.0,
                Lock::Num => locks.1,
            }
        }
        fn restores_locks(&self) -> bool {
            self.restores
        }
    }

    fn locking_input(os: HostOs, caps: Option<bool>, num: Option<bool>, restores: bool) -> (Input, Record) {
        let record: Record = Arc::default();
        let injector = LockingInjector { record: record.clone(), locks: Arc::new(Mutex::new((caps, num))), restores };
        (Input::new(Box::new(injector), os, 1920, 1080), record)
    }

    fn map_key_with(code: u32, down: bool, modifiers: &[ControlKey]) -> KeyEvent {
        KeyEvent { modifiers: modifiers.iter().map(|m| *m as i32).collect(), ..map_key(code, down) }
    }

    // rustdesk/src/server/input_service.rs `LockModesHandler`.
    #[test]
    fn lock_keys_follow_the_client() {
        // X11 or Windows: Caps Lock is switched for the key and put back.
        let (mut input, record) = locking_input(HostOs::Linux, Some(false), Some(true), true);
        // A letter with the client's Caps Lock on (X keycode 38).
        input.key_event(map_key_with(38, true, &[ControlKey::CapsLock]));
        assert_eq!(
            take(&record),
            lines(&["position 58 down", "position 58 up", "position 30 down", "position 58 down", "position 58 up"])
        );
        // Both off: nothing but the key.
        input.key_event(map_key_with(38, false, &[]));
        assert_eq!(take(&record), lines(&["position 30 up"]));
        // A keypad key (KP_1, X keycode 87) with the client's Num Lock off
        // while it is on here.
        input.key_event(map_key_with(87, true, &[]));
        assert_eq!(
            take(&record),
            lines(&["position 69 down", "position 69 up", "position 79 down", "position 69 down", "position 69 up"])
        );
        input.key_event(map_key_with(87, false, &[ControlKey::NumLock]));
        assert_eq!(take(&record), lines(&["position 79 up"]));
        // Modifiers, Tab and Return never touch the locks.
        for code in [50u32, 37, 133, 23, 36] {
            input.key_event(map_key_with(code, true, &[ControlKey::CapsLock]));
            input.key_event(map_key_with(code, false, &[ControlKey::CapsLock]));
            assert!(take(&record).iter().all(|l| !l.contains(" 58 ") && !l.contains(" 69 ")), "{}", code);
        }

        // Wayland with uinput: switched and left so.
        let (mut input, record) = locking_input(HostOs::Linux, Some(false), None, false);
        input.key_event(map_key_with(38, true, &[ControlKey::CapsLock]));
        input.key_event(map_key_with(38, false, &[ControlKey::CapsLock]));
        assert_eq!(take(&record), lines(&["position 58 down", "position 58 up", "position 30 down", "position 30 up"]));
        // Num Lock cannot be read there (None): keypad keys leave it alone.
        input.key_event(map_key_with(87, true, &[ControlKey::NumLock]));
        assert_eq!(take(&record), lines(&["position 58 down", "position 58 up", "position 79 down"]));

        // Windows: by virtual key; and a legacy Home with Num Lock on has it
        // switched off for the key (`should_disable_numlock`).
        let (mut input, record) = locking_input(HostOs::Windows, Some(false), Some(true), true);
        input.key_event(legacy(key_event::Union::ControlKey(ControlKey::Home as i32), true, false, &[]));
        assert_eq!(take(&record), lines(&["key 0x90 down", "key 0x90 up", "key 0x24 down", "key 0x90 down", "key 0x90 up"]));
        input.key_event(map_key_with(0x1E, true, &[ControlKey::CapsLock]));
        assert_eq!(take(&record), lines(&["key 0x14 down", "key 0x14 up", "position 30 down", "key 0x14 down", "key 0x14 up"]));

        // Nothing known about the locks: nothing done about them.
        let (mut input, record) = locking_input(HostOs::Linux, None, None, true);
        input.key_event(map_key_with(38, true, &[ControlKey::CapsLock]));
        assert_eq!(take(&record), lines(&["position 30 down"]));
    }

    #[test]
    fn keys_left_down_are_released_at_the_end() {
        let (mut input, record) = new_input(HostOs::Windows);
        input.key_event(map_key(0x1D, true));
        input.key_event(map_key(0x1E, true));
        // A legacy modifier, with Ctrl among its modifiers so Ctrl stays.
        input.key_event(legacy(key_event::Union::ControlKey(ControlKey::Meta as i32), true, false, &[ControlKey::Control]));
        take(&record);
        input.release_all();
        assert_eq!(take(&record), lines(&["key 0x5B up", "position 29 up", "position 30 up"]));
        input.release_all();
        assert!(take(&record).is_empty());
    }
}
