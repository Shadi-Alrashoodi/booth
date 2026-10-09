// settings.txt in the data folder: one `key = value` per line, so it can be
// read and fixed in Notepad. Save rewrites only the lines of the keys this
// version knows and keeps every other line byte for byte, so a key from a
// newer version, or one typed by hand, survives a save from this one.

use std::fmt;
use std::fs::{self, File};
use std::io::{self, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use input::{Action, Bindings, Chord};
use invite::BuildError;
use room::TalkMode;
use voice::audio::{Choice, Direction};

const FILE: &str = "settings.txt";
const NAME: &str = "name";
const PORT: &str = "port";
const STUN_SERVERS: &str = "stun_servers";
const ADDRESS_NAME: &str = "address_name";
const INPUT_DEVICE: &str = "input_device";
const OUTPUT_DEVICE: &str = "output_device";
const TALK_MODE: &str = "talk_mode";
const CONSTANT_RATE_VOICE: &str = "constant_rate_voice";
const HOTKEY_PUSH_TO_TALK: &str = "hotkey_push_to_talk";
const HOTKEY_MUTE: &str = "hotkey_mute";
const HOTKEY_DEAFEN: &str = "hotkey_deafen";
const HOTKEY_SHARE: &str = "hotkey_share";
const HOTKEY_PANIC: &str = "hotkey_panic";
const HOTKEY_SHOW_PANEL: &str = "hotkey_show_panel";
const HOTKEY_STATS_PANEL: &str = "hotkey_stats_panel";
// Not video_upload or viewer_vsync, which the tests below use as keys a
// newer version could write.
const VIDEO_UPLOAD_MBITS: &str = "video_upload_mbits";
const VSYNC_IN_VIEWER: &str = "vsync_in_viewer";
const HIDE_STRIP_IN_FULLSCREEN: &str = "hide_strip_in_fullscreen";
const SHARE_MONITOR: &str = "share_monitor";
// Off unless the user turns it on, since a check shows GitHub this PC's
// address. Only on is written down.
const CHECK_FOR_NEW_VERSIONS: &str = "check_for_new_versions";
// Every key this version knows, in the order a save adds the missing ones.
const KEYS: [&str; 20] = [
    NAME,
    PORT,
    STUN_SERVERS,
    ADDRESS_NAME,
    HOTKEY_PUSH_TO_TALK,
    HOTKEY_MUTE,
    HOTKEY_DEAFEN,
    HOTKEY_SHARE,
    HOTKEY_PANIC,
    HOTKEY_SHOW_PANEL,
    HOTKEY_STATS_PANEL,
    INPUT_DEVICE,
    OUTPUT_DEVICE,
    TALK_MODE,
    CONSTANT_RATE_VOICE,
    VIDEO_UPLOAD_MBITS,
    VSYNC_IN_VIEWER,
    HIDE_STRIP_IN_FULLSCREEN,
    SHARE_MONITOR,
    CHECK_FOR_NEW_VERSIONS,
];
const PUSH_TO_TALK: &str = "push_to_talk";
const OPEN_MIC: &str = "open_mic";
const BOM: &[u8] = b"\xEF\xBB\xBF";

pub const DEFAULT_PORT: u16 = 41000;
// Windows hands the ports below this to its own services.
pub const LOWEST_PORT: u16 = 1024;
// What the room keeps of a name, so the field shows no more than that.
pub const NAME_CHARS: usize = 32;
// The video upload, in whole Mbit/s.
pub const DEFAULT_UPLOAD_MBITS: u32 = room::DEFAULT_VIDEO_UPLOAD_KBPS / 1000;
pub const MOST_UPLOAD_MBITS: u32 = room::MAX_VIDEO_UPLOAD_KBPS / 1000;
pub const LEAST_UPLOAD_MBITS: u32 = 1;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Settings {
    // None means the Windows user name.
    name: Option<String>,
    // None means DEFAULT_PORT, which is never written down.
    port: Option<u16>,
    // None means net's defaults. Some of an empty list is no STUN at all.
    stun_servers: Option<Vec<String>>,
    // Always None or a name invite::check_hostname accepts.
    address_name: Option<String>,
    // Endpoint ids. None means Windows default, which is never written down.
    input_device: Option<String>,
    output_device: Option<String>,
    // Push to talk and constant-rate voice, the defaults, are never
    // written down.
    open_mic: bool,
    variable_rate: bool,
    // Only the ones off their defaults are written down.
    hotkeys: Bindings,
    // None means DEFAULT_UPLOAD_MBITS. Vsync and the strip's hide are off
    // by default, and only on is written down.
    upload_mbits: Option<u32>,
    vsync: bool,
    hide_strip: bool,
    // The Windows name of the monitor picked last in Share's list, for the
    // share key. Written from inside a room, when it is picked.
    share_monitor: Option<String>,
    check_for_new_versions: bool,
}

impl Settings {
    // A missing file is a first run. Anything else that goes wrong costs the
    // setting it touches and a log line, never the start: a settings file
    // is no reason for Booth not to open.
    pub fn load(dir: &Path) -> (Settings, Vec<String>) {
        let path = dir.join(FILE);
        match read(&path) {
            Ok(bytes) => parse(&bytes),
            Err(err) => (
                Settings::default(),
                vec![format!(
                    "settings: could not read {}: {err}; using the defaults",
                    path.display()
                )],
            ),
        }
    }

    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    // Empty, or only spaces, means the Windows user name.
    pub fn set_name(&mut self, text: &str) {
        let kept: String = text.trim().chars().take(NAME_CHARS).collect();
        let kept = kept.trim_end();
        self.name = (!kept.is_empty()).then(|| kept.to_owned());
    }

    pub fn port(&self) -> u16 {
        self.port.unwrap_or(DEFAULT_PORT)
    }

    // Empty means the default. The error is why not, for the log.
    pub fn set_port(&mut self, text: &str) -> Result<(), &'static str> {
        let text = text.trim();
        if text.is_empty() {
            self.port = None;
            return Ok(());
        }
        // Digits only: u16's own parser also takes a leading plus sign.
        let port = text
            .bytes()
            .all(|byte| byte.is_ascii_digit())
            .then(|| text.parse::<u16>().ok())
            .flatten()
            .filter(|port| *port >= LOWEST_PORT)
            .ok_or("the port must be a number from 1024 to 65535")?;
        self.port = (port != DEFAULT_PORT).then_some(port);
        Ok(())
    }

    pub fn stun_servers(&self) -> Vec<String> {
        self.stun_servers.clone().unwrap_or_else(default_stun)
    }

    // One server per line, as the settings screen has them. Blank lines
    // are nothing. Err is the first line, counted from 1, that is not a
    // server; nothing changes then.
    pub fn set_stun_servers(&mut self, text: &str) -> Result<(), usize> {
        let mut servers = Vec::new();
        for (index, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if !stun_server(line) {
                return Err(index + 1);
            }
            servers.push(line.to_owned());
        }
        self.stun_servers = (servers != default_stun()).then_some(servers);
        Ok(())
    }

    pub fn address_name(&self) -> Option<&str> {
        self.address_name.as_deref()
    }

    // Empty, or only spaces, means no name.
    pub fn set_address_name(&mut self, text: &str) -> Result<(), BuildError> {
        let text = text.trim();
        if text.is_empty() {
            self.address_name = None;
            return Ok(());
        }
        invite::check_hostname(text)?;
        self.address_name = Some(text.to_owned());
        Ok(())
    }

    pub fn device(&self, direction: Direction) -> Choice {
        let id = match direction {
            Direction::Input => &self.input_device,
            Direction::Output => &self.output_device,
        };
        id.as_deref().map_or(Choice::Default, Choice::from_id)
    }

    pub fn set_device(&mut self, direction: Direction, choice: &Choice) {
        let id = choice.id().map(str::to_owned);
        match direction {
            Direction::Input => self.input_device = id,
            Direction::Output => self.output_device = id,
        }
    }

    pub fn talk_mode(&self) -> TalkMode {
        if self.open_mic {
            TalkMode::OpenMic
        } else {
            TalkMode::PushToTalk
        }
    }

    pub fn set_talk_mode(&mut self, mode: TalkMode) {
        self.open_mic = mode == TalkMode::OpenMic;
    }

    // On unless this person turned it off: packets of one size at a steady
    // rate do not show the shape of the speech to anyone on the path.
    pub fn constant_rate(&self) -> bool {
        !self.variable_rate
    }

    pub fn set_constant_rate(&mut self, on: bool) {
        self.variable_rate = !on;
    }

    pub fn hotkeys(&self) -> Bindings {
        self.hotkeys
    }

    // The screen refuses a key that is already used, so this is never handed
    // two actions on one key.
    pub fn set_hotkeys(&mut self, hotkeys: Bindings) {
        self.hotkeys = hotkeys;
    }

    pub fn upload_mbits(&self) -> u32 {
        self.upload_mbits.unwrap_or(DEFAULT_UPLOAD_MBITS)
    }

    // Held to what the setting takes: the slider cannot go past its ends,
    // but a number typed into the file by hand can.
    pub fn set_upload_mbits(&mut self, mbits: u32) {
        let mbits = mbits.clamp(LEAST_UPLOAD_MBITS, MOST_UPLOAD_MBITS);
        self.upload_mbits = (mbits != DEFAULT_UPLOAD_MBITS).then_some(mbits);
    }

    pub fn vsync(&self) -> bool {
        self.vsync
    }

    pub fn set_vsync(&mut self, on: bool) {
        self.vsync = on;
    }

    pub fn hide_strip(&self) -> bool {
        self.hide_strip
    }

    pub fn set_hide_strip(&mut self, on: bool) {
        self.hide_strip = on;
    }

    // What may not change while this PC is controlled, since the controller
    // can drive the settings screen too: the panic key with the other
    // hotkeys, and the sharing settings. Compared by what they mean, so an
    // upload written down at its default is the same as one left out.
    pub fn locked_part_differs(&self, other: &Settings) -> bool {
        self.hotkeys != other.hotkeys
            || self.upload_mbits() != other.upload_mbits()
            || self.vsync != other.vsync
            || self.hide_strip != other.hide_strip
            || self.share_monitor != other.share_monitor
    }

    pub fn share_monitor(&self) -> Option<&str> {
        self.share_monitor.as_deref()
    }

    pub fn set_share_monitor(&mut self, device: Option<&str>) {
        self.share_monitor = device.map(str::to_owned);
    }

    pub fn check_for_new_versions(&self) -> bool {
        self.check_for_new_versions
    }

    pub fn set_check_for_new_versions(&mut self, on: bool) {
        self.check_for_new_versions = on;
    }

    // The file is read again first, so a line added by hand while Booth was
    // open is kept too.
    pub fn save(&self, dir: &Path) -> Result<(), SaveError> {
        let path = dir.join(FILE);
        let old = read(&path).map_err(|source| SaveError::Read {
            path: path.clone(),
            source,
        })?;
        replace(dir, &path, &self.merge(&old))
    }

    // Each key goes in the place of its first line, so the file keeps its
    // order; a key the file lacks goes at the end.
    fn merge(&self, old: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(old.len() + 64);
        let mut written: Vec<&str> = Vec::new();
        for line in lines(old) {
            let known = std::str::from_utf8(line)
                .ok()
                .and_then(|line| match parse_line(line) {
                    Line::Pair { key, .. } => KEYS.into_iter().find(|known| *known == key),
                    _ => None,
                });
            match known {
                None => {
                    out.extend_from_slice(line);
                    out.extend_from_slice(b"\r\n");
                }
                Some(key) if !written.contains(&key) => {
                    self.write_key(key, &mut out);
                    written.push(key);
                }
                Some(_) => {}
            }
        }
        for key in KEYS {
            if !written.contains(&key) {
                self.write_key(key, &mut out);
            }
        }
        out
    }

    fn write_key(&self, key: &str, out: &mut Vec<u8>) {
        let value = match key {
            NAME => self.name.clone(),
            PORT => self.port.map(|port| port.to_string()),
            STUN_SERVERS => self.stun_servers.as_ref().map(|servers| servers.join(", ")),
            ADDRESS_NAME => self.address_name.clone(),
            INPUT_DEVICE => self.input_device.clone(),
            OUTPUT_DEVICE => self.output_device.clone(),
            TALK_MODE => self.open_mic.then(|| String::from(OPEN_MIC)),
            CONSTANT_RATE_VOICE => self.variable_rate.then(|| String::from("off")),
            VIDEO_UPLOAD_MBITS => self.upload_mbits.map(|mbits| mbits.to_string()),
            VSYNC_IN_VIEWER => self.vsync.then(|| String::from("on")),
            HIDE_STRIP_IN_FULLSCREEN => self.hide_strip.then(|| String::from("on")),
            SHARE_MONITOR => self.share_monitor.clone(),
            CHECK_FOR_NEW_VERSIONS => self.check_for_new_versions.then(|| String::from("on")),
            key => hotkey_action(key).and_then(|action| {
                let chord = self.hotkeys.chord(action);
                (chord != action.default_chord()).then(|| chord.to_string())
            }),
        };
        match value {
            Some(value) if value.is_empty() => {
                out.extend_from_slice(format!("{key} =\r\n").as_bytes());
            }
            Some(value) => out.extend_from_slice(format!("{key} = {value}\r\n").as_bytes()),
            None => {}
        }
    }

    // A value as the file has it. Ok holds what was left out of it, for the
    // log; Err is why the whole line was.
    fn set_from_file(&mut self, key: &str, value: &str) -> Result<Vec<String>, String> {
        match key {
            NAME => self.set_name(value),
            PORT => self.set_port(value).map_err(String::from)?,
            STUN_SERVERS => {
                let mut servers = Vec::new();
                let mut left_out = Vec::new();
                for server in value.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                    if stun_server(server) {
                        servers.push(server.to_owned());
                    } else {
                        left_out.push(format!(
                            "{server} left out of {STUN_SERVERS}: it is not name:port"
                        ));
                    }
                }
                self.stun_servers = Some(servers);
                return Ok(left_out);
            }
            ADDRESS_NAME => self
                .set_address_name(value)
                .map_err(|err| err.to_string())?,
            INPUT_DEVICE | OUTPUT_DEVICE => {
                let direction = if key == INPUT_DEVICE {
                    Direction::Input
                } else {
                    Direction::Output
                };
                self.set_device(direction, &device_id(value)?);
            }
            TALK_MODE => {
                self.open_mic = match value {
                    PUSH_TO_TALK => false,
                    OPEN_MIC => true,
                    _ => {
                        return Err(format!(
                            "{TALK_MODE} is {PUSH_TO_TALK} or {OPEN_MIC}; choose how you talk in settings"
                        ));
                    }
                };
            }
            CONSTANT_RATE_VOICE => {
                self.variable_rate = match value {
                    "on" => false,
                    "off" => true,
                    _ => {
                        return Err(format!(
                            "{CONSTANT_RATE_VOICE} is on or off; set it again in settings"
                        ));
                    }
                };
            }
            VIDEO_UPLOAD_MBITS => {
                let mbits = value
                    .bytes()
                    .all(|byte| byte.is_ascii_digit())
                    .then(|| value.parse::<u32>().ok())
                    .flatten()
                    .filter(|mbits| (LEAST_UPLOAD_MBITS..=MOST_UPLOAD_MBITS).contains(mbits))
                    .ok_or_else(|| {
                        format!(
                            "{VIDEO_UPLOAD_MBITS} is a number of Mbit/s from {LEAST_UPLOAD_MBITS} to {MOST_UPLOAD_MBITS}; set it again in settings"
                        )
                    })?;
                self.set_upload_mbits(mbits);
            }
            VSYNC_IN_VIEWER | HIDE_STRIP_IN_FULLSCREEN | CHECK_FOR_NEW_VERSIONS => {
                let on = match value {
                    "on" => true,
                    "off" => false,
                    _ => return Err(format!("{key} is on or off; set it again in settings")),
                };
                match key {
                    VSYNC_IN_VIEWER => self.vsync = on,
                    HIDE_STRIP_IN_FULLSCREEN => self.hide_strip = on,
                    _ => self.check_for_new_versions = on,
                }
            }
            SHARE_MONITOR => {
                self.share_monitor = monitor_name(value)?;
            }
            key => {
                if let Some(action) = hotkey_action(key) {
                    let chord: Chord = value
                        .parse()
                        .map_err(|err| format!("{err}; choose the key again in settings"))?;
                    self.hotkeys.set(action, chord);
                }
            }
        }
        Ok(Vec::new())
    }
}

