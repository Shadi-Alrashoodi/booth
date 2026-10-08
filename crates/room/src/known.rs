// What each side keeps between rooms, so a friend who joined once can come
// back without a new invite.
// A host keeps every device that joined it, with the per-peer secret it
// rejoins with, and the keys it refuses, in devices.bin. A client keeps the
// hosts it joined, with the same secret and every way it knows to reach
// them, in hosts.bin. Both files are written whole through
// keys::write_protected, which is DPAPI and atomic. In a room the saver
// thread writes them (saver.rs), never a thread that answers packets; the
// panel reads and changes them only while no room runs, and Turn keeps a
// room's late save from landing on top of what the panel changed.

mod format;
#[cfg(test)]
mod tests;

use std::fmt;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use invite::{Candidate, Invite, MAX_CANDIDATES, Mapping};
use keys::KeyError;
use zeroize::{Zeroize, Zeroizing};

use crate::log::{Log, log};

pub(crate) use format::{encode_devices, encode_hosts};
#[cfg(test)]
pub(crate) use format::{parse_devices, parse_hosts};

pub(crate) const MAX_DEVICES: usize = 256;
pub(crate) const MAX_BLOCKED: usize = 256;
pub(crate) const MAX_HOSTS: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum List {
    Hosts,
    Devices,
}

impl List {
    pub fn file_name(self) -> &'static str {
        match self {
            List::Hosts => "hosts.bin",
            List::Devices => "devices.bin",
        }
    }

    // What DPAPI stores beside the data. Windows shows it nowhere a user
    // looks, but it names the file for anyone who finds the blob.
    fn description(self) -> &'static str {
        match self {
            List::Hosts => "Booth known hosts",
            List::Devices => "Booth known devices",
        }
    }

    fn words(self) -> &'static str {
        match self {
            List::Hosts => "known hosts",
            List::Devices => "known devices",
        }
    }
}

// A device that joined this host. The panel shows it; only the room reads
// the secret.
#[derive(Clone)]
pub struct KnownDevice {
    pub key: [u8; 32],
    // The name it gave last, cleaned like every name in a room.
    pub name: String,
    // Unix seconds.
    pub first_seen: u64,
    pub last_seen: u64,
    pub(crate) secret: Zeroizing<[u8; 32]>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockedKey {
    pub key: [u8; 32],
    // Unix seconds.
    pub since: u64,
}

#[derive(Clone, Default)]
pub struct KnownDevices {
    pub devices: Vec<KnownDevice>,
    pub blocked: Vec<BlockedKey>,
}

// A host this PC joined, with the secret it rejoins with and every way to
// reach it that Booth knows. Read only outside the room, so nothing the
// panel does with one can hand a rejoin an address the rules refuse.
#[derive(Clone)]
pub struct KnownHost {
    pub(crate) host_key: [u8; 32],
    pub(crate) room_name: String,
    pub(crate) host_name: String,
    pub(crate) secret: Zeroizing<[u8; 32]>,
    // What the invite carried, then what the host said since
    // (control::Message::HostAddresses).
    pub(crate) candidates: Vec<Candidate>,
    pub(crate) address_name: Option<String>,
    // Where the last session with it was, which is where it is most likely
    // to be again.
    pub(crate) last_reached: Option<SocketAddr>,
    pub(crate) manual: Option<Manual>,
    // Unix seconds.
    pub(crate) last_seen: u64,
}

impl KnownHost {
    pub fn host_key(&self) -> &[u8; 32] {
        &self.host_key
    }

    pub fn room_name(&self) -> &str {
        &self.room_name
    }

    pub fn host_name(&self) -> &str {
        &self.host_name
    }

    pub fn manual(&self) -> Option<&Manual> {
        self.manual.as_ref()
    }

    pub fn last_seen(&self) -> u64 {
        self.last_seen
    }

    pub fn candidates(&self) -> &[Candidate] {
        &self.candidates
    }

    pub fn address_name(&self) -> Option<&str> {
        self.address_name.as_deref()
    }

