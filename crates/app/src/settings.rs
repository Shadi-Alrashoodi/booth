// settings.txt in the data folder: one `key = value` per line, so it can be
// read and fixed in Notepad. Save rewrites only the lines of the keys this
// version knows and keeps every other line byte for byte, so a key from a
// newer version, or one typed by hand, survives a save from this one.

use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use input::{Action, Bindings, Chord};
use invite::BuildError;
use room::{DEFAULT_SENSITIVITY_DB, SENSITIVITY_DB, TalkMode};
use voice::audio::{Choice, Direction};

pub const FILE: &str = "settings.txt";
const NAME: &str = "name";
const PORT: &str = "port";
const STUN_SERVERS: &str = "stun_servers";
const ADDRESS_NAME: &str = "address_name";
const INPUT_DEVICE: &str = "input_device";
const OUTPUT_DEVICE: &str = "output_device";
const TALK_MODE: &str = "talk_mode";
const CONSTANT_RATE_VOICE: &str = "constant_rate_voice";
// In dB, written only when moved off room::DEFAULT_SENSITIVITY_DB.
const OPEN_MIC_SENSITIVITY: &str = "open_mic_sensitivity";
// On unless turned off; only off is saved.
const ECHO_CANCELLATION: &str = "echo_cancellation";
// Off unless turned on, since it adds delay; only on is saved.
const NOISE_SUPPRESSION: &str = "noise_suppression";
// On unless turned off; only off is saved.
const AUTOMATIC_VOLUME: &str = "automatic_volume";
// On unless turned off; only off is saved. The volume is in percent,
// written only when moved off room::DEFAULT_CUE_VOLUME.
const SOUND_CUES: &str = "sound_cues";
const SOUND_CUE_VOLUME: &str = "sound_cue_volume";
const SOUND_CUE_JOINS: &str = "sound_cue_joins";
const SOUND_CUE_OWN_MUTE: &str = "sound_cue_own_mute";
const SOUND_CUE_FRIEND_CONNECTION: &str = "sound_cue_friend_connection";
const SOUND_CUE_OWN_CONNECTION: &str = "sound_cue_own_connection";
const SOUND_CUE_SHARES: &str = "sound_cue_shares";
const HOTKEY_PUSH_TO_TALK: &str = "hotkey_push_to_talk";
const HOTKEY_MUTE: &str = "hotkey_mute";
const HOTKEY_DEAFEN: &str = "hotkey_deafen";
const HOTKEY_SHARE: &str = "hotkey_share";
const HOTKEY_PAUSE_SHARE: &str = "hotkey_pause_share";
const HOTKEY_PANIC: &str = "hotkey_panic";
const HOTKEY_SHOW_PANEL: &str = "hotkey_show_panel";
const HOTKEY_STATS_PANEL: &str = "hotkey_stats_panel";
// Not video_upload or viewer_vsync, which the tests below use as keys a
// newer version could write.
const VIDEO_UPLOAD_MBITS: &str = "video_upload_mbits";
const VSYNC_IN_VIEWER: &str = "vsync_in_viewer";
const HIDE_STRIP_IN_FULLSCREEN: &str = "hide_strip_in_fullscreen";
const SHARE_MONITOR: &str = "share_monitor";
// Off unless a window or a region was picked last; only on is saved.
const SHARE_KEY_OPENS_LIST: &str = "share_key_opens_list";
// Off unless the user turns it on, since a check shows GitHub this PC's
// address. Only on is saved.
const CHECK_FOR_NEW_VERSIONS: &str = "check_for_new_versions";
const INVITE_LINKS: &str = "invite_links";
const INVITE_LINK_WAIT_SECS: &str = "invite_link_wait_secs";
// Every key this version knows, in the order a save adds the missing ones.
const KEYS: [&str; 35] = [
    NAME,
    PORT,
    STUN_SERVERS,
    ADDRESS_NAME,
    HOTKEY_PUSH_TO_TALK,
    HOTKEY_MUTE,
    HOTKEY_DEAFEN,
    HOTKEY_SHARE,
    HOTKEY_PAUSE_SHARE,
    HOTKEY_PANIC,
    HOTKEY_SHOW_PANEL,
    HOTKEY_STATS_PANEL,
    INPUT_DEVICE,
    OUTPUT_DEVICE,
    TALK_MODE,
    CONSTANT_RATE_VOICE,
    OPEN_MIC_SENSITIVITY,
    ECHO_CANCELLATION,
    NOISE_SUPPRESSION,
    AUTOMATIC_VOLUME,
    SOUND_CUES,
    SOUND_CUE_VOLUME,
    SOUND_CUE_JOINS,
    SOUND_CUE_OWN_MUTE,
    SOUND_CUE_FRIEND_CONNECTION,
    SOUND_CUE_OWN_CONNECTION,
    SOUND_CUE_SHARES,
    VIDEO_UPLOAD_MBITS,
    VSYNC_IN_VIEWER,
    HIDE_STRIP_IN_FULLSCREEN,
    SHARE_MONITOR,
    SHARE_KEY_OPENS_LIST,
    CHECK_FOR_NEW_VERSIONS,
    INVITE_LINKS,
    INVITE_LINK_WAIT_SECS,
];
const PUSH_TO_TALK: &str = "push_to_talk";
const OPEN_MIC: &str = "open_mic";
const BOM: &[u8] = b"\xEF\xBB\xBF";
// What Booth writes is a few KB at most; a file much bigger is not its own, and
// is not read whole. Each line of it costs more in memory than its bytes, and
// a broken one a log line that stays for the run.
const MOST_BYTES: u64 = 64 * 1024;

pub const DEFAULT_PORT: u16 = 41000;
// Ports below 1024 are the IANA well-known range. Windows lets any process
// bind them, but system services and some firewalls treat them as reserved.
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
    // None means DEFAULT_PORT, which is never saved.
    port: Option<u16>,
    // None means net's defaults. Some of an empty list is no STUN at all.
    stun_servers: Option<Vec<String>>,
    // Always None or a name invite::check_hostname accepts.
    address_name: Option<String>,
    // Endpoint ids. None means Windows default, which is never saved.
    input_device: Option<String>,
    output_device: Option<String>,
    // Push to talk and constant-rate voice, the defaults, are never saved.
    open_mic: bool,
    variable_rate: bool,
    // None means room::DEFAULT_SENSITIVITY_DB.
    sensitivity_db: Option<i8>,
    no_echo_cancellation: bool,
    noise_suppression: bool,
    fixed_volume: bool,
    // None means room::DEFAULT_CUE_VOLUME.
    cue_volume: Option<u8>,
    no_sound_cues: bool,
    no_cue_joins: bool,
    no_cue_own_mute: bool,
    no_cue_friend_connection: bool,
    no_cue_own_connection: bool,
    no_cue_shares: bool,
    // Only the ones off their defaults are saved.
    hotkeys: Bindings,
    // None means DEFAULT_UPLOAD_MBITS. Vsync and the strip's hide are off
    // by default, and only on is saved.
    upload_mbits: Option<u32>,
    vsync: bool,
    hide_strip: bool,
    // The Windows name of the monitor picked last in Share's list, for the
    // share key. Written from inside a room, when it is picked.
    share_monitor: Option<String>,
    // The last thing picked there was a window or a region, so the share key
    // opens the list rather than show a whole monitor. Written as
    // share_monitor is.
    share_key_opens_list: bool,
    check_for_new_versions: bool,
    no_invite_links: bool,
    invite_link_wait_secs: Option<u32>,
    // The file could not be read as Booth started, so a default here says
    // nothing about what the file holds, and a save keeps that key's lines.
    // Stays for the run: nothing reads the file into this again.
    unread: bool,
    // What the saves of such a run have written. Those lines are the run's
    // own, so setting one of these keys back does take its line out. Empty in
    // any other run.
    saved_keys: Vec<&'static str>,
}