fn hotkey_action(key: &str) -> Option<Action> {
    Some(match key {
        HOTKEY_PUSH_TO_TALK => Action::PushToTalk,
        HOTKEY_MUTE => Action::Mute,
        HOTKEY_DEAFEN => Action::Deafen,
        HOTKEY_SHARE => Action::Share,
        HOTKEY_PANIC => Action::Panic,
        HOTKEY_SHOW_PANEL => Action::ShowPanel,
        HOTKEY_STATS_PANEL => Action::StatsPanel,
        _ => return None,
    })
}

// Windows' endpoint ids look like {0.0.1.00000000}.{1ebb6084-...}, about 55
// characters. Anything much longer, or with control characters in it, was
// not written by Booth. Empty is Windows default.
fn device_id(value: &str) -> Result<Choice, String> {
    const MOST: usize = 256;
    if value.chars().count() > MOST {
        return Err(format!(
            "a device id is at most {MOST} characters; choose the device again in settings"
        ));
    }
    if value.chars().any(char::is_control) {
        return Err(String::from(
            "a device id has no control characters; choose the device again in settings",
        ));
    }
    Ok(Choice::from_id(value))
}

// Windows names monitors \\.\DISPLAY1 and so on. Anything long or with
// control characters in it was not written by Booth; empty is none picked.
fn monitor_name(value: &str) -> Result<Option<String>, String> {
    const MOST: usize = 64;
    if value.chars().count() > MOST || value.chars().any(char::is_control) {
        return Err(format!(
            "{SHARE_MONITOR} is not a monitor name Windows gives; pick the monitor again after Share"
        ));
    }
    Ok((!value.is_empty()).then(|| value.to_owned()))
}

