// What is inside devices.bin and hosts.bin once DPAPI has opened them. Only
// this PC's own user can write these files, but a broken disk, a copy from
// another Booth or a hand edit can put anything there, so the parser is as
// strict as the one for packets: every length checked, every count within
// its cap, nothing left over, and nothing the writer would not have written.
// Whatever encode writes, parse takes back byte for byte; a list that saved
// fine and then read as damaged would cost every friend on it a new invite.
//
// devices.bin, version 1, numbers little endian:
//   "BDEV" version
//   count u16, then per device: key 32, secret 32, first seen u64,
//     last seen u64, name (length u8, UTF-8)
//   count u16, then per blocked key: key 32, since u64
//
// hosts.bin, version 1:
//   "BHST" version
//   count u8, then per host: key 32, secret 32, last seen u64,
//     room name (length u8, UTF-8), host name (length u8, UTF-8),
//     candidate count u8 and each candidate (kind u8, address),
//     address name (length u8, 0 for none), last reached (address or 0),
//     manual (0 none, 1 and an address, 2 and a name)
//   An address is a family byte, 4 or 6, then the IP and a u16 port.

use std::fmt;

use invite::MAX_CANDIDATES;
use zeroize::Zeroizing;

use super::{
    BlockedKey, Entry, KnownDevice, KnownDevices, KnownHost, MAX_BLOCKED, MAX_DEVICES, MAX_HOSTS,
    Manual, reachable,
};
use crate::control::{
    self, CANDIDATE_MAX, MAX_HOSTNAME, MAX_NAME_BYTES, PERSON_FALLBACK, ROOM_FALLBACK, Reader,
};
use crate::known;

const DEVICES_TAG: &[u8; 4] = b"BDEV";
const HOSTS_TAG: &[u8; 4] = b"BHST";
pub(crate) const VERSION: u8 = 1;

const NO_MANUAL: u8 = 0;
const MANUAL_ADDR: u8 = 1;
const MANUAL_NAME: u8 = 2;

// Worst cases, for the one allocation each encode makes.
const DEVICE_MAX: usize = 32 + 32 + 8 + 8 + 1 + MAX_NAME_BYTES;
const BLOCKED_LEN: usize = 32 + 8;
const HOST_MAX: usize = 32
    + 32
    + 8
    + 2 * (1 + MAX_NAME_BYTES)
    + 1
    + MAX_CANDIDATES * CANDIDATE_MAX
    + 1
    + MAX_HOSTNAME
    + 19
    + 1
    + 1
    + MAX_HOSTNAME;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Malformed {
    Version(u8),
    Shape(&'static str),
}

// Words for the log, after "could not be read: ".
impl fmt::Display for Malformed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Malformed::Version(version) => write!(
                f,
                "it uses list format {version}, which this version of Booth does not know"
            ),
            Malformed::Shape(why) => f.write_str(why),
        }
    }
}

fn shape(why: &'static str) -> Malformed {
    Malformed::Shape(why)
}

const CUT_SHORT: &str = "it is cut short";

pub(crate) fn encode_devices(list: &KnownDevices) -> Zeroizing<Vec<u8>> {
    let devices = kept_devices(list);
    let blocked = kept_blocked(list);
    let mut out = Zeroizing::new(Vec::with_capacity(
        5 + 2 + devices.len() * DEVICE_MAX + 2 + blocked.len() * BLOCKED_LEN,
    ));
    out.extend_from_slice(DEVICES_TAG);
    out.push(VERSION);
    out.extend_from_slice(&(devices.len() as u16).to_le_bytes());
    for device in devices {
        out.extend_from_slice(&device.key);
        out.extend_from_slice(device.secret.as_slice());
        out.extend_from_slice(&device.first_seen.to_le_bytes());
        out.extend_from_slice(&device.last_seen.to_le_bytes());
        control::put_text(&mut out, &control::clean(&device.name, PERSON_FALLBACK));
    }
    out.extend_from_slice(&(blocked.len() as u16).to_le_bytes());
    for blocked in blocked {
        out.extend_from_slice(&blocked.key);
        out.extend_from_slice(&blocked.since.to_le_bytes());
    }
    out
}

// The first of each key, none that is also blocked, at most the cap: the
// same rules parse holds a file to.
fn kept_devices(list: &KnownDevices) -> Vec<&KnownDevice> {
    let mut kept: Vec<&KnownDevice> = Vec::with_capacity(list.devices.len().min(MAX_DEVICES));
    for device in &list.devices {
        let blocked = list.blocked.iter().any(|b| b.key == device.key);
        if kept.len() < MAX_DEVICES && !blocked && !kept.iter().any(|k| k.key == device.key) {
            kept.push(device);
        }
    }
    kept
}

