// Keys are named by where they sit, not by what the layout prints on them:
// Raw Input hands over the scan code, which is the same on every layout, and
// a hotkey should stay on the same key when someone switches to Arabic or
// French. The names are the US layout's, which is what most keyboards print
// for the letters and what games use in their own key settings.

use std::fmt;
use std::str::FromStr;

// Scan code set 1 make code, with the prefix byte the keyboard sends before
// it (0xE0 or 0xE1) in the high byte. So Left Ctrl is 0x001D and Right Ctrl,
// which sends E0 1D, is 0xE01D.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Key(u16);

const E0: u16 = 0xE000;
const E1: u16 = 0xE100;

impl Key {
    pub const ESC: Key = Key(0x01);
    pub const TAB: Key = Key(0x0F);
    pub const I: Key = Key(0x17);
    pub const D: Key = Key(0x20);
    pub const M: Key = Key(0x32);
    pub const S: Key = Key(0x1F);
    pub const SPACE: Key = Key(0x39);
    pub const F4: Key = Key(0x3E);
    pub const F11: Key = Key(0x57);
    pub const END: Key = Key(E0 | 0x4F);
    pub const LEFT_CTRL: Key = Key(0x1D);
    pub const RIGHT_CTRL: Key = Key(E0 | 0x1D);
    pub const LEFT_SHIFT: Key = Key(0x2A);
    pub const RIGHT_SHIFT: Key = Key(0x36);
    pub const LEFT_ALT: Key = Key(0x38);
    pub const RIGHT_ALT: Key = Key(E0 | 0x38);
    pub const LEFT_WIN: Key = Key(E0 | 0x5B);
    pub const RIGHT_WIN: Key = Key(E0 | 0x5C);
    pub const DELETE: Key = Key(E0 | 0x53);
    pub const PAUSE: Key = Key(E1 | 0x1D);

    // `prefix` is 0, 0xE0 or 0xE1; `code` the make code without it.
    pub const fn from_scan(prefix: u8, code: u8) -> Key {
        Key((prefix as u16) << 8 | code as u16)
    }

    pub fn scan(self) -> u16 {
        self.0
    }

    // Which modifier this key is, Left or Right alike.
    pub fn modifier(self) -> Modifiers {
        match self {
            Key::LEFT_CTRL | Key::RIGHT_CTRL => Modifiers::CTRL,
            Key::LEFT_SHIFT | Key::RIGHT_SHIFT => Modifiers::SHIFT,
            Key::LEFT_ALT | Key::RIGHT_ALT => Modifiers::ALT,
            Key::LEFT_WIN | Key::RIGHT_WIN => Modifiers::WIN,
            _ => Modifiers::NONE,
        }
    }

    pub fn is_modifier(self) -> bool {
        self.modifier() != Modifiers::NONE
    }

    fn name(self) -> Option<&'static str> {
        NAMES
            .iter()
            .find(|(code, _)| *code == self.0)
            .map(|(_, name)| *name)
    }

    fn parse(text: &str) -> Option<Key> {
        if let Some((code, _)) = NAMES
            .iter()
            .find(|(_, name)| name.eq_ignore_ascii_case(text))
        {
            return Some(Key(*code));
        }
        // "Key 56", "Key E0 22": what a key without a name is written as.
        let mut words = text.split_whitespace();
        if !words.next()?.eq_ignore_ascii_case("key") {
            return None;
        }
        let first = byte(words.next()?)?;
        let key = match words.next() {
            // E0 and E1 alone are the prefixes, never keys.
            None if first == 0xE0 || first == 0xE1 => return None,
            None => Key::from_scan(0, first),
            Some(code) if first == 0xE0 || first == 0xE1 => Key::from_scan(first, byte(code)?),
            Some(_) => return None,
        };
        let usable = words.next().is_none() && key.0 & 0xFF != 0;
        usable.then_some(key)
    }
}

// Two hex digits, as the generic names write a byte.
fn byte(text: &str) -> Option<u8> {
    if text.len() != 2 {
        return None;
    }
    u8::from_str_radix(text, 16).ok()
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(name) = self.name() {
            return f.write_str(name);
        }
        match self.0 >> 8 {
            0 => write!(f, "Key {:02X}", self.0 & 0xFF),
            prefix => write!(f, "Key {prefix:02X} {:02X}", self.0 & 0xFF),
        }
    }
}

// Ctrl, Shift, Alt and Win, either side.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Modifiers(u8);

impl Modifiers {
    pub const NONE: Modifiers = Modifiers(0);
    pub const CTRL: Modifiers = Modifiers(1);
    pub const SHIFT: Modifiers = Modifiers(2);
    pub const ALT: Modifiers = Modifiers(4);
    pub const WIN: Modifiers = Modifiers(8);