fn default_stun() -> Vec<String> {
    net::stun::DEFAULT_SERVERS
        .iter()
        .map(|server| server.to_string())
        .collect()
}

// A name or an address, then a port, as stun.cloudflare.com:3478 or
// [2606:4700::]:3478. The name is looked up when a room opens.
fn stun_server(text: &str) -> bool {
    if let Ok(addr) = text.parse::<SocketAddr>() {
        return addr.port() != 0 && !addr.ip().is_unspecified() && !addr.ip().is_multicast();
    }
    let Some((name, port)) = text.rsplit_once(':') else {
        return false;
    };
    let port_ok = !port.is_empty()
        && port.bytes().all(|byte| byte.is_ascii_digit())
        && port.parse::<u16>().is_ok_and(|port| port != 0);
    port_ok && invite::check_hostname(name).is_ok()
}

#[derive(Debug)]
pub enum SaveError {
    // The file is there but could not be read, so writing now would lose
    // whatever else is in it.
    Read { path: PathBuf, source: io::Error },
    Write { path: PathBuf, source: io::Error },
    Replace { path: PathBuf, source: io::Error },
}

impl fmt::Display for SaveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SaveError::Read { path, source } => write!(
                f,
                "could not read {} to keep what else is in it: {source}",
                path.display()
            ),
            SaveError::Write { path, source } => {
                write!(f, "could not write {}: {source}", path.display())
            }
            SaveError::Replace { path, source } => write!(
                f,
                "could not replace {} with the new settings: {source}",
                path.display()
            ),
        }
    }
}