// A hotkey in the file that went back to its default, because a press of it
// would also have set off `with`, on `with_was` at that point: as the file had
// it, or its default if it had gone back already. Or a new action that moved to
// a free key instead, since a press of where it was would also have set off
// `with`. `now` and `with_now` are where reading the file left the two.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GaveWay {
    pub action: Action,
    pub was: Chord,
    pub with: Action,
    pub with_was: Chord,
    pub now: Chord,
    pub with_now: Chord,
}

impl Settings {
    // A missing file is a first run. Anything else that goes wrong costs the
    // setting it touches and a log line, never the start: a settings file
    // is no reason for Booth not to open. A key that gave way is handed back
    // as well, since the person talks with it and the log may not exist.
    pub fn load(dir: &Path) -> (Settings, Vec<String>, Vec<GaveWay>) {
        let path = dir.join(FILE);
        match read(&path) {
            Ok(bytes) => {
                let (mut settings, mut problems) = parse(&bytes);
                let gave_way = settle_hotkeys(&mut settings, &mut problems);
                (settings, problems, gave_way)
            }
            Err(err) => (
                Settings {
                    unread: true,
                    ..Settings::default()
                },
                vec![format!(
                    "settings: could not read {}: {err}; using the defaults",
                    path.display()
                )],
                Vec::new(),
            ),
        }
    }