    // In the order a chord is written.
    const WORDS: [(Modifiers, &'static str); 4] = [
        (Modifiers::CTRL, "Ctrl"),
        (Modifiers::SHIFT, "Shift"),
        (Modifiers::ALT, "Alt"),
        (Modifiers::WIN, "Win"),
    ];

    pub const fn with(self, other: Modifiers) -> Modifiers {
        Modifiers(self.0 | other.0)
    }

    pub fn contains(self, other: Modifiers) -> bool {
        self.0 & other.0 == other.0
    }

    fn parse(word: &str) -> Option<Modifiers> {
        let word = word.to_ascii_lowercase();
        Some(match word.as_str() {
            "ctrl" | "control" => Modifiers::CTRL,
            "shift" => Modifiers::SHIFT,
            "alt" => Modifiers::ALT,
            "win" | "windows" => Modifiers::WIN,
            _ => return None,
        })
    }
}

// One key, pressed while the modifiers are held.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Chord {
    pub modifiers: Modifiers,
    pub key: Key,
}

impl Chord {
    pub const fn new(modifiers: Modifiers, key: Key) -> Chord {
        Chord { modifiers, key }
    }

    // Windows acts on these itself. It keeps every chord with the Windows
    // key for its own shortcuts, and the rest switch windows or open Start,
    // Task Manager or the security screen, so most of them move focus to
    // another window as they are pressed.
    pub fn taken_by_windows(self) -> bool {
        let with = |modifier| self.modifiers.contains(modifier);
        with(Modifiers::WIN)
            || self.key.modifier() == Modifiers::WIN
            || (self.key == Key::TAB && with(Modifiers::ALT))
            || (self.key == Key::ESC && (with(Modifiers::ALT) || with(Modifiers::CTRL)))
            || (self.key == Key::DELETE && with(Modifiers::CTRL.with(Modifiers::ALT)))
    }
}

// "Ctrl+Shift+M", "Right Ctrl": what the panel shows and settings.txt keeps.
impl fmt::Display for Chord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (modifier, word) in Modifiers::WORDS {
            if self.modifiers.contains(modifier) {
                write!(f, "{word}+")?;
            }
        }
        write!(f, "{}", self.key)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParseError {
    NoKey,
    Gap,
    NotAKey(String),
    NotAModifier(String),
    Twice(&'static str),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::NoKey => f.write_str("there is no key after the last +"),
            ParseError::Gap => f.write_str("two + have nothing between them"),
            ParseError::NotAKey(word) => write!(f, "{word} is not a key Booth knows"),
            ParseError::NotAModifier(word) => write!(
                f,
                "{word} comes before a + but is not Ctrl, Shift, Alt or Win"
            ),
            ParseError::Twice(word) => write!(f, "{word} is in it twice"),
        }
    }
}

impl std::error::Error for ParseError {}

// Case and the spaces around each part do not matter, so "ctrl + shift + m"
// typed in Notepad is Ctrl+Shift+M.
impl FromStr for Chord {
    type Err = ParseError;

    fn from_str(text: &str) -> Result<Chord, ParseError> {
        let mut parts: Vec<&str> = text.split('+').map(str::trim).collect();
        let key = parts.pop().unwrap_or_default();
        let mut modifiers = Modifiers::NONE;
        for part in parts {
            if part.is_empty() {
                return Err(ParseError::Gap);
            }
            let modifier =
                Modifiers::parse(part).ok_or_else(|| ParseError::NotAModifier(part.to_owned()))?;
            if modifiers.contains(modifier) {
                let word = Modifiers::WORDS
                    .iter()
                    .find(|(each, _)| *each == modifier)
                    .map_or("a modifier", |(_, word)| *word);
                return Err(ParseError::Twice(word));
            }
            modifiers = modifiers.with(modifier);
        }
        if key.is_empty() {
            return Err(ParseError::NoKey);
        }
        let key = Key::parse(key).ok_or_else(|| ParseError::NotAKey(key.to_owned()))?;
        Ok(Chord { modifiers, key })
    }
}

