// How loud this PC plays each person, in volumes.txt beside settings.txt:
// one line for each person turned up or down, their key in hex and the
// percent. Someone at 100 has no line, and a line Booth cannot read is
// skipped. Nothing in it leaves this PC. The PC sound of their shares has
// sound-volumes.txt, the same way: a third column in volumes.txt would have
// a copy from before PC sound skip the whole line.

use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use room::MOST_VOLUME;

use crate::settings::{self, SaveError};

// The most people the file keeps; the ones changed longest ago go first.
const MOST_PEOPLE: usize = 256;
// What Booth writes is at most MOST_PEOPLE lines of 70 bytes; a file much
// bigger is not its own, and is not read whole.
const MOST_BYTES: u64 = 64 * 1024;

// Which file: each person's voice, or the PC sound of their shares.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Kind {
    #[default]
    Voice,
    PcSound,
}

impl Kind {
    pub fn file(self) -> &'static str {
        match self {
            Kind::Voice => "volumes.txt",
            Kind::PcSound => "sound-volumes.txt",
        }
    }

    // What is not saved when the file cannot be read or written.
    pub fn what(self) -> &'static str {
        match self {
            Kind::Voice => "voice volumes",
            Kind::PcSound => "PC sound volumes",
        }
    }
}

// Holds no path: the line made from it can stay for the whole room, where a
// share of the panel's monitor shows it.
#[derive(Debug)]
pub enum Problem {
    // Read as the room opened; it holds for the room.
    Read { kind: Kind, source: io::Error },
    // Writing the new copy beside the file: a full disk, most likely.
    Write { kind: Kind, source: io::Error },
    // Putting the new copy in the file's place: a program holding it, most
    // likely.
    Replace { kind: Kind, source: io::Error },
}

#[derive(Default)]
pub struct Volumes {
    kind: Kind,
    // None for a panel with no data folder, or a file it could not read,
    // which saves nothing.
    dir: Option<PathBuf>,
    // The last one changed first.
    people: Vec<([u8; 32], u8)>,
    unsaved: bool,
    // Why the file could not be read, which holds for the room, or why the
    // last save failed, until one goes through.
    problem: Option<Problem>,
    // The same failure with the whole path, for the log, until the app takes
    // it.
    log: Option<String>,
}

impl Volumes {
    // Unreadable, it is as if no one was turned up or down, with the reason,
    // and nothing is written over the people in it.
    pub fn load(dir: &Path, kind: Kind) -> Volumes {
        let path = dir.join(kind.file());
        match read(&path) {
            Ok(bytes) => Volumes {
                kind,
                dir: Some(dir.to_owned()),
                people: parse(&bytes),
                unsaved: false,
                problem: None,
                log: None,
            },
            Err(err) => {
                let log = format!(
                    "volumes: could not read {}: {err}; {} changed now are not saved",
                    path.display(),
                    kind.what()
                );
                Volumes {
                    kind,
                    dir: None,
                    people: Vec::new(),
                    unsaved: false,
                    problem: Some(Problem::Read { kind, source: err }),
                    log: Some(log),
                }
            }
        }
    }

    // What the panel says while it holds.
    pub fn problem(&self) -> Option<&Problem> {
        self.problem.as_ref()
    }