    pub fn last_reached(&self) -> Option<SocketAddr> {
        self.last_reached
    }
}

// An address or a name the user typed for a known host whose stored ones
// went stale. Made only by parse, so it always passes the same rules an
// invite's address and name do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manual(pub(crate) Entry);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Entry {
    Addr(SocketAddr),
    Name(String),
}

impl Manual {
    // The panel's field: an address with its port, or a name. Nothing typed
    // is no entry. The error is why not, for the log; the panel has one
    // sentence for all of them.
    pub fn parse(text: &str) -> Result<Option<Manual>, &'static str> {
        let text = text.trim();
        if text.is_empty() {
            return Ok(None);
        }
        let entry = match text.parse::<SocketAddr>() {
            Ok(addr) => Entry::Addr(plain(addr)),
            Err(_) => Entry::Name(text.to_owned()),
        };
        let manual = Manual(entry);
        manual.check()?;
        Ok(Some(manual))
    }

    pub(crate) fn check(&self) -> Result<(), &'static str> {
        match &self.0 {
            Entry::Addr(addr) => invite::check_addr(*addr),
            Entry::Name(name) => invite::check_hostname(name)
                .map_err(|_| "it is neither an address with a port nor a name Booth can use"),
        }
    }
}

// As typed back into the field.
impl fmt::Display for Manual {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            Entry::Addr(addr) => addr.fmt(f),
            Entry::Name(name) => f.write_str(name),
        }
    }
}

impl fmt::Debug for KnownDevice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KnownDevice")
            .field("key", &keys::fingerprint(&self.key))
            .field("name", &self.name)
            .field("first_seen", &self.first_seen)
            .field("last_seen", &self.last_seen)
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for KnownHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KnownHost")
            .field("host_key", &keys::fingerprint(&self.host_key))
            .field("room_name", &self.room_name)
            .field("host_name", &self.host_name)
            .field("candidates", &self.candidates)
            .field("address_name", &self.address_name)
            .field("last_reached", &self.last_reached)
            .field("manual", &self.manual)
            .field("last_seen", &self.last_seen)
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for KnownDevices {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KnownDevices")
            .field("devices", &self.devices)
            .field("blocked", &self.blocked)
            .finish()
    }
}

#[derive(Debug)]
pub enum KnownError {
    // The file could not be read as the list. It is kept beside it as
    // `kept_as` and the list starts empty.
    Damaged {
        list: List,
        kept_as: PathBuf,
        why: String,
    },
    // The same, and it could not be put aside either. It stays where it is
    // and nothing is written over it.
    Stuck {
        list: List,
        path: PathBuf,
        why: String,
        source: io::Error,
    },
    // A newer Booth wrote it. Putting it aside would lose it for that
    // version too, since the empty list this one saved would read fine
    // there; it is left alone and nothing is written over it.
    Newer {
        list: List,
        path: PathBuf,
    },
    // Windows would not open it. It is left alone and nothing is written
    // over it until it can be read.
    Read {
        list: List,
        source: KeyError,
    },
    Write(KeyError),
}

// Words for the log, each with what to do. The panel has its own sentence
// for Damaged and turns the rest into sentences as they are.
impl fmt::Display for KnownError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KnownError::Damaged { list, kept_as, why } => write!(
                f,
                "the list of {} could not be read ({why}), so it starts empty; the old file is kept as {}",
                list.words(),
                kept_as.display()
            ),
            KnownError::Stuck {
                list,
                path,
                why,
                source,
            } => write!(
                f,
                "the list of {} in {} could not be read ({why}) and could not be put aside: {source}; it is left as it is and nothing is saved over it; move it out of that folder, then start Booth again",
                list.words(),
                path.display()
            ),
            KnownError::Newer { list, path } => write!(
                f,
                "the list of {} in {} was written by a newer version of Booth; it is left as it is and nothing is saved over it; run the newest version of Booth",
                list.words(),
                path.display()
            ),
            KnownError::Read { source, .. } => write!(
                f,
                "{source}; it is left as it is and nothing is saved over it; close any program that has it open, then start Booth again"
            ),
            KnownError::Write(err) => err.fmt(f),
        }
    }
}