impl std::error::Error for SaveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SaveError::Read { source, .. }
            | SaveError::Write { source, .. }
            | SaveError::Replace { source, .. } => Some(source),
        }
    }
}

// Nothing there reads as an empty file.
fn read(path: &Path) -> io::Result<Vec<u8>> {
    match fs::read(path) {
        Ok(bytes) => Ok(bytes),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(err) => Err(err),
    }
}

// Written in full next to the old file, flushed to disk, then renamed over
// it, so a crash or a full disk leaves the old file or the new one and
// never half of either. Windows replaces the target in the rename.
fn replace(dir: &Path, path: &Path, bytes: &[u8]) -> Result<(), SaveError> {
    let temp = temp_path(dir);
    let wrote = File::create(&temp).and_then(|mut file| {
        file.write_all(bytes)?;
        file.sync_all()
    });
    if let Err(source) = wrote {
        let _ = fs::remove_file(&temp);
        return Err(SaveError::Write { path: temp, source });
    }
    if let Err(source) = fs::rename(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(SaveError::Replace {
            path: path.to_owned(),
            source,
        });
    }
    Ok(())
}

// Two Booths on the same profile saving at once each get their own.
fn temp_path(dir: &Path) -> PathBuf {
    dir.join(format!("settings.{}.tmp", std::process::id()))
}

// Lines without their line break, "\n" or "\r\n", and without a leading
// byte order mark, which Notepad has written in the past.
fn lines(bytes: &[u8]) -> Vec<&[u8]> {
    let bytes = bytes.strip_prefix(BOM).unwrap_or(bytes);
    if bytes.is_empty() {
        return Vec::new();
    }
    let bytes = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    bytes
        .split(|&byte| byte == b'\n')
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
        .collect()
}

enum Line<'a> {
    Blank,
    Pair { key: &'a str, value: &'a str },
    Broken(&'static str),
}

fn parse_line(line: &str) -> Line<'_> {
    if line.trim().is_empty() {
        return Line::Blank;
    }
    let Some((key, value)) = line.split_once('=') else {
        return Line::Broken("there is no = in it");
    };
    let key = key.trim();
    if key.is_empty() {
        return Line::Broken("there is no key before the =");
    }
    let plain = |byte: u8| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_';
    if !key.bytes().all(plain) {
        return Line::Broken("a key may hold only a to z, 0 to 9 and _");
    }
    Line::Pair {
        key,
        value: value.trim(),
    }
}

fn parse(bytes: &[u8]) -> (Settings, Vec<String>) {
    let mut settings = Settings::default();
    let mut problems = Vec::new();
    // The line each key was last taken from.
    let mut taken: Vec<(&str, usize)> = Vec::new();
    for (index, line) in lines(bytes).into_iter().enumerate() {
        let number = index + 1;
        let Ok(line) = std::str::from_utf8(line) else {
            problems.push(format!(
                "settings: line {number} skipped: it is not UTF-8 text"
            ));
            continue;
        };
        let (key, value) = match parse_line(line) {
            Line::Blank => continue,
            Line::Broken(why) => {
                problems.push(format!("settings: line {number} skipped: {why}"));
                continue;
            }
            Line::Pair { key, value } => (key, value),
        };
        let Some(key) = KEYS.into_iter().find(|known| *known == key) else {
            problems.push(format!(
                "settings: line {number}: {key} is not a setting this version knows; kept as it is"
            ));
            continue;
        };
        let mut next = settings.clone();
        match next.set_from_file(key, value) {
            Ok(left_out) => {
                if let Some(at) = taken.iter_mut().find(|(k, _)| *k == key) {
                    problems.push(format!(
                        "settings: {key} is on line {} and line {number}; line {number} counts",
                        at.1
                    ));
                    at.1 = number;
                } else {
                    taken.push((key, number));
                }
                for what in left_out {
                    problems.push(format!("settings: line {number}: {what}"));
                }
                settings = next;
            }
            Err(why) => problems.push(format!("settings: line {number} skipped: {why}")),
        }
    }
    // Booth never saves two actions on one key. A clash comes from editing by
    // hand, or from a newer version giving a new action, as its default, a
    // key chosen earlier for another. Only an action moved off its default
    // gives way, so the rest of what was chosen stays; of two moved ones,
    // which was meant is anyone's guess, so both do.
    while let Some((one, other)) = settings.hotkeys.first_conflict() {
        let chord = settings.hotkeys.chord(one);
        let moved: Vec<Action> = [one, other]
            .into_iter()
            .filter(|action| settings.hotkeys.chord(*action) != action.default_chord())
            .collect();
        // Two defaults on one key would be this version's mistake, and
        // input's tests rule it out; the loop must end even so.
        if moved.is_empty() {
            problems.push(format!(
                "settings: {} and {} are on one key, {chord}; using the default hotkeys",
                one.name(),
                other.name()
            ));
            settings.hotkeys = Bindings::default();
            break;
        }
        let mut back = Vec::new();
        for action in moved {
            settings.hotkeys.set(action, action.default_chord());
            back.push(format!(
                "{} goes back to {}",
                action.name(),
                action.default_chord()
            ));
        }
        problems.push(format!(
            "settings: {} and {} are on one key, {chord}; {}",
            one.name(),
            other.name(),
            back.join(" and ")
        ));
    }
    (settings, problems)
}

#[cfg(test)]
mod tests {
    use super::*;

    // A fresh folder per test under the system temp folder, gone at the end.
    struct Folder(PathBuf);

