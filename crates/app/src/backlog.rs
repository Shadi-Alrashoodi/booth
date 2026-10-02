// Log lines from outside a room: the firewall step, the settings and the
// update check. A room writes them when it opens the log; under --log, an
// update check outside a room writes them too (follow_update in app.rs).

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

use crate::win;

#[derive(Default)]
pub struct Backlog {
    // Each with the time it happened.
    lines: Vec<(String, String)>,
    // Whether earlier lines already went into the log.
    logged: bool,
}

impl Backlog {
    pub fn add(&mut self, text: String) {
        self.lines.push((win::utc_stamp(), text));
    }

    // The room writes the log from its own first line on, and the first
    // lines here happened before any room, so they go in just ahead of the
    // room's lines with their own times. A line that comes later happened
    // while the log already had newer lines, so it goes in at the time of
    // writing with its own time in the text, and the log stays in order. A
    // failure is left to the room, which opens the same file next and says
    // what went wrong, and the lines wait for the next room.
    pub fn write_log(&mut self, path: &Path, role: &str) {
        if self.lines.is_empty() {
            return;
        }
        let now = win::utc_stamp();
        let lines: String = self
            .lines
            .iter()
            .map(|(stamp, text)| {
                let text = printable(text);
                if self.logged {
                    format!("{now} {role:<6} {text} (at {stamp})\r\n")
                } else {
                    format!("{stamp} {role:<6} {text}\r\n")
                }
            })
            .collect();
        let wrote = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .and_then(|mut file| file.write_all(lines.as_bytes()));
        if wrote.is_ok() {
            self.lines.clear();
            self.logged = true;
        }
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    #[cfg(test)]
    pub fn texts(&self) -> Vec<&str> {
        self.lines.iter().map(|(_, text)| text.as_str()).collect()
    }
}

// Windows' error text can end in a line break, and one log line is one line.
fn printable(text: &str) -> String {
    text.trim()
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_logged_once_in_order() {
        let path = std::env::temp_dir().join(format!("booth-backlog-{}.log", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut backlog = Backlog::default();
        backlog.lines.push((
            String::from("2020-01-01T09:35:32.927Z"),
            String::from("first"),
        ));
        backlog.write_log(&path, "host");
        backlog.write_log(&path, "host");
        backlog.lines.push((
            String::from("2020-01-01T09:35:33.000Z"),
            String::from("late\r\n"),
        ));
        backlog.write_log(&path, "client");
        let log = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        let lines: Vec<&str> = log.lines().collect();
        assert_eq!(lines.len(), 2, "{log}");
        assert_eq!(lines[0], "2020-01-01T09:35:32.927Z host   first");
        assert!(
            lines[1].ends_with(" client late (at 2020-01-01T09:35:33.000Z)"),
            "{log}"
        );
        assert!(lines[1] > lines[0], "{log}");
    }

    #[test]
    fn a_log_that_cannot_be_written_keeps_the_lines() {
        let mut backlog = Backlog::default();
        backlog.add(String::from("kept"));
        let nowhere = std::env::temp_dir()
            .join("booth-no-such-dir")
            .join("x")
            .join("booth.log");
        backlog.write_log(&nowhere, "host");
        assert_eq!(backlog.texts(), ["kept"]);
        assert!(!backlog.logged);
    }
}