impl std::error::Error for KnownError {}

// What the start screen says about a list a room or the panel could not
// use.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ListProblem {
    // Put aside, and the list started empty.
    Damaged(DamagedList),
    // Left where it is and not written to. `why` is the error in log words,
    // with what to do.
    Unusable { list: List, why: String },
}

// A list as read when a room opens or the panel asks.
pub(crate) struct Opened<T> {
    pub list: T,
    pub problem: Option<KnownError>,
    // False when a file is there that could not be read or put aside:
    // writing now would lose it.
    pub writable: bool,
}

impl<T: Default> Opened<T> {
    fn empty(problem: Option<KnownError>, writable: bool) -> Opened<T> {
        Opened {
            list: T::default(),
            problem,
            writable,
        }
    }
}

fn open<T: Default>(
    dir: &Path,
    list: List,
    parse: fn(&[u8]) -> Result<T, format::Malformed>,
) -> Opened<T> {
    let path = dir.join(list.file_name());
    let why = match keys::read_protected(&path) {
        Ok(plain) => match parse(&plain) {
            Ok(found) => {
                return Opened {
                    list: found,
                    problem: None,
                    writable: true,
                };
            }
            Err(format::Malformed::Version(version)) if version > format::VERSION => {
                return Opened::empty(Some(KnownError::Newer { list, path }), false);
            }
            Err(bad) => bad.to_string(),
        },
        Err(KeyError::Read { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
            return Opened::empty(None, true);
        }
        Err(source @ KeyError::Read { .. }) => {
            return Opened::empty(Some(KnownError::Read { list, source }), false);
        }
        // The file around the list is in a format this Booth does not know,
        // which no Booth has written but a newer one.
        Err(KeyError::UnknownVersion { .. }) => {
            return Opened::empty(Some(KnownError::Newer { list, path }), false);
        }
        // Short, since the line around it names the file and says what to
        // do.
        Err(KeyError::NotAKeyFile { .. }) => {
            String::from("it is not a file Booth wrote, or it is damaged")
        }
        Err(KeyError::Decrypt { source, .. }) => format!(
            "Windows could not decrypt it, so another Windows user or another PC made it, or it is damaged: {source}"
        ),
        Err(err) => err.to_string(),
    };
    match keys::put_aside(&path) {
        Ok(kept_as) => Opened::empty(Some(KnownError::Damaged { list, kept_as, why }), true),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Opened::empty(None, true),
        Err(source) => Opened::empty(
            Some(KnownError::Stuck {
                list,
                path,
                why,
                source,
            }),
            false,
        ),
    }
}

pub(crate) fn open_devices(dir: &Path) -> Opened<KnownDevices> {
    open(dir, List::Devices, format::parse_devices)
}

pub(crate) fn open_hosts(dir: &Path) -> Opened<Vec<KnownHost>> {
    open(dir, List::Hosts, format::parse_hosts)
}

pub(crate) fn save(dir: &Path, list: List, bytes: &[u8]) -> Result<(), KnownError> {
    keys::write_protected(&dir.join(list.file_name()), bytes, list.description())
        .map_err(KnownError::Write)
}

// A room writes its list back whole from memory, and its last save can
// still be on its way after leave returned (saver.rs, FINISH_WAIT). A
// Remove or Forget pressed in that gap would be undone by it. So each list
// file in this process has a turn number: a room takes the next one when it
// reads the list, the panel moves it on when it writes the list, and a save
// from a room whose turn has passed is left out. One lock covers a room's
// save and the panel's read, change and write, so neither lands in the
// middle of the other.
static TURNS: Mutex<Vec<(PathBuf, Arc<Mutex<u64>>)>> = Mutex::new(Vec::new());

fn turn_for(path: &Path) -> Arc<Mutex<u64>> {
    let mut turns = lock(&TURNS);
    if let Some((_, turn)) = turns.iter().find(|(known, _)| known == path) {
        return Arc::clone(turn);
    }
    let turn = Arc::new(Mutex::new(0));
    turns.push((path.to_path_buf(), Arc::clone(&turn)));
    turn
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

// A room's right to write its list back.
pub(crate) struct Turn {
    dir: PathBuf,
    list: List,
    current: Arc<Mutex<u64>>,
    mine: u64,
}

pub(crate) enum Saved {
    Written,
    // The panel, or a room opened since, has the list now.
    Passed,
}

impl Turn {
    pub(crate) fn list(&self) -> List {
        self.list
    }

    pub(crate) fn save(&self, bytes: &[u8]) -> Result<Saved, KnownError> {
        let current = lock(&self.current);
        if *current != self.mine {
            return Ok(Saved::Passed);
        }
        save(&self.dir, self.list, bytes)?;
        Ok(Saved::Written)
    }
}

fn open_for_room<T: Default>(
    dir: &Path,
    list: List,
    parse: fn(&[u8]) -> Result<T, format::Malformed>,
) -> (Opened<T>, Turn) {
    let current = turn_for(&dir.join(list.file_name()));
    let (opened, mine) = {
        let mut turn = lock(&current);
        *turn += 1;
        (open(dir, list, parse), *turn)
    };
    let turn = Turn {
        dir: dir.to_path_buf(),
        list,
        current,
        mine,
    };
    (opened, turn)
}

// The list a host's room starts with, and its turn to write it back.
pub(crate) fn room_devices(dir: &Path) -> (Opened<KnownDevices>, Turn) {
    open_for_room(dir, List::Devices, format::parse_devices)
}

pub(crate) fn room_hosts(dir: &Path) -> (Opened<Vec<KnownHost>>, Turn) {
    open_for_room(dir, List::Hosts, format::parse_hosts)
}

// The panel's read, change and write. `change` says whether it changed
// anything.
fn edit<T: Default>(
    dir: &Path,
    list: List,
    parse: fn(&[u8]) -> Result<T, format::Malformed>,
    encode: fn(&T) -> Zeroizing<Vec<u8>>,
    change: impl FnOnce(&mut T) -> bool,
) -> Result<(), KnownError> {
    let current = turn_for(&dir.join(list.file_name()));
    let mut turn = lock(&current);
    let opened = open(dir, list, parse);
    if let Some(problem) = opened.problem {
        return Err(problem);
    }
    let mut found = opened.list;
    if change(&mut found) {
        save(dir, list, &encode(&found))?;
        *turn += 1;
    }
    Ok(())
}

// The one line a room writes about its list when it opens.
pub(crate) fn note_opened<T>(log: &Log, list: List, opened: &Opened<T>, count: usize) {
    match &opened.problem {
        Some(problem) => log!(log, "{problem}"),
        None => log!(log, "{} from {}: {count}", list.words(), list.file_name()),
    }
}

// Newest first, the order the panel shows them in.
pub(crate) fn hosts(dir: &Path) -> Result<Vec<KnownHost>, KnownError> {
    let opened = open_hosts(dir);
    if let Some(problem) = opened.problem {
        return Err(problem);
    }
    let mut hosts = opened.list;
    // Unstable: the stable sort copies every record, secret and all, into a
    // buffer it frees without wiping.
    hosts.sort_unstable_by_key(|host| std::cmp::Reverse(host.last_seen));
    Ok(hosts)
}

pub(crate) fn forget_host(dir: &Path, host_key: &[u8; 32]) -> Result<(), KnownError> {
    edit_hosts(dir, |hosts| {
        let before = hosts.len();
        hosts.retain(|host| host.host_key != *host_key);
        // retain leaves a copy of the last record past the end.
        hosts.spare_capacity_mut().zeroize();
        hosts.len() != before
    })
}

pub(crate) fn set_manual(
    dir: &Path,
    host_key: &[u8; 32],
    manual: Option<Manual>,
) -> Result<(), KnownError> {
    edit_hosts(dir, |hosts| {
        match hosts.iter_mut().find(|host| host.host_key == *host_key) {
            Some(host) if host.manual != manual => {
                host.manual = manual;
                true
            }
            _ => false,
        }
    })
}

fn edit_hosts(
    dir: &Path,
    change: impl FnOnce(&mut Vec<KnownHost>) -> bool,
) -> Result<(), KnownError> {
    edit(
        dir,
        List::Hosts,
        format::parse_hosts,
        |hosts| encode_hosts(hosts),
        change,
    )
}

// Newest first, as the settings screen shows them.
pub(crate) fn devices(dir: &Path) -> Result<KnownDevices, KnownError> {
    let opened = open_devices(dir);
    if let Some(problem) = opened.problem {
        return Err(problem);
    }
    let mut list = opened.list;
    list.devices
        .sort_unstable_by_key(|device| std::cmp::Reverse(device.last_seen));
    list.blocked
        .sort_by_key(|blocked| std::cmp::Reverse(blocked.since));
    Ok(list)
}

// The device and its secret go at once; it needs a new invite to come back.
pub(crate) fn remove_device(dir: &Path, key: &[u8; 32]) -> Result<(), KnownError> {
    edit_devices(dir, |list| {
        let before = list.devices.len();
        list.devices.retain(|device| device.key != *key);
        list.devices.spare_capacity_mut().zeroize();
        list.devices.len() != before
    })
}

pub(crate) fn unblock(dir: &Path, key: &[u8; 32]) -> Result<(), KnownError> {
    edit_devices(dir, |list| {
        let before = list.blocked.len();
        list.blocked.retain(|blocked| blocked.key != *key);
        list.blocked.len() != before
    })
}

fn edit_devices(
    dir: &Path,
    change: impl FnOnce(&mut KnownDevices) -> bool,
) -> Result<(), KnownError> {
    edit(
        dir,
        List::Devices,
        format::parse_devices,
        encode_devices,
        change,
    )
}

// The invite's own rule for each kind of address, applied through Invite so
// there is one copy of it: a list that came from a host or from disk holds
// nothing an invite could not.
pub(crate) fn usable(candidate: &Candidate) -> bool {
    let probe = Invite {
        host_key: [0; 32],
        invite_id: [0; 8],
        secret: [0; 16],
        multi_use: false,
        expires_at: 0,
        candidates: vec![*candidate],
        mapping: Mapping::Unknown,
        mapped: false,
        mapped_verified: false,
        second_router: false,
        hostname: None,
    };
    probe.check().is_ok()
}

// What a list keeps of some candidates: no scope ids, only the usable ones,
// each once, as many as an invite holds.
pub(crate) fn clean_candidates(candidates: &[Candidate]) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = Vec::with_capacity(candidates.len().min(MAX_CANDIDATES));
    for candidate in candidates {
        let candidate = Candidate {
            kind: candidate.kind,
            addr: plain(candidate.addr),
        };
        if out.len() < MAX_CANDIDATES && usable(&candidate) && !out.contains(&candidate) {
            out.push(candidate);
        }
    }
    out
}

// Where a client reached a host: anything it can send to. Loopback counts,
// since two copies of Booth on one PC reach each other there.
pub(crate) fn reachable(addr: SocketAddr) -> bool {
    let ip = addr.ip();
    let broadcast = matches!(addr, SocketAddr::V4(v4) if v4.ip().is_broadcast());
    addr.port() != 0 && !ip.is_unspecified() && !ip.is_multicast() && !broadcast
}

// A scope id or flow label means nothing once written down.
pub(crate) fn plain(addr: SocketAddr) -> SocketAddr {
    match addr {
        SocketAddr::V6(v6) => SocketAddr::new((*v6.ip()).into(), v6.port()),
        v4 => v4,
    }
}

// The host's list while its room runs: read when it opens, changed as
// friends come and go, and handed to the saver after it changed (Unsaved).
pub(crate) struct DeviceBook {
    list: KnownDevices,
    writable: bool,
    unsaved: Unsaved,
}

// What a confirmed join did to the list, for the log.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Joined {
    Added,
    // Added, and the device seen longest ago that is not in the room made
    // way for it.
    AddedInPlaceOf([u8; 32]),
    Again,
    // The list is full of people in the room, or it cannot be written.
    NotKept,
}