fn kept_blocked(list: &KnownDevices) -> Vec<&BlockedKey> {
    let mut kept: Vec<&BlockedKey> = Vec::with_capacity(list.blocked.len().min(MAX_BLOCKED));
    for blocked in &list.blocked {
        if kept.len() < MAX_BLOCKED && !kept.iter().any(|k| k.key == blocked.key) {
            kept.push(blocked);
        }
    }
    kept
}

pub(crate) fn parse_devices(bytes: &[u8]) -> Result<KnownDevices, Malformed> {
    let mut r = Reader(bytes);
    header(&mut r, DEVICES_TAG, "it is not a list of known devices")?;
    let count = u16_count(
        &mut r,
        MAX_DEVICES,
        "it lists more devices than Booth keeps",
    )?;
    // Sized once: a list that grows frees its old buffer, secrets and all,
    // without wiping it.
    let mut list = KnownDevices {
        devices: Vec::with_capacity(count),
        blocked: Vec::new(),
    };
    for _ in 0..count {
        let key = r.array::<32>().ok_or(shape(CUT_SHORT))?;
        let secret = Zeroizing::new(r.array::<32>().ok_or(shape(CUT_SHORT))?);
        let first_seen = u64_le(&mut r)?;
        let last_seen = u64_le(&mut r)?;
        let name = clean_text(&mut r)?;
        if list.devices.iter().any(|d| d.key == key) {
            return Err(shape("a device is in it twice"));
        }
        list.devices.push(KnownDevice {
            key,
            name,
            first_seen,
            last_seen,
            secret,
        });
    }
    let count = u16_count(
        &mut r,
        MAX_BLOCKED,
        "it lists more blocked keys than Booth keeps",
    )?;
    list.blocked.reserve_exact(count);
    for _ in 0..count {
        let key = r.array::<32>().ok_or(shape(CUT_SHORT))?;
        let since = u64_le(&mut r)?;
        if list.blocked.iter().any(|b| b.key == key) {
            return Err(shape("a blocked key is in it twice"));
        }
        if list.devices.iter().any(|d| d.key == key) {
            return Err(shape("a key is in it both as a device and as blocked"));
        }
        list.blocked.push(BlockedKey { key, since });
    }
    end(&r)?;
    Ok(list)
}

pub(crate) fn encode_hosts(hosts: &[KnownHost]) -> Zeroizing<Vec<u8>> {
    let mut kept: Vec<&KnownHost> = Vec::with_capacity(hosts.len().min(MAX_HOSTS));
    for host in hosts {
        if kept.len() < MAX_HOSTS && !kept.iter().any(|k| k.host_key == host.host_key) {
            kept.push(host);
        }
    }
    let mut out = Zeroizing::new(Vec::with_capacity(5 + 1 + kept.len() * HOST_MAX));
    out.extend_from_slice(HOSTS_TAG);
    out.push(VERSION);
    out.push(kept.len() as u8);
    for host in kept {
        out.extend_from_slice(&host.host_key);
        out.extend_from_slice(host.secret.as_slice());
        out.extend_from_slice(&host.last_seen.to_le_bytes());
        control::put_text(&mut out, &control::clean(&host.room_name, ROOM_FALLBACK));
        control::put_text(&mut out, &control::clean(&host.host_name, PERSON_FALLBACK));
        let candidates = known::clean_candidates(&host.candidates);
        out.push(candidates.len() as u8);
        for candidate in &candidates {
            control::put_candidate(&mut out, candidate);
        }
        control::put_hostname(&mut out, host.address_name.as_deref());
        control::put_addr(&mut out, host.last_reached.filter(|addr| reachable(*addr)));
        match host.manual.as_ref().filter(|manual| manual.check().is_ok()) {
            None => out.push(NO_MANUAL),
            Some(Manual(Entry::Addr(addr))) => {
                out.push(MANUAL_ADDR);
                control::put_addr(&mut out, Some(*addr));
            }
            Some(Manual(Entry::Name(name))) => {
                out.push(MANUAL_NAME);
                control::put_hostname(&mut out, Some(name));
            }
        }
    }
    out
}