    impl Folder {
        fn new(name: &str) -> Folder {
            let path =
                std::env::temp_dir().join(format!("booth-settings-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Folder(path)
        }

        fn file(&self) -> PathBuf {
            self.0.join(FILE)
        }
    }

    impl Drop for Folder {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn named(name: &str) -> Settings {
        let mut settings = Settings::default();
        settings.set_address_name(name).unwrap();
        settings
    }

    #[test]
    fn a_missing_file_is_the_defaults_and_no_complaint() {
        let folder = Folder::new("missing");
        let (settings, problems) = Settings::load(&folder.0);
        assert_eq!(settings, Settings::default());
        assert_eq!(settings.address_name(), None);
        assert!(problems.is_empty(), "{problems:?}");
    }

    #[test]
    fn round_trip() {
        let folder = Folder::new("round-trip");
        named("myroom.example.net").save(&folder.0).unwrap();
        assert_eq!(
            fs::read_to_string(folder.file()).unwrap(),
            "address_name = myroom.example.net\r\n"
        );
        let (settings, problems) = Settings::load(&folder.0);
        assert_eq!(settings.address_name(), Some("myroom.example.net"));
        assert!(problems.is_empty(), "{problems:?}");

        Settings::default().save(&folder.0).unwrap();
        assert_eq!(fs::read_to_string(folder.file()).unwrap(), "");
        assert_eq!(Settings::load(&folder.0).0, Settings::default());
    }

    #[test]
    fn unknown_lines_kept() {
        let folder = Folder::new("unknown");
        let before = "video_upload=15\r\n\r\naddress_name = old.example.net\r\n  viewer_vsync =  on\r\nnot a setting\r\n";
        fs::write(folder.file(), before).unwrap();
        named("new.example.net").save(&folder.0).unwrap();
        assert_eq!(
            fs::read_to_string(folder.file()).unwrap(),
            "video_upload=15\r\n\r\naddress_name = new.example.net\r\n  viewer_vsync =  on\r\nnot a setting\r\n"
        );
    }

    #[test]
    fn non_utf8_line_survives() {
        let folder = Folder::new("not-utf8");
        fs::write(
            folder.file(),
            b"name = Andr\xE9\r\naddress_name = a.example.net\r\n",
        )
        .unwrap();
        let (settings, problems) = Settings::load(&folder.0);
        assert_eq!(settings.address_name(), Some("a.example.net"));
        assert_eq!(problems, ["settings: line 1 skipped: it is not UTF-8 text"]);
        named("b.example.net").save(&folder.0).unwrap();
        assert_eq!(
            fs::read(folder.file()).unwrap(),
            b"name = Andr\xE9\r\naddress_name = b.example.net\r\n"
        );
    }

    #[test]
    fn broken_lines_are_skipped_and_said() {
        let text = "\u{FEFF}garbage\n = nothing\nAddress_Name = a.example.net\naddress_name = localhost\naddress_name = 192.168.1.20\nfuture_key = 3\naddress_name = good.example.net\n";
        let (settings, problems) = parse(text.as_bytes());
        assert_eq!(settings.address_name(), Some("good.example.net"));
        assert_eq!(
            problems,
            [
                "settings: line 1 skipped: there is no = in it",
                "settings: line 2 skipped: there is no key before the =",
                "settings: line 3 skipped: a key may hold only a to z, 0 to 9 and _",
                "settings: line 4 skipped: the host name is localhost, which always means this same PC",
                "settings: line 5 skipped: the host name ends in a number, so it would be read as an IP address",
                "settings: line 6: future_key is not a setting this version knows; kept as it is",
            ]
        );
    }

    #[test]
    fn values_as_typed_by_hand() {
        let (settings, problems) = parse(b"  address_name=Home.Example.NET  \r\n");
        assert_eq!(settings.address_name(), Some("Home.Example.NET"));
        assert!(problems.is_empty(), "{problems:?}");
        // Left empty on purpose: no name, and nothing wrong.
        let (settings, problems) = parse(b"address_name =\r\n");
        assert_eq!(settings.address_name(), None);
        assert!(problems.is_empty(), "{problems:?}");
    }

    #[test]
    fn last_good_line_counts() {
        let (settings, problems) =
            parse(b"address_name = a.example.net\naddress_name = b.example.net\n");
        assert_eq!(settings.address_name(), Some("b.example.net"));
        assert_eq!(
            problems,
            ["settings: address_name is on line 1 and line 2; line 2 counts"]
        );
        let folder = Folder::new("twice");
        fs::write(
            folder.file(),
            "address_name = a.example.net\r\nvideo_upload = 15\r\naddress_name = b.example.net\r\n",
        )
        .unwrap();
        named("c.example.net").save(&folder.0).unwrap();
        assert_eq!(
            fs::read_to_string(folder.file()).unwrap(),
            "address_name = c.example.net\r\nvideo_upload = 15\r\n"
        );
    }

    #[test]
    fn failed_save_keeps_old_file() {
        let folder = Folder::new("atomic");
        let before = "address_name = old.example.net\r\nvideo_upload = 15\r\n";
        fs::write(folder.file(), before).unwrap();
        // A folder where the new file would go stands in for a full disk.
        fs::create_dir(temp_path(&folder.0)).unwrap();
        let err = named("new.example.net").save(&folder.0).unwrap_err();
        assert!(matches!(err, SaveError::Write { .. }), "{err}");
        assert_eq!(fs::read_to_string(folder.file()).unwrap(), before);

        fs::remove_dir(temp_path(&folder.0)).unwrap();
        named("new.example.net").save(&folder.0).unwrap();
        let left: Vec<_> = fs::read_dir(&folder.0)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(
            left,
            [FILE],
            "the new file is written beside it and renamed"
        );
    }

    #[test]
    fn unreadable_file_kept() {
        let folder = Folder::new("unreadable");
        // Windows will not open a folder as a file, which stands in for a
        // file another program holds open without sharing.
        fs::create_dir(folder.file()).unwrap();
        let (settings, problems) = Settings::load(&folder.0);
        assert_eq!(settings, Settings::default());
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].ends_with("; using the defaults"),
            "{problems:?}"
        );
        let err = named("a.example.net").save(&folder.0).unwrap_err();
        assert!(matches!(err, SaveError::Read { .. }), "{err}");
    }

    #[test]
    fn only_names_the_invite_can_carry_are_taken() {
        let mut settings = named("myroom.example.net");
        for bad in [
            "localhost",
            "192.168.1.20",
            "127.1",
            "my room.example.net",
            "https://myroom.example.net",
            "myroom.example.net.",
        ] {
            assert!(settings.set_address_name(bad).is_err(), "{bad}");
            assert_eq!(settings.address_name(), Some("myroom.example.net"));
        }
        settings.set_address_name("  ").unwrap();
        assert_eq!(settings.address_name(), None);
    }

    #[test]
    fn name_port_and_stun_servers_round_trip() {
        let folder = Folder::new("all-keys");
        let mut settings = named("myroom.example.net");
        settings.set_name("  Mara ");
        settings.set_port("41010").unwrap();
        settings
            .set_stun_servers("stun.example.org:3478\n\n[2001:db8::3]:3478\n")
            .unwrap();
        settings.save(&folder.0).unwrap();
        assert_eq!(
            fs::read_to_string(folder.file()).unwrap(),
            "name = Mara\r\nport = 41010\r\nstun_servers = stun.example.org:3478, [2001:db8::3]:3478\r\naddress_name = myroom.example.net\r\n"
        );
        let (loaded, problems) = Settings::load(&folder.0);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(loaded, settings);
        assert_eq!(loaded.name(), Some("Mara"));
        assert_eq!(loaded.port(), 41010);
        assert_eq!(
            loaded.stun_servers(),
            ["stun.example.org:3478", "[2001:db8::3]:3478"]
        );
    }