// A room's list is encoded under the room's state lock, where every friend's
// packet waits behind it, and a friend can change it with every packet (a
// new name in each Hello). So it goes to the saver at most once per
// SAVE_GAP, and a change in between waits for the timer thread (save_due).
pub(crate) const SAVE_GAP: Duration = Duration::from_secs(1);

#[derive(Default)]
struct Unsaved {
    changed: bool,
    // Not handed over before this.
    next: Option<Instant>,
}

impl Unsaved {
    fn take(&mut self, writable: bool, now: Instant) -> bool {
        if !self.changed || !writable || self.next.is_some_and(|next| now < next) {
            return false;
        }
        self.changed = false;
        self.next = Some(now + SAVE_GAP);
        true
    }

    // When the room closes: whenever the one before went.
    fn take_last(&mut self, writable: bool) -> bool {
        writable && std::mem::take(&mut self.changed)
    }

    fn due(&self, writable: bool) -> Option<Instant> {
        self.next.filter(|_| self.changed && writable)
    }
}

// Room for a whole list from the start. A Vec that grows moves what it holds
// and frees the old buffer, secrets and all, without wiping it; copying and
// dropping the old one here wipes each secret where it was.
fn with_room<T: Clone>(items: Vec<T>, cap: usize) -> Vec<T> {
    if items.capacity() >= cap {
        return items;
    }
    let mut out = Vec::with_capacity(cap);
    out.extend(items.iter().cloned());
    out
}