    pub fn take_log(&mut self) -> Option<String> {
        self.log.take()
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
    pub fn save(&mut self) {
        let Some(dir) = self.dir.as_deref().filter(|_| self.unsaved) else {
            return;
        };
        self.unsaved = false;
        let mut text = String::new();
        for (key, percent) in &self.people {
            for byte in key {
                text.push_str(&format!("{byte:02x}"));
            }
            text.push_str(&format!(" {percent}\r\n"));
        }
        let err = match settings::replace(dir, &dir.join(self.kind.file()), text.as_bytes()) {
            Ok(()) => {
                self.problem = None;
                return;
            }
            Err(err) => err,
        };
        self.log = Some(format!("volumes: {err}; tried again at the next change"));
        let kind = self.kind;
        self.problem = Some(match err {
            SaveError::Write { source, .. } => Problem::Write { kind, source },
            // replace() never reads the file first, so Read cannot come from
            // it.
            SaveError::Read { source, .. } | SaveError::Replace { source, .. } => {
                Problem::Replace { kind, source }
            }
        });
    }
}

// A room left with a person's menu still open has not written it yet.
impl Drop for Volumes {
    fn drop(&mut self) {
        self.save();
    }
}

// Nothing there reads as an empty file.
fn read(path: &Path) -> io::Result<Vec<u8>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err),
    };
    let mut bytes = Vec::new();
    file.take(MOST_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MOST_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::FileTooLarge,
            "it is over 64 KB, more than Booth ever writes",
        ));
    }
    Ok(bytes)
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
        let mut volumes = Volumes::load(&dir, Kind::Voice);
        assert!(volumes.problem().is_none());
        assert_eq!(volumes.get(&[7; 32]), 100);
        volumes.set([7; 32], 40);
        volumes.set([9; 32], 0);
        volumes.set([3; 32], 200);
        volumes.set([3; 32], 100);
        volumes.save();
        assert!(volumes.problem().is_none());
        let text = std::fs::read_to_string(dir.join("volumes.txt")).unwrap();
        assert_eq!(text.lines().count(), 2, "{text}");
        assert!(
            text.starts_with(&format!("{} 0\r\n", "09".repeat(32))),
            "{text}"
        );
        let again = Volumes::load(&dir, Kind::Voice);
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
        let mut volumes = Volumes::load(&dir, Kind::Voice);
        volumes.set([5; 32], 80);
        drop(volumes);
        assert_eq!(Volumes::load(&dir, Kind::Voice).get(&[5; 32]), 80);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // A person's PC sound volume has a file of its own, which their voice's
    // never reads: the same key in both keeps both.
    #[test]
    fn pc_sound_has_its_own_file() {
        let dir = folder("sound");
        let mut voice = Volumes::load(&dir, Kind::Voice);
        let mut sound = Volumes::load(&dir, Kind::PcSound);
        assert!(sound.problem().is_none());
        voice.set([7; 32], 40);
        sound.set([7; 32], 0);
        voice.save();
        sound.save();
        let text = std::fs::read_to_string(dir.join("sound-volumes.txt")).unwrap();
        assert_eq!(text, format!("{} 0\r\n", "07".repeat(32)));
        let voice = Volumes::load(&dir, Kind::Voice);
        let sound = Volumes::load(&dir, Kind::PcSound);
        assert_eq!((voice.get(&[7; 32]), sound.get(&[7; 32])), (40, 0));
        std::fs::remove_file(dir.join("sound-volumes.txt")).unwrap();
        std::fs::create_dir(dir.join("sound-volumes.txt")).unwrap();
        let mut unreadable = Volumes::load(&dir, Kind::PcSound);
        assert!(
            matches!(
                unreadable.problem(),
                Some(Problem::Read {
                    kind: Kind::PcSound,
                    ..
                })
            ),
            "{:?}",
            unreadable.problem()
        );
        let log = unreadable.take_log().expect("a log line");
        let path = dir.join("sound-volumes.txt");
        assert!(
            log.starts_with(&format!("volumes: could not read {}: ", path.display())),
            "{log}"
        );
        assert!(
            log.ends_with("; PC sound volumes changed now are not saved"),
            "{log}"
        );
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
        std::fs::create_dir(dir.join("volumes.txt")).unwrap();
        let mut volumes = Volumes::load(&dir, Kind::Voice);
        let read = |volumes: &Volumes| {
            matches!(
                volumes.problem(),
                Some(Problem::Read {
                    kind: Kind::Voice,
                    ..
                })
            )
        };
        assert!(read(&volumes), "{:?}", volumes.problem());
        let log = volumes.take_log().expect("a log line");
        assert!(
            log.ends_with("; voice volumes changed now are not saved"),
            "{log}"
        );
        volumes.set([7; 32], 40);
        assert_eq!(volumes.get(&[7; 32]), 40);
        volumes.save();
        assert!(read(&volumes), "it holds: {:?}", volumes.problem());
        assert_eq!(volumes.take_log(), None);
        drop(volumes);
        assert!(dir.join("volumes.txt").is_dir());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // A file far bigger than Booth writes is not read, and nothing is
    // written over it.
    #[test]
    fn a_file_too_big_is_kept() {
        let dir = folder("big");
        let line = format!("{} 50\r\n", "ab".repeat(32));
        let big = line.repeat(30_000);
        std::fs::write(dir.join("sound-volumes.txt"), &big).unwrap();
        let mut volumes = Volumes::load(&dir, Kind::PcSound);
        match volumes.problem() {
            Some(Problem::Read {
                kind: Kind::PcSound,
                source,
            }) => assert_eq!(source.kind(), io::ErrorKind::FileTooLarge, "{source}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(volumes.get(&[0xab; 32]), 100);
        volumes.set([7; 32], 40);
        volumes.save();
        drop(volumes);
        let kept = std::fs::read_to_string(dir.join("sound-volumes.txt")).unwrap();
        assert!(kept == big, "written over");
        let at_most = line.repeat(256);
        std::fs::write(dir.join("sound-volumes.txt"), &at_most).unwrap();
        assert!(Volumes::load(&dir, Kind::PcSound).problem().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // A save that fails says so, with which volumes, until one goes
    // through.
    #[test]
    fn a_failed_save_says_so_until_one_goes_through() {
        let dir = folder("failed");
        let mut volumes = Volumes::load(&dir, Kind::PcSound);
        volumes.set([7; 32], 40);
        // A folder where the file goes: the rename over it fails.
        std::fs::create_dir(dir.join("sound-volumes.txt")).unwrap();
        volumes.save();
        let replace = |volumes: &Volumes| {
            matches!(
                volumes.problem(),
                Some(Problem::Replace {
                    kind: Kind::PcSound,
                    ..
                })
            )
        };
        assert!(replace(&volumes), "{:?}", volumes.problem());
        let log = volumes.take_log().expect("a log line");
        let path = dir.join("sound-volumes.txt");
        assert!(
            log.starts_with(&format!("volumes: could not replace {}: ", path.display())),
            "{log}"
        );
        assert!(log.ends_with("; tried again at the next change"), "{log}");
        assert!(!log.contains("with the new settings"), "{log}");
        // As the room asks each frame while the menu is closed.
        volumes.save();
        assert!(replace(&volumes), "it holds: {:?}", volumes.problem());
        assert_eq!(volumes.take_log(), None);
        std::fs::remove_dir(dir.join("sound-volumes.txt")).unwrap();
        volumes.set([7; 32], 50);
        volumes.save();
        assert!(volumes.problem().is_none());
        assert_eq!(volumes.take_log(), None);
        drop(volumes);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // The new copy cannot be written beside the file, as on a full disk:
    // that is a problem of its own, and only the log names the temporary
    // file.
    #[test]
    fn a_full_disk_is_a_write_not_a_replace() {
        let dir = folder("full");
        let mut volumes = Volumes::load(&dir, Kind::Voice);
        volumes.set([7; 32], 40);
        // A folder where the new copy goes stands in for a full disk.
        let temp = dir.join(format!("volumes.{}.tmp", std::process::id()));
        std::fs::create_dir(&temp).unwrap();
        volumes.save();
        assert!(
            matches!(
                volumes.problem(),
                Some(Problem::Write {
                    kind: Kind::Voice,
                    ..
                })
            ),
            "{:?}",
            volumes.problem()
        );
        let log = volumes.take_log().expect("a log line");
        assert!(
            log.starts_with(&format!("volumes: could not write {}: ", temp.display())),
            "{log}"
        );
        std::fs::remove_dir(&temp).unwrap();
        drop(volumes);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn at_most_256_people() {
        let dir = folder("most");
        let mut volumes = Volumes::load(&dir, Kind::Voice);
        for n in 0..=300u16 {
            let mut key = [0u8; 32];
            key[..2].copy_from_slice(&n.to_le_bytes());
            volumes.set(key, 50);
        }
        volumes.save();
        assert!(volumes.problem().is_none());
        let again = Volumes::load(&dir, Kind::Voice);
        assert_eq!(again.each().count(), MOST_PEOPLE);
        let mut newest = [0u8; 32];
        newest[..2].copy_from_slice(&300u16.to_le_bytes());
        assert_eq!(again.get(&newest), 50);
        assert_eq!(again.get(&[0; 32]), 100);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