// US layout names. Nothing here contains a +, which separates the parts of
// a chord; the keypad's plus key is Num Plus for that reason.
const NAMES: &[(u16, &str)] = &[
    (0x01, "Esc"),
    (0x02, "1"),
    (0x03, "2"),
    (0x04, "3"),
    (0x05, "4"),
    (0x06, "5"),
    (0x07, "6"),
    (0x08, "7"),
    (0x09, "8"),
    (0x0A, "9"),
    (0x0B, "0"),
    (0x0C, "-"),
    (0x0D, "="),
    (0x0E, "Backspace"),
    (0x0F, "Tab"),
    (0x10, "Q"),
    (0x11, "W"),
    (0x12, "E"),
    (0x13, "R"),
    (0x14, "T"),
    (0x15, "Y"),
    (0x16, "U"),
    (0x17, "I"),
    (0x18, "O"),
    (0x19, "P"),
    (0x1A, "["),
    (0x1B, "]"),
    (0x1C, "Enter"),
    (0x1D, "Left Ctrl"),
    (0x1E, "A"),
    (0x1F, "S"),
    (0x20, "D"),
    (0x21, "F"),
    (0x22, "G"),
    (0x23, "H"),
    (0x24, "J"),
    (0x25, "K"),
    (0x26, "L"),
    (0x27, ";"),
    (0x28, "'"),
    (0x29, "`"),
    (0x2A, "Left Shift"),
    (0x2B, "\\"),
    (0x2C, "Z"),
    (0x2D, "X"),
    (0x2E, "C"),
    (0x2F, "V"),
    (0x30, "B"),
    (0x31, "N"),
    (0x32, "M"),
    (0x33, ","),
    (0x34, "."),
    (0x35, "/"),
    (0x36, "Right Shift"),
    (0x37, "Num *"),
    (0x38, "Left Alt"),
    (0x39, "Space"),
    (0x3A, "Caps Lock"),
    (0x3B, "F1"),
    (0x3C, "F2"),
    (0x3D, "F3"),
    (0x3E, "F4"),
    (0x3F, "F5"),
    (0x40, "F6"),
    (0x41, "F7"),
    (0x42, "F8"),
    (0x43, "F9"),
    (0x44, "F10"),
    (0x45, "Num Lock"),
    (0x46, "Scroll Lock"),
    (0x47, "Num 7"),
    (0x48, "Num 8"),
    (0x49, "Num 9"),
    (0x4A, "Num -"),
    (0x4B, "Num 4"),
    (0x4C, "Num 5"),
    (0x4D, "Num 6"),
    (0x4E, "Num Plus"),
    (0x4F, "Num 1"),
    (0x50, "Num 2"),
    (0x51, "Num 3"),
    (0x52, "Num 0"),
    (0x53, "Num ."),
    (0x57, "F11"),
    (0x58, "F12"),
    (0x64, "F13"),
    (0x65, "F14"),
    (0x66, "F15"),
    (0x67, "F16"),
    (0x68, "F17"),
    (0x69, "F18"),
    (0x6A, "F19"),
    (0x6B, "F20"),
    (0x6C, "F21"),
    (0x6D, "F22"),
    (0x6E, "F23"),
    (0x76, "F24"),
    (E0 | 0x1C, "Num Enter"),
    (E0 | 0x1D, "Right Ctrl"),
    (E0 | 0x35, "Num /"),
    (E0 | 0x37, "Print Screen"),
    (E0 | 0x38, "Right Alt"),
    // Ctrl+Pause sends this instead of the Pause sequence.
    (E0 | 0x46, "Break"),
    (E0 | 0x47, "Home"),
    (E0 | 0x48, "Up"),
    (E0 | 0x49, "Page Up"),
    (E0 | 0x4B, "Left"),
    (E0 | 0x4D, "Right"),
    (E0 | 0x4F, "End"),
    (E0 | 0x50, "Down"),
    (E0 | 0x51, "Page Down"),
    (E0 | 0x52, "Insert"),
    (E0 | 0x53, "Delete"),
    (E0 | 0x5B, "Left Win"),
    (E0 | 0x5C, "Right Win"),
    (E0 | 0x5D, "Menu"),
    (E1 | 0x1D, "Pause"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_name_is_one_key_and_reads_back() {
        for (code, name) in NAMES {
            assert!(!name.contains('+'), "{name}");
            assert_eq!(Key::parse(name), Some(Key(*code)), "{name}");
            assert_eq!(Key(*code).to_string(), *name);
            let same = NAMES.iter().filter(|(_, other)| other == name).count();
            assert_eq!(same, 1, "{name}");
        }
    }

    #[test]
    fn a_key_without_a_name_is_written_by_its_scan_code() {
        let iso = Key::from_scan(0, 0x56);
        assert_eq!(iso.to_string(), "Key 56");
        assert_eq!(Key::parse("key 56"), Some(iso));
        let media = Key::from_scan(0xE0, 0x22);
        assert_eq!(media.to_string(), "Key E0 22");
        assert_eq!(Key::parse("Key e0 22"), Some(media));
        for bad in [
            "Key",
            "Key 5",
            "Key 123",
            "Key 56 00",
            "Key E0",
            "Key 20 22",
            "Key 00",
            "Key zz",
        ] {
            assert_eq!(Key::parse(bad), None, "{bad}");
        }
    }
}