impl DeviceBook {
    pub(crate) fn new(mut list: KnownDevices, writable: bool) -> DeviceBook {
        list.devices = with_room(list.devices, MAX_DEVICES);
        DeviceBook {
            list,
            writable,
            unsaved: Unsaved::default(),
        }
    }

    pub(crate) fn devices(&self) -> &[KnownDevice] {
        &self.list.devices
    }

    pub(crate) fn is_known(&self, key: &[u8; 32]) -> bool {
        self.list.devices.iter().any(|device| device.key == *key)
    }

    pub(crate) fn is_blocked(&self, key: &[u8; 32]) -> bool {
        self.list.blocked.iter().any(|blocked| blocked.key == *key)
    }

    // `in_room` keeps the people here from making way.
    pub(crate) fn joined(
        &mut self,
        key: [u8; 32],
        secret: &Zeroizing<[u8; 32]>,
        now_unix: u64,
        in_room: impl Fn(&[u8; 32]) -> bool,
    ) -> Joined {
        if let Some(device) = self.list.devices.iter_mut().find(|d| d.key == key) {
            device.last_seen = now_unix;
            device.secret = secret.clone();
            self.unsaved.changed = true;
            return Joined::Again;
        }
        if !self.writable || self.is_blocked(&key) {
            return Joined::NotKept;
        }
        let mut made_way = None;
        if self.list.devices.len() >= MAX_DEVICES {
            let oldest = self
                .list
                .devices
                .iter()
                .enumerate()
                .filter(|(_, device)| !in_room(&device.key))
                .min_by_key(|(_, device)| device.last_seen)
                .map(|(i, _)| i);
            let Some(oldest) = oldest else {
                return Joined::NotKept;
            };
            made_way = Some(self.list.devices.remove(oldest).key);
        }
        self.list.devices.push(KnownDevice {
            key,
            name: String::from(crate::control::PERSON_FALLBACK),
            first_seen: now_unix,
            last_seen: now_unix,
            secret: secret.clone(),
        });
        self.unsaved.changed = true;
        match made_way {
            Some(gone) => Joined::AddedInPlaceOf(gone),
            None => Joined::Added,
        }
    }