    #[test]
    fn defaults_are_not_written_down() {
        let mut settings = Settings::default();
        settings.set_name("   ");
        settings.set_port("41000").unwrap();
        settings
            .set_stun_servers("stun.cloudflare.com:3478\nstun.l.google.com:19302")
            .unwrap();
        assert_eq!(settings, Settings::default());
        assert_eq!(settings.name(), None);
        assert_eq!(settings.port(), DEFAULT_PORT);
        assert_eq!(settings.stun_servers(), net::stun::DEFAULT_SERVERS);
        assert!(settings.merge(b"").is_empty());
        settings.set_port("").unwrap();
        assert_eq!(settings.port(), DEFAULT_PORT);
    }

    // No STUN at all is a choice, and not the same as the defaults.
    #[test]
    fn an_empty_stun_list_is_kept_as_empty() {
        let mut settings = Settings::default();
        settings.set_stun_servers("\n  \n").unwrap();
        assert!(settings.stun_servers().is_empty());
        let written = String::from_utf8(settings.merge(b"")).unwrap();
        assert_eq!(written, "stun_servers =\r\n");
        let (loaded, problems) = parse(written.as_bytes());
        assert!(problems.is_empty(), "{problems:?}");
        assert!(loaded.stun_servers().is_empty());
    }

    #[test]
    fn a_port_is_a_number_from_1024_up() {
        let mut settings = Settings::default();
        for bad in ["0", "80", "1023", "65536", "+41010", "41 010", "port", "-1"] {
            assert!(settings.set_port(bad).is_err(), "{bad}");
        }
        assert_eq!(settings.port(), DEFAULT_PORT);
        for good in ["1024", "65535", " 41500 "] {
            settings.set_port(good).unwrap();
            assert_eq!(settings.port().to_string(), good.trim());
        }
    }

    #[test]
    fn stun_servers_need_a_port() {
        let mut settings = Settings::default();
        for good in [
            "stun.cloudflare.com:3478",
            "STUN.example.org:19302",
            "203.0.113.5:3478",
            "[2001:db8::3]:3478",
        ] {
            settings.set_stun_servers(good).unwrap();
            assert_eq!(settings.stun_servers(), [good]);
        }
        let before = settings.clone();
        for (text, line) in [
            ("stun.cloudflare.com", 1),
            ("a.example.org:3478\n\nstun.example.org:0", 3),
            ("a.example.org:3478\nstun.example.org:99999", 2),
            ("stun.example.org:34 78", 1),
            ("localhost:3478", 1),
            ("0.0.0.0:3478", 1),
            ("a.example.org:3478, b.example.org:3478", 1),
            ("https://stun.example.org:3478", 1),
        ] {
            assert_eq!(settings.set_stun_servers(text), Err(line), "{text:?}");
            assert_eq!(settings, before, "{text:?} changed the list");
        }
    }

    #[test]
    fn the_file_s_own_mistakes_are_said_and_skipped() {
        let text = "name = \u{e9}mile\nport = 99\nport = 41010\nstun_servers = a.example.org:3478, nothing, b.example.org:3478\n";
        let (settings, problems) = parse(text.as_bytes());
        assert_eq!(settings.name(), Some("\u{e9}mile"));
        assert_eq!(settings.port(), 41010);
        assert_eq!(
            settings.stun_servers(),
            ["a.example.org:3478", "b.example.org:3478"]
        );
        assert_eq!(
            problems,
            [
                "settings: line 2 skipped: the port must be a number from 1024 to 65535",
                "settings: line 4: nothing left out of stun_servers: it is not name:port",
            ]
        );
    }

    #[test]
    fn devices_by_id() {
        let folder = Folder::new("devices");
        let mic = "{0.0.1.00000000}.{1ebb6084-821b-407b-a62b-101f24e2841c}";
        fs::write(
            folder.file(),
            "output_device = old\r\nvideo_upload = 15\r\n",
        )
        .unwrap();
        let mut settings = Settings::default();
        settings.set_device(Direction::Input, &Choice::Device(mic.to_owned()));
        settings.save(&folder.0).unwrap();
        // The output line goes, since its device is Windows default now.
        assert_eq!(
            fs::read_to_string(folder.file()).unwrap(),
            format!("video_upload = 15\r\ninput_device = {mic}\r\n")
        );
        let (loaded, problems) = Settings::load(&folder.0);
        assert_eq!(problems.len(), 1, "only video_upload: {problems:?}");
        assert_eq!(
            loaded.device(Direction::Input),
            Choice::Device(mic.to_owned())
        );
        assert_eq!(loaded.device(Direction::Output), Choice::Default);

        settings.set_device(Direction::Input, &Choice::Default);
        assert_eq!(settings, Settings::default());
    }

    #[test]
    fn a_device_id_booth_did_not_write_is_skipped() {
        let long = "x".repeat(300);
        let text = format!("input_device = {long}\noutput_device = a\u{7}b\noutput_device =\n");
        let (settings, problems) = parse(text.as_bytes());
        assert_eq!(settings, Settings::default());
        assert_eq!(
            problems,
            [
                "settings: line 1 skipped: a device id is at most 256 characters; choose the device again in settings",
                "settings: line 2 skipped: a device id has no control characters; choose the device again in settings",
            ]
        );
    }

    #[test]
    fn long_name_cut() {
        let mut settings = Settings::default();
        settings.set_name(&format!("{} tail", "a".repeat(31)));
        assert_eq!(settings.name(), Some("a".repeat(31).as_str()));
    }

    #[test]
    fn voice_round_trip() {
        let folder = Folder::new("voice");
        let mut settings = Settings::default();
        assert_eq!(settings.talk_mode(), TalkMode::PushToTalk);
        assert!(settings.constant_rate());
        settings.set_talk_mode(TalkMode::OpenMic);
        settings.set_constant_rate(false);
        settings.save(&folder.0).unwrap();
        assert_eq!(
            fs::read_to_string(folder.file()).unwrap(),
            "talk_mode = open_mic\r\nconstant_rate_voice = off\r\n"
        );
        let (loaded, problems) = Settings::load(&folder.0);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(loaded, settings);

        settings.set_talk_mode(TalkMode::PushToTalk);
        settings.set_constant_rate(true);
        assert_eq!(settings, Settings::default());
        // Written by hand, the defaults read back as the defaults.
        let (loaded, problems) = parse(b"talk_mode = push_to_talk\nconstant_rate_voice = on\n");
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(loaded, Settings::default());
    }