    pub fn unread(&self) -> bool {
        self.unread
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

    pub fn sensitivity_db(&self) -> i8 {
        self.sensitivity_db.unwrap_or(DEFAULT_SENSITIVITY_DB)
    }

    pub fn set_sensitivity_db(&mut self, db: i8) {
        let db = db.clamp(*SENSITIVITY_DB.start(), *SENSITIVITY_DB.end());
        self.sensitivity_db = (db != DEFAULT_SENSITIVITY_DB).then_some(db);
    }

    pub fn echo_cancellation(&self) -> bool {
        !self.no_echo_cancellation
    }

    pub fn set_echo_cancellation(&mut self, on: bool) {
        self.no_echo_cancellation = !on;
    }

    pub fn noise_suppression(&self) -> bool {
        self.noise_suppression
    }

    pub fn set_noise_suppression(&mut self, on: bool) {
        self.noise_suppression = on;
    }

    pub fn automatic_volume(&self) -> bool {
        !self.fixed_volume
    }

    pub fn set_automatic_volume(&mut self, on: bool) {
        self.fixed_volume = !on;
    }

    pub fn sound_cues(&self) -> room::SoundCues {
        room::SoundCues {
            on: !self.no_sound_cues,
            volume: self.cue_volume.unwrap_or(room::DEFAULT_CUE_VOLUME),
            joins: !self.no_cue_joins,
            own_mute: !self.no_cue_own_mute,
            friend_connection: !self.no_cue_friend_connection,
            own_connection: !self.no_cue_own_connection,
            shares: !self.no_cue_shares,
        }
    }

    pub fn set_sound_cues(&mut self, cues: room::SoundCues) {
        self.no_sound_cues = !cues.on;
        let volume = cues
            .volume
            .clamp(*room::CUE_VOLUME.start(), *room::CUE_VOLUME.end());
        self.cue_volume = (volume != room::DEFAULT_CUE_VOLUME).then_some(volume);
        self.no_cue_joins = !cues.joins;
        self.no_cue_own_mute = !cues.own_mute;
        self.no_cue_friend_connection = !cues.friend_connection;
        self.no_cue_own_connection = !cues.own_connection;
        self.no_cue_shares = !cues.shares;
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
    // hotkeys, the sharing settings, and the devices and how you talk, which
    // Settings over a room applies at once. Compared by what they mean, so an
    // upload saved at its default is the same as one left out.
    pub fn locked_part_differs(&self, other: &Settings) -> bool {
        self.hotkeys != other.hotkeys
            || self.upload_mbits() != other.upload_mbits()
            || self.vsync != other.vsync
            || self.hide_strip != other.hide_strip
            || self.share_monitor != other.share_monitor
            || self.share_key_opens_list != other.share_key_opens_list
            || self.device(Direction::Input) != other.device(Direction::Input)
            || self.device(Direction::Output) != other.device(Direction::Output)
            || self.talk_mode() != other.talk_mode()
            || self.sensitivity_db() != other.sensitivity_db()
            || self.constant_rate() != other.constant_rate()
    }

    pub fn share_monitor(&self) -> Option<&str> {
        self.share_monitor.as_deref()
    }

    pub fn set_share_monitor(&mut self, device: Option<&str>) {
        self.share_monitor = device.map(str::to_owned);
    }

    pub fn share_key_opens_list(&self) -> bool {
        self.share_key_opens_list
    }

    pub fn set_share_key_opens_list(&mut self, on: bool) {
        self.share_key_opens_list = on;
    }

    pub fn check_for_new_versions(&self) -> bool {
        self.check_for_new_versions
    }

    pub fn set_check_for_new_versions(&mut self, on: bool) {
        self.check_for_new_versions = on;
    }

    pub fn invite_links(&self) -> bool {
        !self.no_invite_links
    }

    pub fn set_invite_links(&mut self, on: bool) {
        self.no_invite_links = !on;
    }

    pub fn invite_link_wait_secs(&self) -> u32 {
        self.invite_link_wait_secs.unwrap_or(1)
    }

    pub fn set_invite_link_wait_secs(&mut self, seconds: u32) {
        let seconds = seconds.clamp(1, 5);
        self.invite_link_wait_secs = (seconds != 1).then_some(seconds);
    }

    // The file is read again first, so a line added by hand while Booth was
    // open is kept too.
    pub fn save(&mut self, dir: &Path) -> Result<(), SaveError> {
        let path = dir.join(FILE);
        let old = read(&path).map_err(|source| SaveError::Read {
            path: path.clone(),
            source,
        })?;
        let new = self.merge(&old);
        // Merging can make the file longer, each line getting its \r. Written,
        // it would be refused at the next start.
        if new.len() as u64 > MOST_BYTES {
            return Err(SaveError::Write {
                path,
                source: io::Error::new(
                    io::ErrorKind::FileTooLarge,
                    "this save would take it over 64 KB, more than Booth reads",
                ),
            });
        }
        replace(dir, &path, &new)?;
        if self.unread {
            for key in KEYS {
                if !self.at_default(key) && !self.saved_keys.contains(&key) {
                    self.saved_keys.push(key);
                }
            }
        }
        Ok(())
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
                })
                .filter(|key| {
                    !self.unread || !self.at_default(key) || self.saved_keys.contains(key)
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

    // A key at its default is the one thing write_key leaves out.
    fn at_default(&self, key: &str) -> bool {
        let mut line = Vec::new();
        self.write_key(key, &mut line);
        line.is_empty()
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
            OPEN_MIC_SENSITIVITY => self.sensitivity_db.map(|db| db.to_string()),
            ECHO_CANCELLATION => self.no_echo_cancellation.then(|| String::from("off")),
            NOISE_SUPPRESSION => self.noise_suppression.then(|| String::from("on")),
            AUTOMATIC_VOLUME => self.fixed_volume.then(|| String::from("off")),
            SOUND_CUE_VOLUME => self.cue_volume.map(|volume| volume.to_string()),
            SOUND_CUES => self.no_sound_cues.then(|| String::from("off")),
            SOUND_CUE_JOINS => self.no_cue_joins.then(|| String::from("off")),
            SOUND_CUE_OWN_MUTE => self.no_cue_own_mute.then(|| String::from("off")),
            SOUND_CUE_FRIEND_CONNECTION => {
                self.no_cue_friend_connection.then(|| String::from("off"))
            }
            SOUND_CUE_OWN_CONNECTION => self.no_cue_own_connection.then(|| String::from("off")),
            SOUND_CUE_SHARES => self.no_cue_shares.then(|| String::from("off")),
            VIDEO_UPLOAD_MBITS => self.upload_mbits.map(|mbits| mbits.to_string()),
            VSYNC_IN_VIEWER => self.vsync.then(|| String::from("on")),
            HIDE_STRIP_IN_FULLSCREEN => self.hide_strip.then(|| String::from("on")),
            SHARE_MONITOR => self.share_monitor.clone(),
            SHARE_KEY_OPENS_LIST => self.share_key_opens_list.then(|| String::from("on")),
            CHECK_FOR_NEW_VERSIONS => self.check_for_new_versions.then(|| String::from("on")),
            INVITE_LINKS => self.no_invite_links.then(|| String::from("off")),
            INVITE_LINK_WAIT_SECS => self
                .invite_link_wait_secs
                .map(|seconds| seconds.to_string()),
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

    // Sets one key from the file. Ok carries the parts of the value that were
    // dropped, for the log; Err carries the reason the whole line was dropped.
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
            OPEN_MIC_SENSITIVITY => {
                let db = value
                    .parse::<i8>()
                    .ok()
                    .filter(|db| SENSITIVITY_DB.contains(db))
                    .ok_or_else(|| {
                        format!(
                            "{OPEN_MIC_SENSITIVITY} is a number of dB from {} to {}; set it again in settings",
                            SENSITIVITY_DB.start(),
                            SENSITIVITY_DB.end()
                        )
                    })?;
                self.set_sensitivity_db(db);
            }
            SOUND_CUE_VOLUME => {
                let volume = value
                    .bytes()
                    .all(|byte| byte.is_ascii_digit())
                    .then(|| value.parse::<u8>().ok())
                    .flatten()
                    .filter(|volume| room::CUE_VOLUME.contains(volume))
                    .ok_or_else(|| {
                        format!(
                            "{SOUND_CUE_VOLUME} is a percent from {} to {}; set it again in settings",
                            room::CUE_VOLUME.start(),
                            room::CUE_VOLUME.end()
                        )
                    })?;
                let mut cues = self.sound_cues();
                cues.volume = volume;
                self.set_sound_cues(cues);
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
            INVITE_LINK_WAIT_SECS => {
                let seconds = value
                    .bytes()
                    .all(|byte| byte.is_ascii_digit())
                    .then(|| value.parse::<u32>().ok())
                    .flatten()
                    .filter(|seconds| (1..=5).contains(seconds))
                    .ok_or_else(|| format!("{INVITE_LINK_WAIT_SECS} is a number of seconds from 1 to 5; set it again in settings"))?;
                self.set_invite_link_wait_secs(seconds);
            }
            AUTOMATIC_VOLUME => {
                self.fixed_volume = match value {
                    "on" => false,
                    "off" => true,
                    _ => {
                        return Err(format!(
                            "{AUTOMATIC_VOLUME} is on or off; set it again in settings"
                        ));
                    }
                };
            }
            VSYNC_IN_VIEWER
            | HIDE_STRIP_IN_FULLSCREEN
            | CHECK_FOR_NEW_VERSIONS
            | INVITE_LINKS
            | NOISE_SUPPRESSION
            | ECHO_CANCELLATION
            | SOUND_CUES
            | SOUND_CUE_JOINS
            | SOUND_CUE_OWN_MUTE
            | SOUND_CUE_FRIEND_CONNECTION
            | SOUND_CUE_OWN_CONNECTION
            | SOUND_CUE_SHARES => {
                let on = match value {
                    "on" => true,
                    "off" => false,
                    _ => return Err(format!("{key} is on or off; set it again in settings")),
                };
                match key {
                    VSYNC_IN_VIEWER => self.vsync = on,
                    HIDE_STRIP_IN_FULLSCREEN => self.hide_strip = on,
                    NOISE_SUPPRESSION => self.noise_suppression = on,
                    ECHO_CANCELLATION => self.no_echo_cancellation = !on,
                    INVITE_LINKS => self.no_invite_links = !on,
                    SOUND_CUES => self.no_sound_cues = !on,
                    SOUND_CUE_JOINS => self.no_cue_joins = !on,
                    SOUND_CUE_OWN_MUTE => self.no_cue_own_mute = !on,
                    SOUND_CUE_FRIEND_CONNECTION => self.no_cue_friend_connection = !on,
                    SOUND_CUE_OWN_CONNECTION => self.no_cue_own_connection = !on,
                    SOUND_CUE_SHARES => self.no_cue_shares = !on,
                    _ => self.check_for_new_versions = on,
                }
            }
            SHARE_MONITOR => {
                self.share_monitor = monitor_name(value)?;
            }
            SHARE_KEY_OPENS_LIST => {
                self.share_key_opens_list = match value {
                    "on" => true,
                    "off" => false,
                    _ => {
                        return Err(format!(
                            "{SHARE_KEY_OPENS_LIST} is on or off; pick what to share again after Share"
                        ));
                    }
                };
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
        HOTKEY_PAUSE_SHARE => Action::PauseShare,
        HOTKEY_PANIC => Action::Panic,
        HOTKEY_SHOW_PANEL => Action::ShowPanel,
        HOTKEY_STATS_PANEL => Action::StatsPanel,
        _ => return None,
    })
}

// Actions some settings files are older than: pause sharing came in 1.3.0. Such
// a file has no line for one, so a press of its default can set off a key
// chosen there for another action. A copy older than such an action does not
// know where it is either, so it can put another key on the spare it went to.
const NEW_ACTIONS: [Action; 1] = [Action::PauseShare];
// Where such an action goes then, if its default is not free: the first of
// these that is. There are as many as other actions, and one more with the
// default, and since no default is on their keys, each other action keeps it
// off one at most: by its key or, if it went back over a clash with this one,
// by the key it left. Not F11, which the viewer goes in and out of fullscreen
// on whatever else is held.
const SPARE_CHORDS: [&str; 7] = [
    "Ctrl+Shift+F10",
    "Ctrl+Shift+F12",
    "Ctrl+Shift+F8",
    "Ctrl+Shift+F7",
    "Ctrl+Shift+F6",
    "Ctrl+Shift+F5",
    "Ctrl+Shift+F3",
];

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
            SaveError::Replace { path, source } => {
                write!(f, "could not replace {}: {source}", path.display())
            }
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
pub(crate) fn read(path: &Path) -> io::Result<Vec<u8>> {
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

// Written in full next to the old file, flushed to disk, then renamed over
// it, so a crash or a full disk leaves the old file or the new one and
// never half of either. Windows replaces the target in the rename.
pub(crate) fn replace(dir: &Path, path: &Path, bytes: &[u8]) -> Result<(), SaveError> {
    let temp = temp_path(dir, path);
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
fn temp_path(dir: &Path, path: &Path) -> PathBuf {
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    dir.join(format!("{stem}.{}.tmp", std::process::id()))
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
    (settings, problems)
}

// Booth does not put two actions on one key in a file it has read. A clash
// comes from editing by hand, from a newer version giving a new action, as its
// default, a key chosen earlier for another, or from a key moved in a run that
// could not read the file. Only an action moved off its default gives way, so
// the rest of what was chosen stays; of two moved ones, which was meant is
// anyone's guess, so both do. A new action on its default or a spare, as the
// file had it or once it went back, was most likely put there by Booth and not
// chosen, so it is the one that moves, and last, once the other keys are where
// they stay. It does not take the key one that went back over a clash with it
// has left, or that one alone would have given way.
fn settle_hotkeys(settings: &mut Settings, problems: &mut Vec<String>) -> Vec<GaveWay> {
    let mut gave_way: Vec<GaveWay> = Vec::new();
    let spares: Vec<Chord> = SPARE_CHORDS
        .into_iter()
        .filter_map(|chord| chord.parse().ok())
        .collect();
    let mut new: Vec<Action> = NEW_ACTIONS
        .into_iter()
        .filter(|action| {
            let chord = settings.hotkeys.chord(*action);
            chord == action.default_chord() || spares.contains(&chord)
        })
        .collect();
    while let Some((one, other)) = settings.hotkeys.first_conflict_without(&new) {
        let chord = settings.hotkeys.chord(one);
        let other_chord = settings.hotkeys.chord(other);
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
            let (with, with_was) = if action == one {
                (other, other_chord)
            } else {
                (one, chord)
            };
            gave_way.push(GaveWay {
                action,
                was: settings.hotkeys.chord(action),
                with,
                with_was,
                now: action.default_chord(),
                with_now: with.default_chord(),
            });
            settings.hotkeys.set(action, action.default_chord());
            if NEW_ACTIONS.contains(&action) {
                new.push(action);
            }
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
    for action in new {
        let was = settings.hotkeys.chord(action);
        let Some(with) = settings.hotkeys.conflict(action, was) else {
            continue;
        };
        let with_was = settings.hotkeys.chord(with);
        let free = std::iter::once(action.default_chord())
            .chain(spares.iter().copied())
            .find(|chord| {
                settings.hotkeys.conflict(action, *chord).is_none()
                    && !gave_way
                        .iter()
                        .any(|gave| gave.with == action && gave.was == *chord)
            });
        // Never, by the count above; the keys must not clash even so.
        let Some(now) = free else {
            problems.push(format!(
                "settings: no free key for {}; using the default hotkeys",
                action.name()
            ));
            settings.hotkeys = Bindings::default();
            break;
        };
        settings.hotkeys.set(action, now);
        // If it went back to its default above, it is not there now, and a key
        // that gave way to it is told where it is.
        gave_way.retain(|gave| gave.action != action);
        for gave in &mut gave_way {
            if gave.with == action {
                gave.with_now = now;
            }
        }
        gave_way.push(GaveWay {
            action,
            was,
            with,
            with_was,
            now,
            with_now: with_was,
        });
        problems.push(format!(
            "settings: {} and {} are on one key, {with_was}; {} goes to {now}",
            with.name(),
            action.name(),
            action.name()
        ));
    }
    gave_way
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

    fn settled(bytes: &[u8]) -> (Settings, Vec<String>) {
        let (mut settings, mut problems) = parse(bytes);
        settle_hotkeys(&mut settings, &mut problems);
        (settings, problems)
    }

    #[test]
    fn a_missing_file_is_the_defaults_and_no_complaint() {
        let folder = Folder::new("missing");
        let (settings, problems, _) = Settings::load(&folder.0);
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
        let (settings, problems, _) = Settings::load(&folder.0);
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
        let (settings, problems, _) = Settings::load(&folder.0);
        assert_eq!(settings.address_name(), Some("a.example.net"));
        assert_eq!(problems, ["settings: line 1 skipped: it is not UTF-8 text"]);
        named("b.example.net").save(&folder.0).unwrap();
        assert_eq!(
            fs::read(folder.file()).unwrap(),
            b"name = Andr\xE9\r\naddress_name = b.example.net\r\n"
        );
    }

    #[test]
    fn broken_lines_are_skipped_and_reported() {
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
        fs::create_dir(temp_path(&folder.0, &folder.file())).unwrap();
        let err = named("new.example.net").save(&folder.0).unwrap_err();
        assert!(matches!(err, SaveError::Write { .. }), "{err}");
        assert_eq!(fs::read_to_string(folder.file()).unwrap(), before);

        fs::remove_dir(temp_path(&folder.0, &folder.file())).unwrap();
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
        let (settings, problems, _) = Settings::load(&folder.0);
        assert!(settings.unread);
        assert_eq!(
            Settings {
                unread: false,
                ..settings
            },
            Settings::default()
        );
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].ends_with("; using the defaults"),
            "{problems:?}"
        );
        let err = named("a.example.net").save(&folder.0).unwrap_err();
        assert!(matches!(err, SaveError::Read { .. }), "{err}");
    }

    // The other program has let go by the time of the save. Nothing here saw
    // what the file holds, so a key still at its default says nothing about
    // it, and only what was changed goes in. The settings screen saves a clone,
    // so the flag has to travel with it.
    #[test]
    fn a_file_read_again_after_a_failed_read_keeps_what_was_never_seen() {
        let folder = Folder::new("unread-comes-back");
        fs::create_dir(folder.file()).unwrap();
        let (settings, _, _) = Settings::load(&folder.0);
        fs::remove_dir(folder.file()).unwrap();
        fs::write(
            folder.file(),
            "name = Mara\r\nport = 99\r\nport = 41010\r\nvideo_upload = 15\r\naddress_name = old.example.net\r\nhotkey_mute = F9\r\n",
        )
        .unwrap();

        let mut next = settings.clone();
        next.set_share_monitor(Some(r"\\.\DISPLAY2"));
        next.set_address_name("new.example.net").unwrap();
        next.save(&folder.0).unwrap();
        assert_eq!(
            fs::read_to_string(folder.file()).unwrap(),
            "name = Mara\r\nport = 99\r\nport = 41010\r\nvideo_upload = 15\r\naddress_name = new.example.net\r\nhotkey_mute = F9\r\nshare_monitor = \\\\.\\DISPLAY2\r\n"
        );

        // The next start reads it all and is back to the usual save.
        let (mut loaded, _, _) = Settings::load(&folder.0);
        assert_eq!(loaded.name(), Some("Mara"));
        assert_eq!(loaded.port(), 41010);
        assert_eq!(loaded.hotkeys().chord(Action::Mute).to_string(), "F9");
        assert_eq!(loaded.share_monitor(), Some(r"\\.\DISPLAY2"));
        loaded.set_name("");
        loaded.save(&folder.0).unwrap();
        assert!(
            !fs::read_to_string(folder.file())
                .unwrap()
                .contains("name = Mara")
        );
    }

    // A line this run wrote is not one it never saw: set back, the key's line
    // goes and the next start has what the panel showed. The file's other
    // lines stay, and so does a line of a key whose only save failed.
    #[test]
    fn a_key_set_back_after_a_failed_read_loses_the_line_this_run_wrote() {
        let folder = Folder::new("unread-set-back");
        fs::create_dir(folder.file()).unwrap();
        let (mut settings, _, _) = Settings::load(&folder.0);
        fs::remove_dir(folder.file()).unwrap();
        fs::write(
            folder.file(),
            "name = Mara\r\ncheck_for_new_versions = on\r\nvsync_in_viewer = on\r\n",
        )
        .unwrap();

        // A folder where the new file would go stands in for a full disk.
        fs::create_dir(temp_path(&folder.0, &folder.file())).unwrap();
        settings.set_vsync(true);
        assert!(settings.save(&folder.0).is_err());
        settings.set_vsync(false);
        fs::remove_dir(temp_path(&folder.0, &folder.file())).unwrap();

        let mut next = settings.clone();
        next.set_check_for_new_versions(true);
        next.set_talk_mode(TalkMode::OpenMic);
        next.set_constant_rate(false);
        next.set_address_name("a.example.net").unwrap();
        next.save(&folder.0).unwrap();
        assert_eq!(
            fs::read_to_string(folder.file()).unwrap(),
            "name = Mara\r\ncheck_for_new_versions = on\r\nvsync_in_viewer = on\r\naddress_name = a.example.net\r\ntalk_mode = open_mic\r\nconstant_rate_voice = off\r\n"
        );

        // Each save goes through a clone of the last, as the settings screen's do.
        let mut last = next.clone();
        last.set_check_for_new_versions(false);
        last.set_talk_mode(TalkMode::PushToTalk);
        last.set_constant_rate(true);
        last.set_address_name("").unwrap();
        last.save(&folder.0).unwrap();
        assert_eq!(
            fs::read_to_string(folder.file()).unwrap(),
            "name = Mara\r\nvsync_in_viewer = on\r\n"
        );
        let (loaded, _, _) = Settings::load(&folder.0);
        assert!(!loaded.check_for_new_versions());
        assert_eq!(loaded.talk_mode(), TalkMode::PushToTalk);
        assert!(loaded.constant_rate());
        assert_eq!(loaded.address_name(), None);
        assert_eq!(loaded.name(), Some("Mara"));
        assert!(loaded.vsync());
    }

    // Under the limit a file full of nothing is read as ever, and over it no
    // line of it is looked at. Saving cannot go on without reading what else
    // is in it, so the file stays until someone has dealt with it.
    #[test]
    fn a_file_over_the_limit_is_not_read() {
        let folder = Folder::new("too-big");
        let at_limit = vec![b'\n'; MOST_BYTES as usize];
        fs::write(folder.file(), &at_limit).unwrap();
        let (settings, problems, _) = Settings::load(&folder.0);
        assert!(!settings.unread);
        assert!(problems.is_empty(), "{problems:?}");

        let over = vec![b'\n'; MOST_BYTES as usize + 1];
        fs::write(folder.file(), &over).unwrap();
        let (settings, problems, _) = Settings::load(&folder.0);
        assert!(settings.unread);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0]
                .ends_with("it is over 64 KB, more than Booth ever writes; using the defaults"),
            "{problems:?}"
        );
        let err = named("a.example.net").save(&folder.0).unwrap_err();
        let SaveError::Read { source, .. } = &err else {
            panic!("{err}");
        };
        assert_eq!(source.kind(), io::ErrorKind::FileTooLarge);
        assert_eq!(
            crate::messages::settings_error(&err),
            "Could not save settings.txt: it is over 64 KB, more than Booth ever writes. Make it smaller or delete it, then press Save again."
        );
        assert_eq!(fs::read(folder.file()).unwrap(), over);
    }

    // Each "\n" gets its "\r" in a save, so a file under the limit can come out
    // over it, and the next start would refuse the file this one wrote.
    #[test]
    fn a_save_that_would_go_over_the_limit_is_refused() {
        let folder = Folder::new("would-be-too-big");
        let before = vec![b'\n'; MOST_BYTES as usize / 2 + 1];
        fs::write(folder.file(), &before).unwrap();
        let err = named("a.example.net").save(&folder.0).unwrap_err();
        let SaveError::Write { source, .. } = &err else {
            panic!("{err}");
        };
        assert_eq!(source.kind(), io::ErrorKind::FileTooLarge);
        assert_eq!(
            crate::messages::settings_error(&err),
            "Could not save settings.txt: this save would take it over 64 KB, more than Booth reads. Make it smaller or delete it, then press Save again."
        );
        assert_eq!(fs::read(folder.file()).unwrap(), before);
        assert!(!temp_path(&folder.0, &folder.file()).exists());
    }

    // Saved by an older version, before pause sharing had Ctrl+Shift+F9. Push
    // to talk also runs with more modifiers down, so that press would set off
    // both. Push to talk's key was chosen and pausing's never was, so pausing
    // goes to a free key. The log says so, and so does what is handed on for
    // the start screen.
    #[test]
    fn a_key_that_gave_way_is_handed_back() {
        let folder = Folder::new("gave-way");
        fs::write(
            folder.file(),
            "hotkey_push_to_talk = F9\r\nhotkey_stats_panel = F10\r\n",
        )
        .unwrap();
        let (settings, problems, gave_way) = Settings::load(&folder.0);
        assert_eq!(
            settings.hotkeys().chord(Action::PushToTalk).to_string(),
            "F9"
        );
        assert_eq!(
            settings.hotkeys().chord(Action::StatsPanel).to_string(),
            "F10"
        );
        assert_eq!(
            settings.hotkeys().chord(Action::PauseShare).to_string(),
            "Ctrl+Shift+F10"
        );
        assert_eq!(
            problems,
            [
                "settings: push to talk and pause sharing, resume (press twice) are on one key, F9; pause sharing, resume (press twice) goes to Ctrl+Shift+F10"
            ]
        );
        assert_eq!(
            gave_way,
            [GaveWay {
                action: Action::PauseShare,
                was: "Ctrl+Shift+F9".parse().unwrap(),
                with: Action::PushToTalk,
                with_was: "F9".parse().unwrap(),
                now: "Ctrl+Shift+F10".parse().unwrap(),
                with_now: "F9".parse().unwrap(),
            }]
        );

        // Both moved: each is told, with the other.
        fs::write(folder.file(), "hotkey_mute = F8\r\nhotkey_deafen = F8\r\n").unwrap();
        let (_, _, gave_way) = Settings::load(&folder.0);
        let f8: Chord = "F8".parse().unwrap();
        assert_eq!(
            gave_way,
            [
                GaveWay {
                    action: Action::Mute,
                    was: f8,
                    with: Action::Deafen,
                    with_was: f8,
                    now: Action::Mute.default_chord(),
                    with_now: Action::Deafen.default_chord(),
                },
                GaveWay {
                    action: Action::Deafen,
                    was: f8,
                    with: Action::Mute,
                    with_was: f8,
                    now: Action::Deafen.default_chord(),
                    with_now: Action::Mute.default_chord(),
                },
            ]
        );

        // Only the one moved off its default gives way, here the second named.
        fs::write(folder.file(), "hotkey_deafen = Ctrl+Shift+M\r\n").unwrap();
        let (_, _, gave_way) = Settings::load(&folder.0);
        assert_eq!(
            gave_way,
            [GaveWay {
                action: Action::Deafen,
                was: "Ctrl+Shift+M".parse().unwrap(),
                with: Action::Mute,
                with_was: "Ctrl+Shift+M".parse().unwrap(),
                now: Action::Deafen.default_chord(),
                with_now: Action::Mute.default_chord(),
            }]
        );

        // Two moved, on two chords of one key: each is told the other's.
        fs::write(
            folder.file(),
            "hotkey_push_to_talk = F5\r\nhotkey_mute = Ctrl+F5\r\n",
        )
        .unwrap();
        let (_, _, gave_way) = Settings::load(&folder.0);
        assert_eq!(
            gave_way,
            [
                GaveWay {
                    action: Action::PushToTalk,
                    was: "F5".parse().unwrap(),
                    with: Action::Mute,
                    with_was: "Ctrl+F5".parse().unwrap(),
                    now: Action::PushToTalk.default_chord(),
                    with_now: Action::Mute.default_chord(),
                },
                GaveWay {
                    action: Action::Mute,
                    was: "Ctrl+F5".parse().unwrap(),
                    with: Action::PushToTalk,
                    with_was: "F5".parse().unwrap(),
                    now: Action::Mute.default_chord(),
                    with_now: Action::PushToTalk.default_chord(),
                },
            ]
        );

        // In a chain the second is told where the first went back to, not
        // where the file had it.
        fs::write(
            folder.file(),
            "hotkey_mute = Ctrl+Shift+S\r\nhotkey_deafen = Ctrl+Shift+M\r\n",
        )
        .unwrap();
        let (_, _, gave_way) = Settings::load(&folder.0);
        assert_eq!(
            gave_way,
            [
                GaveWay {
                    action: Action::Mute,
                    was: "Ctrl+Shift+S".parse().unwrap(),
                    with: Action::Share,
                    with_was: "Ctrl+Shift+S".parse().unwrap(),
                    now: Action::Mute.default_chord(),
                    with_now: Action::Share.default_chord(),
                },
                GaveWay {
                    action: Action::Deafen,
                    was: "Ctrl+Shift+M".parse().unwrap(),
                    with: Action::Mute,
                    with_was: "Ctrl+Shift+M".parse().unwrap(),
                    now: Action::Deafen.default_chord(),
                    with_now: Action::Mute.default_chord(),
                },
            ]
        );

        // A plain F9 on mute sets off nothing else, and nothing is said.
        fs::write(folder.file(), "hotkey_mute = F9\r\n").unwrap();
        let (settings, problems, gave_way) = Settings::load(&folder.0);
        assert_eq!(settings.hotkeys().chord(Action::Mute).to_string(), "F9");
        assert!(problems.is_empty() && gave_way.is_empty(), "{problems:?}");
    }

    // As 1.2.0 saves it, before pause sharing had Ctrl+Shift+F9: push to talk
    // or the panic key on F9, alone or with Shift, Ctrl or both, or another key
    // on exactly Ctrl+Shift+F9. Every key stays where the person put it, and
    // pausing takes the first free one. A save keeps that, so the next start
    // has nothing to settle.
    #[test]
    fn an_upgrade_from_1_2_0_keeps_every_key() {
        let folder = Folder::new("from-1.2.0");
        let pause_now: Chord = "Ctrl+Shift+F12".parse().unwrap();
        for (key, chord) in [
            (HOTKEY_PUSH_TO_TALK, "F9"),
            (HOTKEY_PUSH_TO_TALK, "Shift+F9"),
            (HOTKEY_PUSH_TO_TALK, "Ctrl+F9"),
            (HOTKEY_PUSH_TO_TALK, "Ctrl+Shift+F9"),
            (HOTKEY_PANIC, "F9"),
            (HOTKEY_PANIC, "Shift+F9"),
            (HOTKEY_PANIC, "Ctrl+F9"),
            (HOTKEY_PANIC, "Ctrl+Shift+F9"),
            (HOTKEY_MUTE, "Ctrl+Shift+F9"),
        ] {
            let before = format!(
                "name = Mara\r\n{key} = {chord}\r\nhotkey_stats_panel = Ctrl+Shift+F10\r\ntalk_mode = open_mic\r\nshare_monitor = \\\\.\\DISPLAY2\r\n"
            );
            fs::write(folder.file(), &before).unwrap();
            let action = hotkey_action(key).unwrap();
            let chord: Chord = chord.parse().unwrap();
            let mut expected = Bindings::default();
            expected.set(action, chord);
            expected.set(Action::StatsPanel, "Ctrl+Shift+F10".parse().unwrap());
            expected.set(Action::PauseShare, pause_now);

            let (mut settings, problems, gave_way) = Settings::load(&folder.0);
            assert_eq!(settings.hotkeys(), expected, "{key} = {chord}");
            assert_eq!(
                problems,
                [format!(
                    "settings: {} and pause sharing, resume (press twice) are on one key, {chord}; pause sharing, resume (press twice) goes to Ctrl+Shift+F12",
                    action.name()
                )]
            );
            assert_eq!(
                gave_way,
                [GaveWay {
                    action: Action::PauseShare,
                    was: Action::PauseShare.default_chord(),
                    with: action,
                    with_was: chord,
                    now: pause_now,
                    with_now: chord,
                }]
            );

            settings.save(&folder.0).unwrap();
            assert_eq!(
                fs::read_to_string(folder.file()).unwrap(),
                format!("{before}hotkey_pause_share = Ctrl+Shift+F12\r\n")
            );
            let (again, problems, gave_way) = Settings::load(&folder.0);
            assert!(problems.is_empty() && gave_way.is_empty(), "{problems:?}");
            assert_eq!(again.hotkeys(), expected);
        }
    }

    // 1.3.0 let push to talk and the panic key share a key when neither's
    // modifiers held the other's. Holding Ctrl and Shift sets off both, and
    // which was meant is anyone's guess, so both go back.
    #[test]
    fn an_upgrade_from_1_3_0_sends_push_to_talk_and_panic_on_one_key_back() {
        let (mut settings, mut problems) =
            parse(b"hotkey_push_to_talk = Ctrl+K\nhotkey_panic = Shift+K\n");
        let gave_way = settle_hotkeys(&mut settings, &mut problems);
        assert_eq!(settings.hotkeys(), Bindings::default());
        assert_eq!(
            problems,
            [
                "settings: push to talk and panic key are on one key, Ctrl+K; push to talk goes back to Right Ctrl and panic key goes back to Ctrl+Shift+End"
            ]
        );
        let ctrl_k: Chord = "Ctrl+K".parse().unwrap();
        let shift_k: Chord = "Shift+K".parse().unwrap();
        assert_eq!(
            gave_way,
            [
                GaveWay {
                    action: Action::PushToTalk,
                    was: ctrl_k,
                    with: Action::Panic,
                    with_was: shift_k,
                    now: Action::PushToTalk.default_chord(),
                    with_now: Action::Panic.default_chord(),
                },
                GaveWay {
                    action: Action::Panic,
                    was: shift_k,
                    with: Action::PushToTalk,
                    with_was: ctrl_k,
                    now: Action::Panic.default_chord(),
                    with_now: Action::PushToTalk.default_chord(),
                },
            ]
        );
    }

    // Two keys of the file on F9 go back first, as 1.2.0 had them go, and then
    // pausing's default is free and stays.
    #[test]
    fn keys_of_the_file_are_settled_before_a_new_action_moves() {
        let (settings, problems) = settled(b"hotkey_panic = F9\nhotkey_stats_panel = F9\n");
        assert_eq!(settings.hotkeys(), Bindings::default());
        assert_eq!(
            problems,
            [
                "settings: panic key and stats panel are on one key, F9; panic key goes back to Ctrl+Shift+End and stats panel goes back to Ctrl+Shift+I"
            ]
        );
    }

    // Pausing and mute on one key in the file, each moved there: both go back,
    // and a press of pausing's default then sets off push to talk or the panic
    // key on F9 as well. That default was never chosen either, so pausing moves
    // on and F9 stays, and what is handed on for the start screen says where
    // the keys are now.
    #[test]
    fn a_new_action_that_went_back_moves_on() {
        let f9: Chord = "F9".parse().unwrap();
        let both_were: Chord = "F8".parse().unwrap();
        let spare: Chord = "Ctrl+Shift+F10".parse().unwrap();
        for key in [HOTKEY_PUSH_TO_TALK, HOTKEY_PANIC] {
            let (mut settings, mut problems) = parse(
                format!("{key} = F9\nhotkey_pause_share = F8\nhotkey_mute = F8\n").as_bytes(),
            );
            let gave_way = settle_hotkeys(&mut settings, &mut problems);
            let action = hotkey_action(key).unwrap();
            let mut expected = Bindings::default();
            expected.set(action, f9);
            expected.set(Action::PauseShare, spare);
            assert_eq!(settings.hotkeys(), expected, "{key}");
            assert_eq!(settings.hotkeys().first_conflict(), None);
            assert_eq!(
                problems,
                [
                    "settings: mute and pause sharing, resume (press twice) are on one key, F8; mute goes back to Ctrl+Shift+M and pause sharing, resume (press twice) goes back to Ctrl+Shift+F9".to_string(),
                    format!(
                        "settings: {} and pause sharing, resume (press twice) are on one key, F9; pause sharing, resume (press twice) goes to Ctrl+Shift+F10",
                        action.name()
                    ),
                ]
            );
            assert_eq!(
                gave_way,
                [
                    GaveWay {
                        action: Action::Mute,
                        was: both_were,
                        with: Action::PauseShare,
                        with_was: both_were,
                        now: Action::Mute.default_chord(),
                        with_now: spare,
                    },
                    GaveWay {
                        action: Action::PauseShare,
                        was: Action::PauseShare.default_chord(),
                        with: action,
                        with_was: f9,
                        now: spare,
                        with_now: f9,
                    },
                ]
            );
        }
    }

    // A save after pausing moved writes the spare it is on, and a copy older
    // than pausing lets any key go there. That key was chosen and the spare was
    // not, so pausing alone moves: back to its default if that is free, or else
    // on to the next spare.
    #[test]
    fn a_key_put_on_a_new_actions_spare_stays() {
        let spare: Chord = "Ctrl+Shift+F10".parse().unwrap();
        for key in [HOTKEY_PUSH_TO_TALK, HOTKEY_PANIC] {
            let (mut settings, mut problems) = parse(
                format!("{key} = Ctrl+Shift+F10\nhotkey_pause_share = Ctrl+Shift+F10\n").as_bytes(),
            );
            let gave_way = settle_hotkeys(&mut settings, &mut problems);
            let action = hotkey_action(key).unwrap();
            let mut expected = Bindings::default();
            expected.set(action, spare);
            assert_eq!(settings.hotkeys(), expected, "{key}");
            assert_eq!(settings.hotkeys().first_conflict(), None);
            assert_eq!(
                problems,
                [format!(
                    "settings: {} and pause sharing, resume (press twice) are on one key, Ctrl+Shift+F10; pause sharing, resume (press twice) goes to Ctrl+Shift+F9",
                    action.name()
                )]
            );
            assert_eq!(
                gave_way,
                [GaveWay {
                    action: Action::PauseShare,
                    was: spare,
                    with: action,
                    with_was: spare,
                    now: Action::PauseShare.default_chord(),
                    with_now: spare,
                }]
            );
        }

        let (mut settings, mut problems) = parse(
            b"hotkey_push_to_talk = F9\nhotkey_mute = Ctrl+Shift+F10\nhotkey_pause_share = Ctrl+Shift+F10\n",
        );
        let gave_way = settle_hotkeys(&mut settings, &mut problems);
        let next: Chord = "Ctrl+Shift+F12".parse().unwrap();
        let mut expected = Bindings::default();
        expected.set(Action::PushToTalk, "F9".parse().unwrap());
        expected.set(Action::Mute, spare);
        expected.set(Action::PauseShare, next);
        assert_eq!(settings.hotkeys(), expected);
        assert_eq!(settings.hotkeys().first_conflict(), None);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert_eq!(
            gave_way,
            [GaveWay {
                action: Action::PauseShare,
                was: spare,
                with: Action::Mute,
                with_was: spare,
                now: next,
                with_now: spare,
            }]
        );
    }

    // Push to talk or the panic key, and pausing on a chord of its key with
    // more modifiers, both go back, and the other of the two on F9 keeps
    // pausing off its default. Pausing then does not move on to the key the
    // first just left, or only that one would have given way.
    #[test]
    fn a_new_action_takes_no_key_given_up_over_it() {
        for (key, on_f9) in [
            (HOTKEY_PUSH_TO_TALK, HOTKEY_PANIC),
            (HOTKEY_PANIC, HOTKEY_PUSH_TO_TALK),
        ] {
            let (mut settings, mut problems) = parse(
                format!(
                    "{on_f9} = F9\n{key} = Ctrl+Shift+F10\nhotkey_pause_share = Ctrl+Shift+Alt+F10\n"
                )
                .as_bytes(),
            );
            let gave_way = settle_hotkeys(&mut settings, &mut problems);
            let (action, other) = (hotkey_action(key).unwrap(), hotkey_action(on_f9).unwrap());
            let (f9, next): (Chord, Chord) =
                ("F9".parse().unwrap(), "Ctrl+Shift+F12".parse().unwrap());
            let mut expected = Bindings::default();
            expected.set(other, f9);
            expected.set(Action::PauseShare, next);
            assert_eq!(settings.hotkeys(), expected, "{key}");
            assert_eq!(settings.hotkeys().first_conflict(), None);
            assert_eq!(problems.len(), 2, "{problems:?}");
            assert_eq!(
                gave_way,
                [
                    GaveWay {
                        action,
                        was: "Ctrl+Shift+F10".parse().unwrap(),
                        with: Action::PauseShare,
                        with_was: "Ctrl+Shift+Alt+F10".parse().unwrap(),
                        now: action.default_chord(),
                        with_now: next,
                    },
                    GaveWay {
                        action: Action::PauseShare,
                        was: Action::PauseShare.default_chord(),
                        with: other,
                        with_was: f9,
                        now: next,
                        with_now: f9,
                    },
                ]
            );
        }
    }

    // Each spare is on a key of its own and not any default's, one for each
    // other action, so with all of them on F keys the last is still free. None
    // is on F11, which the viewer acts on whatever else is held.
    #[test]
    fn a_new_action_always_finds_a_free_key() {
        let spares: Vec<Chord> = SPARE_CHORDS
            .into_iter()
            .map(|text| text.parse().unwrap())
            .collect();
        let mut keys: Vec<_> = spares.iter().map(|spare| spare.key).collect();
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), spares.len());
        for action in Action::ALL {
            assert!(!keys.contains(&action.default_chord().key));
        }
        assert!(!keys.contains(&input::Key::F11));
        assert!(spares.len() >= Action::ALL.len() - 1);

        let (settings, problems) = settled(
            b"hotkey_push_to_talk = F9\nhotkey_mute = Ctrl+Shift+F10\nhotkey_deafen = Ctrl+Shift+F12\nhotkey_share = Ctrl+Shift+F8\nhotkey_panic = Ctrl+Shift+F7\nhotkey_show_panel = Ctrl+Shift+F6\nhotkey_stats_panel = Ctrl+Shift+F5\n",
        );
        assert_eq!(
            settings.hotkeys().chord(Action::PauseShare).to_string(),
            "Ctrl+Shift+F3"
        );
        assert_eq!(settings.hotkeys().first_conflict(), None);
        assert_eq!(problems.len(), 1, "{problems:?}");
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
        let (loaded, problems, _) = Settings::load(&folder.0);
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
    fn the_file_s_own_mistakes_are_reported_and_skipped() {
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
        let (loaded, problems, _) = Settings::load(&folder.0);
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
        let (loaded, problems, _) = Settings::load(&folder.0);
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

    // Written only when moved off -50, kept within -60 to -20, and a value
    // outside that or not a number is a line skipped and said.
    #[test]
    fn open_mic_sensitivity_round_trip() {
        let folder = Folder::new("sensitivity");
        let mut settings = Settings::default();
        assert_eq!(settings.sensitivity_db(), -50);
        settings.set_sensitivity_db(-35);
        settings.save(&folder.0).unwrap();
        assert_eq!(
            fs::read_to_string(folder.file()).unwrap(),
            "open_mic_sensitivity = -35\r\n"
        );
        let (loaded, _, _) = Settings::load(&folder.0);
        assert_eq!(loaded.sensitivity_db(), -35);
        settings.set_sensitivity_db(-90);
        assert_eq!(settings.sensitivity_db(), -60);
        settings.set_sensitivity_db(-50);
        assert_eq!(settings, Settings::default());
        let (settings, problems) =
            parse(b"open_mic_sensitivity = -10\nopen_mic_sensitivity = loud\n");
        assert_eq!(settings, Settings::default());
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(problems[0].ends_with(
            "open_mic_sensitivity is a number of dB from -60 to -20; set it again in settings"
        ));
    }

    #[test]
    fn echo_cancellation_round_trip() {
        let folder = Folder::new("echo cancellation");
        let mut settings = Settings::default();
        assert!(settings.echo_cancellation());
        settings.set_echo_cancellation(false);
        settings.save(&folder.0).unwrap();
        assert_eq!(
            fs::read_to_string(folder.file()).unwrap(),
            "echo_cancellation = off\r\n"
        );
        assert!(!Settings::load(&folder.0).0.echo_cancellation());
    }

    #[test]
    fn noise_suppression_round_trip() {
        let folder = Folder::new("noise suppression");
        let mut settings = Settings::default();
        assert!(!settings.noise_suppression());
        settings.set_noise_suppression(true);
        settings.save(&folder.0).unwrap();
        assert_eq!(
            fs::read_to_string(folder.file()).unwrap(),
            "noise_suppression = on\r\n"
        );
        assert!(Settings::load(&folder.0).0.noise_suppression());
    }

    #[test]
    fn automatic_volume_round_trip() {
        let folder = Folder::new("automatic volume");
        let mut settings = Settings::default();
        assert!(settings.automatic_volume());
        settings.set_automatic_volume(false);
        settings.save(&folder.0).unwrap();
        assert_eq!(
            fs::read_to_string(folder.file()).unwrap(),
            "automatic_volume = off\r\n"
        );
        assert!(!Settings::load(&folder.0).0.automatic_volume());
        let (loaded, problems) = parse(b"automatic_volume = on\n");
        assert!(problems.is_empty() && loaded == Settings::default());
    }

    #[test]
    fn sound_cue_keys_round_trip() {
        let mut settings = Settings::default();
        assert_eq!(settings.sound_cues(), room::SoundCues::default());
        assert!(settings.merge(b"").is_empty());
        let cues = room::SoundCues {
            on: false,
            volume: 60,
            joins: false,
            own_mute: false,
            friend_connection: false,
            own_connection: false,
            shares: false,
        };
        settings.set_sound_cues(cues);
        let folder = Folder::new("sound-cues");
        settings.save(&folder.0).unwrap();
        let (saved, problems, _) = Settings::load(&folder.0);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(saved.sound_cues(), cues);
        let bytes = settings.merge(b"# My settings\r\nname = Mara\r\nfuture_key = stay\r\n");
        let (loaded, problems) = parse(&bytes);
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert_eq!(loaded.sound_cues(), cues);
        let text = String::from_utf8(bytes).unwrap();
        for line in [
            "sound_cues = off",
            "sound_cue_volume = 60",
            "sound_cue_joins = off",
            "sound_cue_own_mute = off",
            "sound_cue_friend_connection = off",
            "sound_cue_own_connection = off",
            "sound_cue_shares = off",
        ] {
            assert_eq!(text.matches(line).count(), 1, "{text}");
        }
        let (mut old, problems) = parse(b"# My settings\r\nname = Mara\r\nfuture_key = stay\r\n");
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert_eq!(old.sound_cues(), room::SoundCues::default());
        assert_eq!(
            old.merge(b"# My settings\r\nname = Mara\r\nfuture_key = stay\r\n"),
            b"# My settings\r\nname = Mara\r\nfuture_key = stay\r\n"
        );
        old.set_sound_cues(cues);
        let bytes = old.merge(b"# My settings\r\nname = Mara\r\nfuture_key = stay\r\n");
        assert!(bytes.starts_with(b"# My settings\r\nname = Mara\r\nfuture_key = stay\r\n"));
        old.set_sound_cues(room::SoundCues::default());
        assert!(
            !String::from_utf8(old.merge(&bytes))
                .unwrap()
                .contains("sound_cue")
        );
        for volume in ["5", "151", "loud", "+10", ""] {
            assert_eq!(
                settings
                    .set_from_file(SOUND_CUE_VOLUME, volume)
                    .unwrap_err(),
                "sound_cue_volume is a percent from 10 to 150; set it again in settings"
            );
        }
        for key in [
            SOUND_CUES,
            SOUND_CUE_JOINS,
            SOUND_CUE_OWN_MUTE,
            SOUND_CUE_FRIEND_CONNECTION,
            SOUND_CUE_OWN_CONNECTION,
            SOUND_CUE_SHARES,
        ] {
            assert_eq!(
                settings.set_from_file(key, "maybe").unwrap_err(),
                format!("{key} is on or off; set it again in settings")
            );
        }
        for (given, want) in [(0, 10), (10, 10), (100, 100), (150, 150), (200, 150)] {
            settings.set_sound_cues(room::SoundCues {
                volume: given,
                ..room::SoundCues::default()
            });
            assert_eq!(settings.sound_cues().volume, want);
        }
        for volume in ["10", "100", "150"] {
            settings.set_from_file(SOUND_CUE_VOLUME, volume).unwrap();
            assert_eq!(settings.sound_cues().volume.to_string(), volume);
        }
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
        let (loaded, problems, _) = Settings::load(&folder.0);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(loaded.hotkeys(), hotkeys);

        settings.set_hotkeys(Bindings::default());
        assert_eq!(settings, Settings::default());
        settings.save(&folder.0).unwrap();
        assert_eq!(fs::read_to_string(folder.file()).unwrap(), "");
    }

    // The panic key is saved and read back like any other, and only
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
        let (loaded, problems, _) = Settings::load(&folder.0);
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
        assert!(changed(&|next| next.set_share_key_opens_list(true)));
        let other = Choice::Device(String::from("other"));
        assert!(changed(&|next| next.set_device(Direction::Input, &other)));
        assert!(changed(&|next| next.set_device(Direction::Output, &other)));
        assert!(changed(&|next| next.set_talk_mode(TalkMode::OpenMic)));
        assert!(changed(
            &|next| next.set_sensitivity_db(*SENSITIVITY_DB.start())
        ));
        assert!(changed(&|next| next.set_constant_rate(false)));
        // The upload at its default, saved or not, is no change.
        assert!(!changed(&|next| next.set_upload_mbits(DEFAULT_UPLOAD_MBITS)));
        assert!(!changed(&|next| next.set_name("Tom")));
        assert!(!changed(&|next| next.set_port("41010").unwrap()));
        assert!(!changed(&|next| next.set_noise_suppression(true)));
        assert!(!changed(&|next| next.set_sound_cues(room::SoundCues {
            on: false,
            ..room::SoundCues::default()
        })));
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
        let (settings, problems) = settled(b"hotkey_mute = F9\nhotkey_deafen = Ctrl+Shift+M\n");
        assert_eq!(settings.hotkeys().chord(Action::Mute).to_string(), "F9");
        assert!(problems.is_empty(), "{problems:?}");

        let (settings, problems) = settled(b"hotkey_deafen = Ctrl+Shift+M\n");
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
        let (mut settings, problems, _) = Settings::load(&folder.0);
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
        let (settings, problems) = settled(
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

    // Editing by hand puts two moved actions on one key, and so can moving a key
    // in a run that could not read the file.
    #[test]
    fn two_moved_on_one_key() {
        let (settings, problems) = settled(
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
        settings.set_share_key_opens_list(true);
        settings.save(&folder.0).unwrap();
        assert_eq!(
            fs::read_to_string(folder.file()).unwrap(),
            "video_upload_mbits = 40\r\nvsync_in_viewer = on\r\nhide_strip_in_fullscreen = on\r\nshare_monitor = \\\\.\\DISPLAY2\r\nshare_key_opens_list = on\r\n"
        );
        let (loaded, problems, _) = Settings::load(&folder.0);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(loaded, settings);

        settings.set_upload_mbits(15);
        settings.set_vsync(false);
        settings.set_hide_strip(false);
        settings.set_share_monitor(None);
        settings.set_share_key_opens_list(false);
        assert_eq!(settings, Settings::default());
        settings.save(&folder.0).unwrap();
        assert_eq!(fs::read_to_string(folder.file()).unwrap(), "");
        // Written by hand, the defaults read back as the defaults.
        let (loaded, problems) =
            parse(b"video_upload_mbits = 15\nvsync_in_viewer = off\nhide_strip_in_fullscreen = off\nshare_monitor =\nshare_key_opens_list = off\n");
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
        let (mut settings, problems, _) = Settings::load(&folder.0);
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
        let (loaded, problems, _) = Settings::load(&folder.0);
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
    fn invite_link_settings_round_trip() {
        let mut settings = Settings::default();
        assert!(settings.invite_links());
        assert_eq!(settings.invite_link_wait_secs(), 1);
        assert!(settings.merge(b"").is_empty());
        assert_eq!(KEYS.len(), 35);
        settings.set_invite_links(false);
        settings.set_invite_link_wait_secs(3);
        let text = settings.merge(b"future_link_key = kept\r\n");
        assert_eq!(
            text,
            b"future_link_key = kept\r\ninvite_links = off\r\ninvite_link_wait_secs = 3\r\n"
        );
        let (loaded, problems) = parse(&text);
        assert_eq!(
            problems,
            [
                "settings: line 1: future_link_key is not a setting this version knows; kept as it is"
            ]
        );
        assert_eq!(loaded, settings);
        let (loaded, problems) = parse(b"invite_links = on\ninvite_link_wait_secs = 1\n");
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(loaded, Settings::default());
        assert!(loaded.merge(b"").is_empty());
    }

    #[test]
    fn invalid_invite_link_settings_keep_defaults() {
        for value in ["0", "6", "1e3", "maybe"] {
            let text = format!("invite_link_wait_secs = {value}\n");
            let (loaded, problems) = parse(text.as_bytes());
            assert_eq!(loaded, Settings::default());
            assert_eq!(
                problems,
                [
                    "settings: line 1 skipped: invite_link_wait_secs is a number of seconds from 1 to 5; set it again in settings"
                ]
            );
        }
        let (loaded, problems) = parse(b"invite_links = maybe\n");
        assert_eq!(loaded, Settings::default());
        assert_eq!(
            problems,
            ["settings: line 1 skipped: invite_links is on or off; set it again in settings"]
        );
    }

    #[test]
    fn unknown_sharing_values() {
        let text = format!(
            "video_upload_mbits = 81\nvideo_upload_mbits = fast\nvsync_in_viewer = yes\nhide_strip_in_fullscreen = 1\nshare_monitor = a\u{7}b\nshare_monitor = {}\nshare_key_opens_list = yes\n",
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
                "settings: line 7 skipped: share_key_opens_list is on or off; pick what to share again after Share",
            ]
        );
    }
}
