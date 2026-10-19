// How loud this PC plays each person, in volumes.txt beside settings.txt:
// one line for each person turned up or down, their key in hex and the
// percent. Someone at 100 has no line, and a line Booth cannot read is
// skipped. Nothing in it leaves this PC.

use std::path::{Path, PathBuf};

use room::MOST_VOLUME;

use crate::settings::{self, SaveError};

const FILE: &str = "volumes.txt";
// The most people the file keeps; the ones changed longest ago go first.
const MOST_PEOPLE: usize = 256;

#[derive(Default)]
pub struct Volumes {
    // None for a panel with no data folder, or a file it could not read,
    // which saves nothing.
    dir: Option<PathBuf>,
    // The last one changed first.
    people: Vec<([u8; 32], u8)>,
    unsaved: bool,
}

impl Volumes {
    // Unreadable, it is as if no one was turned up or down, with the reason,
    // and nothing is written over the people in it.
    pub fn load(dir: &Path) -> (Volumes, Option<String>) {
        let path = dir.join(FILE);
        match settings::read(&path) {
            Ok(bytes) => {
                let volumes = Volumes {
                    dir: Some(dir.to_owned()),
                    people: parse(&bytes),
                    unsaved: false,
                };
                (volumes, None)
            }
            Err(err) => (
                Volumes::default(),
                Some(format!(
                    "could not read {}: {err}; volumes changed now are not saved",
                    path.display()
                )),
            ),
        }
    }

    pub fn get(&self, key: &[u8; 32]) -> u8 {
        self.people
            .iter()
            .find(|(have, _)| have == key)
            .map_or(100, |&(_, percent)| percent)
    }

    pub fn set(&mut self, key: [u8; 32], percent: u8) {
        let percent = percent.min(MOST_VOLUME);
        if self.get(&key) == percent {
            return;
        }
        self.people.retain(|(have, _)| *have != key);
        if percent != 100 {
            self.people.insert(0, (key, percent));
            self.people.truncate(MOST_PEOPLE);
        }
        self.unsaved = true;
    }

    pub fn each(&self) -> impl Iterator<Item = ([u8; 32], u8)> + '_ {
        self.people.iter().copied()
    }

    // Only when something changed since the last save, and once: a disk
    // that refused it is tried again at the next change, not every frame.
    pub fn save(&mut self) -> Result<(), SaveError> {
        let Some(dir) = self.dir.as_deref().filter(|_| self.unsaved) else {
            return Ok(());
        };
        self.unsaved = false;
        let mut text = String::new();
        for (key, percent) in &self.people {
            for byte in key {
                text.push_str(&format!("{byte:02x}"));
            }
            text.push_str(&format!(" {percent}\r\n"));
        }
        settings::replace(dir, &dir.join(FILE), text.as_bytes())
    }
}

// A room left with a person's menu still open has not written it yet.
impl Drop for Volumes {
    fn drop(&mut self) {
        let _ = self.save();
    }
}

fn parse(bytes: &[u8]) -> Vec<([u8; 32], u8)> {
    let mut people: Vec<([u8; 32], u8)> = Vec::new();
    let text = String::from_utf8_lossy(bytes);
    // Notepad has written a byte order mark in front in the past.
    for line in text.strip_prefix('\u{feff}').unwrap_or(&text).lines() {
        let mut parts = line.split_whitespace();
        let (Some(key), Some(percent), None) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        let (Some(key), Ok(percent)) = (key_from_hex(key), percent.parse::<u16>()) else {
            continue;
        };
        let percent = percent.min(u16::from(MOST_VOLUME)) as u8;
        if percent == 100 || people.iter().any(|(have, _)| *have == key) {
            continue;
        }
        people.push((key, percent));
        if people.len() == MOST_PEOPLE {
            break;
        }
    }
    people
}

fn key_from_hex(text: &str) -> Option<[u8; 32]> {
    let bytes = text.as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let mut key = [0u8; 32];
    for (at, pair) in bytes.chunks(2).enumerate() {
        let pair = std::str::from_utf8(pair).ok()?;
        key[at] = u8::from_str_radix(pair, 16).ok()?;
    }
    Some(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("booth-volumes-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn kept_across_a_restart() {
        let dir = folder("kept");
        let (mut volumes, problem) = Volumes::load(&dir);
        assert_eq!(problem, None);
        assert_eq!(volumes.get(&[7; 32]), 100);
        volumes.set([7; 32], 40);
        volumes.set([9; 32], 0);
        volumes.set([3; 32], 200);
        volumes.set([3; 32], 100);
        volumes.save().unwrap();
        let text = std::fs::read_to_string(dir.join(FILE)).unwrap();
        assert_eq!(text.lines().count(), 2, "{text}");
        assert!(
            text.starts_with(&format!("{} 0\r\n", "09".repeat(32))),
            "{text}"
        );
        let (again, _) = Volumes::load(&dir);
        assert_eq!(
            (
                again.get(&[7; 32]),
                again.get(&[9; 32]),
                again.get(&[3; 32])
            ),
            (40, 0, 100)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn written_when_the_room_goes_with_a_menu_open() {
        let dir = folder("left");
        let (mut volumes, _) = Volumes::load(&dir);
        volumes.set([5; 32], 80);
        drop(volumes);
        assert_eq!(Volumes::load(&dir).0.get(&[5; 32]), 80);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lines_it_cannot_read_are_skipped() {
        let good = "ab".repeat(32);
        let text = format!(
            "{good} 70\r\nnot a key 50\r\n{} 20\r\n{good}\r\n{good} 30\r\n{} 999\n\n{} 100\n",
            "ab".repeat(31),
            "cd".repeat(32),
            "ef".repeat(32),
        );
        let people = parse(text.as_bytes());
        assert_eq!(people, [([0xab; 32], 70), ([0xcd; 32], 150)]);
    }

    #[test]
    fn a_byte_order_mark_is_not_part_of_the_first_key() {
        let text = format!(
            "\u{feff}{} 70\r\n{} 20\r\n",
            "ab".repeat(32),
            "cd".repeat(32)
        );
        assert_eq!(parse(text.as_bytes()), [([0xab; 32], 70), ([0xcd; 32], 20)]);
    }

    #[test]
    fn unreadable_file_kept() {
        let dir = folder("unreadable");
        // Windows will not open a folder as a file, which stands in for a
        // file another program holds open without sharing.
        std::fs::create_dir(dir.join(FILE)).unwrap();
        let (mut volumes, problem) = Volumes::load(&dir);
        let problem = problem.expect("a reason");
        assert!(
            problem.ends_with("; volumes changed now are not saved"),
            "{problem}"
        );
        volumes.set([7; 32], 40);
        assert_eq!(volumes.get(&[7; 32]), 40);
        volumes.save().unwrap();
        drop(volumes);
        assert!(dir.join(FILE).is_dir());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn at_most_256_people() {
        let dir = folder("most");
        let (mut volumes, _) = Volumes::load(&dir);
        for n in 0..=300u16 {
            let mut key = [0u8; 32];
            key[..2].copy_from_slice(&n.to_le_bytes());
            volumes.set(key, 50);
        }
        volumes.save().unwrap();
        let (again, _) = Volumes::load(&dir);
        assert_eq!(again.each().count(), MOST_PEOPLE);
        let mut newest = [0u8; 32];
        newest[..2].copy_from_slice(&300u16.to_le_bytes());
        assert_eq!(again.get(&newest), 50);
        assert_eq!(again.get(&[0; 32]), 100);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