    #[test]
    fn unknown_voice_values() {
        let (settings, problems) = parse(b"talk_mode = shout\nconstant_rate_voice = maybe\n");
        assert_eq!(settings, Settings::default());
        assert_eq!(
            problems,
            [
                "settings: line 1 skipped: talk_mode is push_to_talk or open_mic; choose how you talk in settings",
                "settings: line 2 skipped: constant_rate_voice is on or off; set it again in settings",
            ]
        );
    }

    #[test]
    fn hotkeys_round_trip() {
        let folder = Folder::new("hotkeys");
        let mut hotkeys = Bindings::default();
        hotkeys.set(Action::Mute, "F9".parse().unwrap());
        hotkeys.set(Action::PushToTalk, "Shift+Right Ctrl".parse().unwrap());
        let mut settings = Settings::default();
        settings.set_hotkeys(hotkeys);
        settings.save(&folder.0).unwrap();
        assert_eq!(
            fs::read_to_string(folder.file()).unwrap(),
            "hotkey_push_to_talk = Shift+Right Ctrl\r\nhotkey_mute = F9\r\n"
        );
        let (loaded, problems) = Settings::load(&folder.0);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(loaded.hotkeys(), hotkeys);

        settings.set_hotkeys(Bindings::default());
        assert_eq!(settings, Settings::default());
        settings.save(&folder.0).unwrap();
        assert_eq!(fs::read_to_string(folder.file()).unwrap(), "");
    }

    // The panic key is written down and read back like any other, and only
    // when it is off its default.
    #[test]
    fn the_panic_key_is_kept_like_the_other_hotkeys() {
        let folder = Folder::new("panic-key");
        let mut hotkeys = Bindings::default();
        hotkeys.set(Action::Panic, "Ctrl+Alt+End".parse().unwrap());
        let mut settings = Settings::default();
        settings.set_hotkeys(hotkeys);
        settings.save(&folder.0).unwrap();
        assert_eq!(
            fs::read_to_string(folder.file()).unwrap(),
            "hotkey_panic = Ctrl+Alt+End\r\n"
        );
        let (loaded, problems) = Settings::load(&folder.0);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(
            loaded.hotkeys().chord(Action::Panic).to_string(),
            "Ctrl+Alt+End"
        );
    }

    #[test]
    fn what_the_lock_covers() {
        let saved = Settings::default();
        let changed = |change: &dyn Fn(&mut Settings)| {
            let mut next = saved.clone();
            change(&mut next);
            next.locked_part_differs(&saved)
        };
        assert!(!changed(&|_| {}));
        assert!(changed(&|next| {
            let mut keys = Bindings::default();
            keys.set(Action::Panic, "F9".parse().unwrap());
            next.set_hotkeys(keys);
        }));
        assert!(changed(&|next| {
            let mut keys = Bindings::default();
            keys.set(Action::Mute, "F9".parse().unwrap());
            next.set_hotkeys(keys);
        }));
        assert!(changed(&|next| next.set_upload_mbits(40)));
        assert!(changed(&|next| next.set_vsync(true)));
        assert!(changed(&|next| next.set_hide_strip(true)));
        assert!(changed(
            &|next| next.set_share_monitor(Some(r"\\.\DISPLAY2"))
        ));
        // The upload at its default, written down or not, is no change.
        assert!(!changed(&|next| next.set_upload_mbits(DEFAULT_UPLOAD_MBITS)));
        assert!(!changed(&|next| next.set_name("Tom")));
        assert!(!changed(&|next| next.set_port("41010").unwrap()));
        assert!(!changed(&|next| next.set_constant_rate(false)));
    }

    #[test]
    fn hotkey_typed_by_hand() {
        let (settings, problems) =
            parse(b"hotkey_deafen = ctrl + alt + d\nhotkey_stats_panel = Ctrl+Pgup\n");
        assert_eq!(
            settings.hotkeys().chord(Action::Deafen).to_string(),
            "Ctrl+Alt+D"
        );
        assert_eq!(
            settings.hotkeys().chord(Action::StatsPanel),
            Action::StatsPanel.default_chord()
        );
        assert_eq!(
            problems,
            [
                "settings: line 2 skipped: Pgup is not a key Booth knows; choose the key again in settings"
            ]
        );
    }

    #[test]
    fn key_on_another_default_goes_back() {
        let (settings, problems) = parse(b"hotkey_mute = F9\nhotkey_deafen = Ctrl+Shift+M\n");
        assert_eq!(settings.hotkeys().chord(Action::Mute).to_string(), "F9");
        assert!(problems.is_empty(), "{problems:?}");

        let (settings, problems) = parse(b"hotkey_deafen = Ctrl+Shift+M\n");
        assert_eq!(settings.hotkeys(), Bindings::default());
        assert_eq!(
            problems,
            [
                "settings: mute and deafen are on one key, Ctrl+Shift+M; deafen goes back to Ctrl+Shift+D"
            ]
        );
    }

    // Saved by an older settings screen, before share had Ctrl+Shift+S. Only
    // mute gives way: push to talk and the stats key stay, and a save
    // afterwards keeps their lines.
    #[test]
    fn new_default_moves_one_action_back() {
        let folder = Folder::new("new-default");
        fs::write(
            folder.file(),
            "hotkey_push_to_talk = Shift+Right Ctrl\r\nhotkey_mute = Ctrl+Shift+S\r\nhotkey_stats_panel = F10\r\n",
        )
        .unwrap();
        let (settings, problems) = Settings::load(&folder.0);
        assert_eq!(
            problems,
            [
                "settings: mute and share (press twice) or stop sharing are on one key, Ctrl+Shift+S; mute goes back to Ctrl+Shift+M"
            ]
        );
        let hotkeys = settings.hotkeys();
        assert_eq!(hotkeys.chord(Action::Mute), Action::Mute.default_chord());
        assert_eq!(hotkeys.chord(Action::Share), Action::Share.default_chord());
        assert_eq!(
            hotkeys.chord(Action::PushToTalk).to_string(),
            "Shift+Right Ctrl"
        );
        assert_eq!(hotkeys.chord(Action::StatsPanel).to_string(), "F10");
        assert_eq!(hotkeys.first_conflict(), None);
        settings.save(&folder.0).unwrap();
        assert_eq!(
            fs::read_to_string(folder.file()).unwrap(),
            "hotkey_push_to_talk = Shift+Right Ctrl\r\nhotkey_stats_panel = F10\r\n"
        );
    }