    pub(crate) fn named(&mut self, key: &[u8; 32], name: &str) {
        if let Some(device) = self.list.devices.iter_mut().find(|d| d.key == *key)
            && device.name != name
        {
            device.name = name.to_owned();
            self.unsaved.changed = true;
        }
    }

    pub(crate) fn seen(&mut self, key: &[u8; 32], now_unix: u64) {
        if let Some(device) = self.list.devices.iter_mut().find(|d| d.key == *key)
            && device.last_seen != now_unix
        {
            device.last_seen = now_unix;
            self.unsaved.changed = true;
        }
    }

    pub(crate) fn take_save(&mut self, now: Instant) -> Option<Save> {
        self.unsaved.take(self.writable, now).then(|| self.encode())
    }

    pub(crate) fn take_last_save(&mut self) -> Option<Save> {
        self.unsaved.take_last(self.writable).then(|| self.encode())
    }

    pub(crate) fn save_due(&self) -> Option<Instant> {
        self.unsaved.due(self.writable)
    }

    fn encode(&self) -> Save {
        Save {
            bytes: encode_devices(&self.list),
        }
    }
}

// A client's list while its room runs: every host it knows, so the one it
// is with can be written back among them.
pub(crate) struct HostBook {
    hosts: Vec<KnownHost>,
    writable: bool,
    unsaved: Unsaved,
}