pub(crate) fn parse_hosts(bytes: &[u8]) -> Result<Vec<KnownHost>, Malformed> {
    let mut r = Reader(bytes);
    header(&mut r, HOSTS_TAG, "it is not a list of known hosts")?;
    let count = usize::from(r.u8().ok_or(shape(CUT_SHORT))?);
    if count > MAX_HOSTS {
        return Err(shape("it lists more hosts than Booth keeps"));
    }
    let mut hosts: Vec<KnownHost> = Vec::with_capacity(count);
    for _ in 0..count {
        let host = host(&mut r)?;
        if hosts.iter().any(|h| h.host_key == host.host_key) {
            return Err(shape("a host is in it twice"));
        }
        hosts.push(host);
    }
    end(&r)?;
    Ok(hosts)
}

fn host(r: &mut Reader) -> Result<KnownHost, Malformed> {
    let host_key = r.array::<32>().ok_or(shape(CUT_SHORT))?;
    let secret = Zeroizing::new(r.array::<32>().ok_or(shape(CUT_SHORT))?);
    let last_seen = u64_le(r)?;
    let room_name = clean_text(r)?;
    let host_name = clean_text(r)?;
    let count = usize::from(r.u8().ok_or(shape(CUT_SHORT))?);
    if count > MAX_CANDIDATES {
        return Err(shape(
            "a host in it has more addresses than an invite holds",
        ));
    }
    let mut candidates = Vec::with_capacity(count);
    for _ in 0..count {
        let candidate = r
            .candidate()
            .ok_or(shape("an address in it is not one an invite could carry"))?;
        if candidates.contains(&candidate) {
            return Err(shape("an address is in it twice for one host"));
        }
        candidates.push(candidate);
    }
    let address_name = r
        .hostname()
        .ok_or(shape("an address name in it is not one Booth can use"))?;
    let last_reached = r
        .addr()
        .ok_or(shape("the last address a host was reached at is damaged"))?;
    if last_reached.is_some_and(|addr| !reachable(addr)) {
        return Err(shape(
            "the last address a host was reached at is not one Booth sends to",
        ));
    }
    let manual = match r.u8().ok_or(shape(CUT_SHORT))? {
        NO_MANUAL => None,
        MANUAL_ADDR => {
            Some(Manual(Entry::Addr(r.addr().flatten().ok_or(shape(
                "an address typed in for a host is damaged",
            ))?)))
        }
        MANUAL_NAME => Some(Manual(Entry::Name(
            r.hostname()
                .flatten()
                .ok_or(shape("a name typed in for a host is damaged"))?,
        ))),
        _ => return Err(shape("an address typed in for a host is damaged")),
    };
    if manual
        .as_ref()
        .is_some_and(|manual| manual.check().is_err())
    {
        return Err(shape(
            "an address typed in for a host is not one Booth can use",
        ));
    }
    Ok(KnownHost {
        host_key,
        room_name,
        host_name,
        secret,
        candidates,
        address_name,
        last_reached,
        manual,
        last_seen,
    })
}

fn header(r: &mut Reader, tag: &[u8; 4], not_ours: &'static str) -> Result<(), Malformed> {
    if r.bytes(4) != Some(tag.as_slice()) {
        return Err(shape(not_ours));
    }
    match r.u8() {
        Some(VERSION) => Ok(()),
        Some(version) => Err(Malformed::Version(version)),
        None => Err(shape(CUT_SHORT)),
    }
}

fn u16_count(r: &mut Reader, cap: usize, too_many: &'static str) -> Result<usize, Malformed> {
    let count = usize::from(u16::from_le_bytes(r.array().ok_or(shape(CUT_SHORT))?));
    if count > cap {
        return Err(shape(too_many));
    }
    Ok(count)
}

fn u64_le(r: &mut Reader) -> Result<u64, Malformed> {
    r.array().map(u64::from_le_bytes).ok_or(shape(CUT_SHORT))
}

// Names are written cleaned, so one that cleaning would change was not. One
// written before cleaning cut stacked marks loads with them cut.
fn clean_text(r: &mut Reader) -> Result<String, Malformed> {
    let text = control::drop_stacked_marks(r.text().ok_or(shape(CUT_SHORT))?);
    if text.is_empty() || control::clean(&text, "") != text {
        return Err(shape("a name in it is not one Booth would have written"));
    }
    Ok(text)
}

fn end(r: &Reader) -> Result<(), Malformed> {
    if r.0.is_empty() {
        Ok(())
    } else {
        Err(shape("it has bytes after the last record"))
    }
}