    // Mute's own default is where deafen was moved to, so deafen gives way
    // next.
    #[test]
    fn moves_back_in_a_chain() {
        let (settings, problems) = parse(
            b"hotkey_mute = Ctrl+Shift+S\nhotkey_deafen = Ctrl+Shift+M\nhotkey_push_to_talk = Shift+Right Ctrl\n",
        );
        let mut expected = Bindings::default();
        expected.set(Action::PushToTalk, "Shift+Right Ctrl".parse().unwrap());
        assert_eq!(settings.hotkeys(), expected);
        assert_eq!(
            problems,
            [
                "settings: mute and share (press twice) or stop sharing are on one key, Ctrl+Shift+S; mute goes back to Ctrl+Shift+M",
                "settings: mute and deafen are on one key, Ctrl+Shift+M; deafen goes back to Ctrl+Shift+D",
            ]
        );
    }

    // Only editing by hand puts two moved actions on one key.
    #[test]
    fn two_moved_on_one_key() {
        let (settings, problems) = parse(
            b"hotkey_mute = F9\nhotkey_deafen = F9\nhotkey_push_to_talk = Shift+Right Ctrl\n",
        );
        let mut expected = Bindings::default();
        expected.set(Action::PushToTalk, "Shift+Right Ctrl".parse().unwrap());
        assert_eq!(settings.hotkeys(), expected);
        assert_eq!(
            problems,
            [
                "settings: mute and deafen are on one key, F9; mute goes back to Ctrl+Shift+M and deafen goes back to Ctrl+Shift+D"
            ]
        );
    }

    #[test]
    fn sharing_round_trip() {
        let folder = Folder::new("sharing");
        let mut settings = Settings::default();
        assert_eq!(settings.upload_mbits(), 15);
        assert!(!settings.vsync() && !settings.hide_strip());
        assert_eq!(settings.share_monitor(), None);
        settings.set_upload_mbits(40);
        settings.set_vsync(true);
        settings.set_hide_strip(true);
        settings.set_share_monitor(Some(r"\\.\DISPLAY2"));
        settings.save(&folder.0).unwrap();
        assert_eq!(
            fs::read_to_string(folder.file()).unwrap(),
            "video_upload_mbits = 40\r\nvsync_in_viewer = on\r\nhide_strip_in_fullscreen = on\r\nshare_monitor = \\\\.\\DISPLAY2\r\n"
        );
        let (loaded, problems) = Settings::load(&folder.0);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(loaded, settings);

        settings.set_upload_mbits(15);
        settings.set_vsync(false);
        settings.set_hide_strip(false);
        settings.set_share_monitor(None);
        assert_eq!(settings, Settings::default());
        settings.save(&folder.0).unwrap();
        assert_eq!(fs::read_to_string(folder.file()).unwrap(), "");
        // Written by hand, the defaults read back as the defaults.
        let (loaded, problems) =
            parse(b"video_upload_mbits = 15\nvsync_in_viewer = off\nhide_strip_in_fullscreen = off\nshare_monitor =\n");
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(loaded, Settings::default());
    }

    #[test]
    fn the_upload_stays_between_the_slider_s_ends() {
        let mut settings = Settings::default();
        settings.set_upload_mbits(500);
        assert_eq!(settings.upload_mbits(), 80);
        settings.set_upload_mbits(0);
        assert_eq!(settings.upload_mbits(), 1);
    }

    // video_upload and viewer_vsync stay what the tests above use them for:
    // keys this version does not know, kept as they are.
    #[test]
    fn newer_sharing_names_kept() {
        let folder = Folder::new("sharing-names");
        fs::write(
            folder.file(),
            "video_upload = 15\r\nvideo_upload_mbits = 30\r\nviewer_vsync = on\r\n",
        )
        .unwrap();
        let (mut settings, problems) = Settings::load(&folder.0);
        assert_eq!(settings.upload_mbits(), 30);
        assert!(!settings.vsync());
        assert_eq!(problems.len(), 2, "{problems:?}");
        settings.set_upload_mbits(50);
        settings.save(&folder.0).unwrap();
        assert_eq!(
            fs::read_to_string(folder.file()).unwrap(),
            "video_upload = 15\r\nvideo_upload_mbits = 50\r\nviewer_vsync = on\r\n"
        );
    }

    #[test]
    fn update_check_off_by_default() {
        let folder = Folder::new("new-versions");
        let mut settings = Settings::default();
        assert!(!settings.check_for_new_versions());
        settings.set_check_for_new_versions(true);
        settings.save(&folder.0).unwrap();
        assert_eq!(
            fs::read_to_string(folder.file()).unwrap(),
            "check_for_new_versions = on\r\n"
        );
        let (loaded, problems) = Settings::load(&folder.0);
        assert!(problems.is_empty(), "{problems:?}");
        assert!(loaded.check_for_new_versions());

        settings.set_check_for_new_versions(false);
        assert_eq!(settings, Settings::default());
        settings.save(&folder.0).unwrap();
        assert_eq!(fs::read_to_string(folder.file()).unwrap(), "");
        let (loaded, problems) = parse(b"check_for_new_versions = off\n");
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(loaded, Settings::default());
        let (loaded, problems) = parse(b"check_for_new_versions = yes\n");
        assert!(!loaded.check_for_new_versions());
        assert_eq!(
            problems,
            [
                "settings: line 1 skipped: check_for_new_versions is on or off; set it again in settings"
            ]
        );
    }

    #[test]
    fn unknown_sharing_values() {
        let text = format!(
            "video_upload_mbits = 81\nvideo_upload_mbits = fast\nvsync_in_viewer = yes\nhide_strip_in_fullscreen = 1\nshare_monitor = a\u{7}b\nshare_monitor = {}\n",
            "x".repeat(65)
        );
        let (settings, problems) = parse(text.as_bytes());
        assert_eq!(settings, Settings::default());
        assert_eq!(
            problems,
            [
                "settings: line 1 skipped: video_upload_mbits is a number of Mbit/s from 1 to 80; set it again in settings",
                "settings: line 2 skipped: video_upload_mbits is a number of Mbit/s from 1 to 80; set it again in settings",
                "settings: line 3 skipped: vsync_in_viewer is on or off; set it again in settings",
                "settings: line 4 skipped: hide_strip_in_fullscreen is on or off; set it again in settings",
                "settings: line 5 skipped: share_monitor is not a monitor name Windows gives; pick the monitor again after Share",
                "settings: line 6 skipped: share_monitor is not a monitor name Windows gives; pick the monitor again after Share",
            ]
        );
    }
}