impl HostBook {
    pub(crate) fn new(hosts: Vec<KnownHost>, writable: bool) -> HostBook {
        HostBook {
            hosts: with_room(hosts, MAX_HOSTS),
            writable,
            unsaved: Unsaved::default(),
        }
    }

    pub(crate) fn get(&self, key: &[u8; 32]) -> Option<&KnownHost> {
        self.hosts.iter().find(|host| host.host_key == *key)
    }

    // `change` says whether it changed anything.
    pub(crate) fn update(&mut self, key: &[u8; 32], change: impl FnOnce(&mut KnownHost) -> bool) {
        if let Some(host) = self.hosts.iter_mut().find(|host| host.host_key == *key)
            && change(host)
        {
            self.unsaved.changed = true;
        }
    }

    // Returns the host that made way for it, when the list was full: the
    // one seen longest ago.
    pub(crate) fn add(&mut self, host: KnownHost) -> Option<KnownHost> {
        self.hosts.retain(|known| known.host_key != host.host_key);
        let mut made_way = None;
        if self.hosts.len() >= MAX_HOSTS
            && let Some(oldest) = (0..self.hosts.len()).min_by_key(|&i| self.hosts[i].last_seen)
        {
            made_way = Some(self.hosts.remove(oldest));
        }
        self.hosts.push(host);
        self.unsaved.changed = true;
        made_way
    }

    pub(crate) fn take_save(&mut self, now: Instant) -> Option<Save> {
        self.unsaved.take(self.writable, now).then(|| self.encode())
    }

    pub(crate) fn take_last_save(&mut self) -> Option<Save> {
        self.unsaved.take_last(self.writable).then(|| self.encode())
    }

    pub(crate) fn save_due(&self) -> Option<Instant> {
        self.unsaved.due(self.writable)
    }

    fn encode(&self) -> Save {
        Save {
            bytes: encode_hosts(&self.hosts),
        }
    }
}

// A list as it is now, for the saver, whose Turn says which list it is.
pub(crate) struct Save {
    pub bytes: Zeroizing<Vec<u8>>,
}

// A list that could not be read and was put aside, for the panel's
// sentence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DamagedList {
    pub list: List,
    // Only the file name, "hosts.bin.bad": the folder is Booth's own.
    pub kept_as: String,
}

impl KnownError {
    // None for a write that failed: that is said where it was asked for.
    pub fn problem(&self) -> Option<ListProblem> {
        match self {
            KnownError::Damaged { .. } => self.damaged().map(ListProblem::Damaged),
            KnownError::Stuck { list, .. }
            | KnownError::Newer { list, .. }
            | KnownError::Read { list, .. } => Some(ListProblem::Unusable {
                list: *list,
                why: self.to_string(),
            }),
            KnownError::Write(_) => None,
        }
    }

    pub fn damaged(&self) -> Option<DamagedList> {
        match self {
            KnownError::Damaged { list, kept_as, .. } => Some(DamagedList {
                list: *list,
                kept_as: kept_as
                    .file_name()
                    .map_or_else(String::new, |name| name.to_string_lossy().into_owned()),
            }),
            _ => None,
        }
    }
}
