// Control messages ride the reliable channel. Every byte here came from a peer,
// so decode checks every length and returns None rather than guessing.

use std::fmt;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use channels::reliable::MAX_MESSAGE;
use invite::{Candidate, CandidateKind, MAX_CANDIDATES, PROTOCOL, Version};
use zeroize::Zeroizing;

use crate::known;
use crate::remote::ControlEnd;
use crate::screen::wire::ShapeChunk;

pub(crate) const MAX_NAME_CHARS: usize = 32;
// A 32-character name can be 128 bytes of UTF-8, and nine of those do not fit
// one message. 64 bytes keeps 32 characters of any Latin script.
pub(crate) const MAX_NAME_BYTES: usize = 64;
pub(crate) const PERSON_FALLBACK: &str = "Friend";
pub(crate) const ROOM_FALLBACK: &str = "Room";
// More combining marks in a row than any script puts on one letter.
const MAX_MARKS: usize = 4;

// Eight people in a room, the host included. More has never been tested.
pub(crate) const MAX_CLIENTS: usize = 7;
pub(crate) const MAX_ROSTER: usize = MAX_CLIENTS + 1;

// What test builds from before version numbers open a link with. Never
// used for anything else, so they can always be named.
const UNVERSIONED_HELLO: u8 = 1;
const PEER_SECRET: u8 = 2;
const ROSTER: u8 = 3;
const BYE: u8 = 4;
const HOST_ADDRESSES: u8 = 5;
const VOICE_LOSS: u8 = 6;
const WORST_LOSS: u8 = 7;
const PERIODS: u8 = 8;
const SHARE_START: u8 = 9;
const SHARE_ANSWER: u8 = 10;
const SHARE_STOP: u8 = 11;
const WATCH: u8 = 12;
const RECOVER: u8 = 13;
const IDR: u8 = 14;
const VIDEO_LOSS: u8 = 15;
const SHARE_FACTS: u8 = 16;
const SHARER_CLOCK: u8 = 17;
const SHAPE: u8 = 18;
const CONTROL_ASK: u8 = 19;
const CONTROL_ASKED: u8 = 20;
const CONTROL_ANSWER: u8 = 21;
const CONTROL_END: u8 = 22;
const CONTROL_PAUSED: u8 = 23;
// The Hello from 0.1.0 on. This number and the Hello's fields up to the
// name stay as they are in every later protocol.
const HELLO: u8 = 24;

const IS_HOST: u8 = 1 << 0;
const JOINED_BY_INVITE: u8 = 1 << 1;
const RECONNECTING: u8 = 1 << 2;
const SHARING: u8 = 1 << 3;
const CONTROLLING: u8 = 1 << 4;
const KNOWN_FLAGS: u8 = IS_HOST | JOINED_BY_INVITE | RECONNECTING | SHARING | CONTROLLING;
const NO_RTT: u16 = u16::MAX;

// ShareAnswer's first byte.
const GRANTED: u8 = 0;
const BUSY: u8 = 1;
const TOO_SOON: u8 = 2;

// ControlAnswer's first byte.
const ALLOW: u8 = 0;
const DONT_ALLOW: u8 = 1;
const CONTROL_BUSY: u8 = 2;
const CONTROL_TOO_SOON: u8 = 3;

// ControlEnd's reason byte.
const RELEASED: u8 = 0;
const STOPPED: u8 = 1;
const PANIC: u8 = 2;
const ENDED_BY_HOST: u8 = 3;
const SHARE_ENDED: u8 = 4;
const SESSION_LOST: u8 = 5;
const CLOSED: u8 = 6;

// ShareFacts' flags: every link the share goes over is on the LAN, every
// watcher decodes HEVC, and a watcher counted in them asks for an IDR.
const ALL_LAN: u8 = 1 << 0;
const ALL_HEVC: u8 = 1 << 1;
const WATCHER_IDR: u8 = 1 << 2;

// Watch's one flag: this watcher decodes HEVC.
const TAKES_HEVC: u8 = 1 << 0;

// VideoLoss before anything was measured.
const NO_LOSS_YET: u16 = u16::MAX;

// Capture runs up to 120 frames a second and a monitor up to 240.
pub(crate) const MAX_FPS: u8 = 240;
// A recover request covers frames a viewer dropped in 20 ms, or after an
// outage the frames the reassembler still follows (channels::video's AHEAD);
// a longer range is not one a viewer sends.
pub(crate) const MAX_RECOVER_SPAN: u32 = 1024;

// The family byte in front of an address.
const NO_ADDR: u8 = 0;
const V4: u8 = 4;
const V6: u8 = 6;

const ENTRY_MAX: usize = 32 + 1 + 1 + 2 + 1 + MAX_NAME_BYTES;
// One entry at most carries a share: its number and frame rate.
const ENTRY_SHARE: usize = 4 + 1;
const ROSTER_MAX: usize = 1 + 1 + MAX_NAME_BYTES + 1 + MAX_ROSTER * ENTRY_MAX + ENTRY_SHARE;
const _: () = assert!(ROSTER_MAX <= MAX_MESSAGE);
pub(crate) const CANDIDATE_MAX: usize = 1 + 1 + 16 + 2;
pub(crate) const MAX_HOSTNAME: usize = 253;
const ADDRESSES_MAX: usize = 1 + 1 + MAX_CANDIDATES * CANDIDATE_MAX + 1 + MAX_HOSTNAME;
const _: () = assert!(ADDRESSES_MAX <= MAX_MESSAGE);

pub(crate) enum Message {
    // What each side sends first on a link, the host too: its protocol,
    // which the other side must share, its Booth version and its name.
    // `reached` is the host address that answered the client's handshake.
    // It is only ever used to tell the host its port mapping works, and the
    // host's own Hello has none.
    Hello {
        version: Version,
        name: String,
        reached: Option<SocketAddr>,
    },
    // A Hello of another protocol. What follows its name is that protocol's
    // and is not read.
    OtherHello {
        protocol: u16,
        version: Version,
        name: String,
    },
    // The Hello of a test build from before version numbers.
    UnversionedHello {
        name: String,
    },
    PeerSecret {
        secret: Zeroizing<[u8; 32]>,
    },
    Roster(Roster),
    Bye,
    // Where the host can be reached now: what an invite made this moment
    // would carry. The client keeps it for rejoining later.
    HostAddresses {
        candidates: Vec<Candidate>,
        address_name: Option<String>,
    },
    // A listener to the host, once a second: for each person it heard over
    // the last 2 s, by slot, the share of their frames it lost.
    VoiceLoss(Vec<(u8, LossPermille)>),
    // The host to a talker, once a second: the worst any listener lost of
    // their voice, each of the two numbers on its own.
    WorstLoss(LossPermille),
    // Either way: this PC's audio periods and render latency.
    Periods(Periods),
    // A client to the host: may it share, at this many frames a second. It
    // captures nothing before the answer. Sent again while its share lasts,
    // it says the frame rate changed and gets the same answer.
    ShareStart {
        fps: u8,
    },
    ShareAnswer(ShareAnswer),
    // A client to the host: its share is over. Nothing answers it.
    ShareStop,
    // A client to the host: it watches the share, or stopped, and whether
    // its viewer decodes HEVC. Sent again while watching when its viewer
    // finds out it does not after all, or does.
    Watch {
        share: u32,
        on: bool,
        hevc: bool,
    },
    // A watcher to the host, and the host to the sharer: frames `first` to
    // `last`, wrapping, were dropped or would not decode.
    Recover {
        share: u32,
        first: u32,
        last: u32,
    },
    // A watcher to the host, and the host to the sharer: nothing decodes
    // until an IDR. `seen` is the frame that showed it; the host's own ask
    // for someone who just started watching names none.
    Idr {
        share: u32,
        seen: Option<u32>,
    },
    // A watcher to the host: its shard loss over the last 2 s in tenths of
    // a percent. The host to the sharer: the worst of every watcher's.
    VideoLoss {
        share: u32,
        loss: Option<u16>,
    },
    // The host to a client sharer: what the bitrate rule and the pacer need.
    ShareFacts(Facts),
    // The host to a client watching a client's share: the sharer's ping
    // clock minus the host's, and whether it was taken over a jittery link.
    SharerClock {
        share: u32,
        offset_us: i64,
        about: bool,
    },
    // A sharer to the host, and the host to each watcher: part of a pointer
    // shape.
    Shape(ShapeChunk),
    // A watcher to the host: may it control share `share`. `ask` is its own
    // number for this ask, which the host's answer and any end name.
    ControlAsk {
        share: u32,
        ask: u32,
    },
    // The host to a client sharing `share`: the person in `slot` asks to
    // control it. `control` is the host's number for it, never used twice
    // in a room.
    ControlAsked {
        share: u32,
        control: u32,
        slot: u8,
        name: String,
    },
    // The sharer to the host, with the host's number; the host to the one
    // who asked, with theirs.
    ControlAnswer {
        share: u32,
        number: u32,
        answer: ControlAnswer,
    },
    // Control ended, or an ask was taken back, with the same numbers: from
    // the controller or the sharer to the host, and the host to either.
    ControlEnd {
        share: u32,
        number: u32,
        why: ControlEnd,
    },
    // The sharer to the host, and the host to the controller: an
    // administrator window in front pauses control, or no longer does.
    ControlPaused {
        share: u32,
        number: u32,
        paused: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ControlAnswer {
    Allow,
    DontAllow,
    // The host to the one who asked: someone else asked or controls; one
    // controller at a time.
    Busy { key: [u8; 32], name: String },
    // The host to the one who asked: asked again too soon.
    TooSoon,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ShareAnswer {
    Granted { share: u32 },
    // Someone else shares. One share at a time: one viewer window, one upload
    // budget.
    Busy { key: [u8; 32], name: String },
    // Asked again too soon after the last ask.
    TooSoon,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Facts {
    pub share: u32,
    // Everyone watching, and those of them on an internet path.
    pub watchers: u8,
    pub internet: u8,
    // The host's upload setting divided by `internet`, in kbit/s. Zero when
    // nobody watches over the internet, and then the sharer's own setting
    // is the whole rule.
    pub cap_kbps: u32,
    // The sharer's link and every watcher's are on the LAN.
    pub lan: bool,
    // Every watcher decodes HEVC, so the share may go in it. True while
    // nobody watches.
    pub hevc: bool,
    // A watcher counted in these just started and needs an IDR. Its ask
    // comes in the facts rather than after them, so the sharer reads both
    // at once: when they change the codec, the new encoder's first frame is
    // the one IDR for both. Apart, it could read the facts a frame before
    // the ask and make an IDR for each.
    pub idr: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct EntryShare {
    // Every new share gets a new number, so a watcher knows a new stream.
    pub number: u32,
    pub fps: u8,
}

// A share of a talker's frames, in tenths of a percent: all that were lost,
// and those lost one or two in a row (view::VoiceLoss), which can never be
// more.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct LossPermille {
    pub all: u16,
    pub scattered: u16,
}

impl LossPermille {
    fn put(self, out: &mut Vec<u8>) {
        let all = self.all.min(PERMILLE);
        out.extend_from_slice(&all.to_le_bytes());
        out.extend_from_slice(&self.scattered.min(all).to_le_bytes());
    }
}

// None when that side has no stream open. Each at most a second, which no
// driver comes near.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Periods {
    pub input: Option<Duration>,
    pub output: Option<Duration>,
    pub render_latency: Option<Duration>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Roster {
    pub room: String,
    pub entries: Vec<Entry>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Entry {
    pub key: [u8; 32],
    // What the host's relayed voice calls this person: a talker never
    // names itself.
    pub slot: u8,
    pub name: String,
    pub rtt_ms: Option<u16>,
    pub is_host: bool,
    pub joined_by_invite: bool,
    pub reconnecting: bool,
    // What this person shares now. At most one entry of a roster has it.
    pub share: Option<EntryShare>,
    // This person controls the share, with the sharer's leave. At most one
    // entry has it, and only while another shares.
    pub controlling: bool,
}

impl Message {
    // Zeroizing because a PeerSecret sits in this buffer.
    pub(crate) fn encode(&self) -> Zeroizing<Vec<u8>> {
        let mut out = Zeroizing::new(Vec::with_capacity(64));
        match self {
            Message::Hello {
                version,
                name,
                reached,
            } => {
                put_hello(&mut out, PROTOCOL, *version, name);
                put_addr(&mut out, *reached);
            }
            Message::OtherHello {
                protocol,
                version,
                name,
            } => put_hello(&mut out, *protocol, *version, name),
            Message::UnversionedHello { name } => {
                out.push(UNVERSIONED_HELLO);
                put_text(&mut out, &clean(name, PERSON_FALLBACK));
                put_addr(&mut out, None);
            }
            Message::PeerSecret { secret } => {
                out.push(PEER_SECRET);
                out.extend_from_slice(secret.as_slice());
            }
            Message::Roster(roster) => {
                out.push(ROSTER);
                put_text(&mut out, &clean(&roster.room, ROOM_FALLBACK));
                let entries = roster.entries.iter().take(MAX_ROSTER);
                out.push(entries.len() as u8);
                for entry in entries {
                    out.extend_from_slice(&entry.key);
                    let mut flags = 0;
                    for (on, bit) in [
                        (entry.is_host, IS_HOST),
                        (entry.joined_by_invite, JOINED_BY_INVITE),
                        (entry.reconnecting, RECONNECTING),
                        (entry.share.is_some(), SHARING),
                        (entry.controlling, CONTROLLING),
                    ] {
                        if on {
                            flags |= bit;
                        }
                    }
                    out.push(flags);
                    out.push(entry.slot);
                    let rtt = entry.rtt_ms.map_or(NO_RTT, |ms| ms.min(NO_RTT - 1));
                    out.extend_from_slice(&rtt.to_le_bytes());
                    if let Some(share) = entry.share {
                        out.extend_from_slice(&share.number.to_le_bytes());
                        out.push(share.fps);
                    }
                    put_text(&mut out, &clean(&entry.name, PERSON_FALLBACK));
                }
            }
            Message::Bye => out.push(BYE),
            Message::HostAddresses {
                candidates,
                address_name,
            } => {
                out.push(HOST_ADDRESSES);
                let usable: Vec<&Candidate> = candidates
                    .iter()
                    .filter(|c| known::usable(c))
                    .take(MAX_CANDIDATES)
                    .collect();
                out.push(usable.len() as u8);
                for candidate in usable {
                    put_candidate(&mut out, candidate);
                }
                put_hostname(&mut out, address_name.as_deref());
            }
            Message::VoiceLoss(heard) => {
                out.push(VOICE_LOSS);
                let heard = &heard[..heard.len().min(MAX_ROSTER)];
                out.push(heard.len() as u8);
                for &(slot, lost) in heard {
                    out.push(slot);
                    lost.put(&mut out);
                }
            }
            Message::WorstLoss(lost) => {
                out.push(WORST_LOSS);
                lost.put(&mut out);
            }
            Message::Periods(periods) => {
                out.push(PERIODS);
                for period in [periods.input, periods.output, periods.render_latency] {
                    out.extend_from_slice(&micros(period).to_le_bytes());
                }
            }
            Message::ShareStart { fps } => {
                out.push(SHARE_START);
                out.push(*fps);
            }
            Message::ShareAnswer(answer) => {
                out.push(SHARE_ANSWER);
                match answer {
                    ShareAnswer::Granted { share } => {
                        out.push(GRANTED);
                        out.extend_from_slice(&share.to_le_bytes());
                    }
                    ShareAnswer::Busy { key, name } => {
                        out.push(BUSY);
                        out.extend_from_slice(key);
                        put_text(&mut out, &clean(name, PERSON_FALLBACK));
                    }
                    ShareAnswer::TooSoon => out.push(TOO_SOON),
                }
            }
            Message::ShareStop => out.push(SHARE_STOP),
            Message::Watch { share, on, hevc } => {
                out.push(WATCH);
                out.extend_from_slice(&share.to_le_bytes());
                out.push(u8::from(*on));
                out.push(if *hevc { TAKES_HEVC } else { 0 });
            }
            Message::Recover { share, first, last } => {
                out.push(RECOVER);
                for number in [share, first, last] {
                    out.extend_from_slice(&number.to_le_bytes());
                }
            }
            Message::Idr { share, seen } => {
                out.push(IDR);
                out.extend_from_slice(&share.to_le_bytes());
                match seen {
                    Some(seen) => {
                        out.push(1);
                        out.extend_from_slice(&seen.to_le_bytes());
                    }
                    None => out.push(0),
                }
            }
            Message::VideoLoss { share, loss } => {
                out.push(VIDEO_LOSS);
                out.extend_from_slice(&share.to_le_bytes());
                let loss = loss.map_or(NO_LOSS_YET, |loss| loss.min(PERMILLE));
                out.extend_from_slice(&loss.to_le_bytes());
            }
            Message::ShareFacts(facts) => {
                out.push(SHARE_FACTS);
                out.extend_from_slice(&facts.share.to_le_bytes());
                out.push(facts.watchers);
                out.push(facts.internet);
                out.extend_from_slice(&facts.cap_kbps.to_le_bytes());
                let mut flags = 0;
                if facts.lan {
                    flags |= ALL_LAN;
                }
                if facts.hevc {
                    flags |= ALL_HEVC;
                }
                if facts.idr {
                    flags |= WATCHER_IDR;
                }
                out.push(flags);
            }
            Message::SharerClock {
                share,
                offset_us,
                about,
            } => {
                out.push(SHARER_CLOCK);
                out.extend_from_slice(&share.to_le_bytes());
                out.extend_from_slice(&offset_us.to_le_bytes());
                out.push(u8::from(*about));
            }
            Message::Shape(chunk) => {
                out.push(SHAPE);
                chunk.write(&mut out);
            }
            Message::ControlAsk { share, ask } => {
                out.push(CONTROL_ASK);
                out.extend_from_slice(&share.to_le_bytes());
                out.extend_from_slice(&ask.to_le_bytes());
            }
            Message::ControlAsked {
                share,
                control,
                slot,
                name,
            } => {
                out.push(CONTROL_ASKED);
                out.extend_from_slice(&share.to_le_bytes());
                out.extend_from_slice(&control.to_le_bytes());
                out.push(*slot);
                put_text(&mut out, &clean(name, PERSON_FALLBACK));
            }
            Message::ControlAnswer {
                share,
                number,
                answer,
            } => {
                out.push(CONTROL_ANSWER);
                out.extend_from_slice(&share.to_le_bytes());
                out.extend_from_slice(&number.to_le_bytes());
                match answer {
                    ControlAnswer::Allow => out.push(ALLOW),
                    ControlAnswer::DontAllow => out.push(DONT_ALLOW),
                    ControlAnswer::Busy { key, name } => {
                        out.push(CONTROL_BUSY);
                        out.extend_from_slice(key);
                        put_text(&mut out, &clean(name, PERSON_FALLBACK));
                    }
                    ControlAnswer::TooSoon => out.push(CONTROL_TOO_SOON),
                }
            }
            Message::ControlEnd { share, number, why } => {
                out.push(CONTROL_END);
                out.extend_from_slice(&share.to_le_bytes());
                out.extend_from_slice(&number.to_le_bytes());
                out.push(match why {
                    ControlEnd::Released => RELEASED,
                    ControlEnd::Stopped => STOPPED,
                    ControlEnd::Panic => PANIC,
                    ControlEnd::EndedByHost => ENDED_BY_HOST,
                    ControlEnd::ShareEnded => SHARE_ENDED,
                    ControlEnd::SessionLost => SESSION_LOST,
                    ControlEnd::Closed => CLOSED,
                });
            }
            Message::ControlPaused {
                share,
                number,
                paused,
            } => {
                out.push(CONTROL_PAUSED);
                out.extend_from_slice(&share.to_le_bytes());
                out.extend_from_slice(&number.to_le_bytes());
                out.push(u8::from(*paused));
            }
        }
        out
    }

    // Names come back cleaned, so no caller can forget to.
    pub(crate) fn decode(buf: &[u8]) -> Option<Message> {
        let mut r = Reader(buf);
        let message = match r.u8()? {
            HELLO => {
                let protocol = u16::from_le_bytes(r.array()?);
                let version = r.version()?;
                let name = clean(r.text()?, PERSON_FALLBACK);
                if protocol == 0 {
                    return None;
                }
                if protocol != PROTOCOL {
                    return Some(Message::OtherHello {
                        protocol,
                        version,
                        name,
                    });
                }
                Message::Hello {
                    version,
                    name,
                    reached: r.addr()?,
                }
            }
            UNVERSIONED_HELLO => {
                let name = clean(r.text()?, PERSON_FALLBACK);
                // Builds from before the address was added end it after the
                // name. The address is read only to hold the old layout to
                // its rules; nothing but the name is used.
                if !r.0.is_empty() {
                    r.addr()?;
                }
                Message::UnversionedHello { name }
            }
            PEER_SECRET => Message::PeerSecret {
                secret: Zeroizing::new(r.array()?),
            },
            ROSTER => {
                let room = clean(r.text()?, ROOM_FALLBACK);
                let count = usize::from(r.u8()?);
                if count > MAX_ROSTER {
                    return None;
                }
                let mut entries = Vec::with_capacity(count);
                for _ in 0..count {
                    let key = r.array()?;
                    let flags = r.u8()?;
                    if flags & !KNOWN_FLAGS != 0 {
                        return None;
                    }
                    let slot = r.u8()?;
                    if usize::from(slot) >= MAX_ROSTER
                        || entries.iter().any(|e: &Entry| e.slot == slot)
                    {
                        return None;
                    }
                    let rtt = u16::from_le_bytes(r.array()?);
                    let share = if flags & SHARING != 0 {
                        // One share at a time, so two entries sharing is a
                        // roster no host sends.
                        if entries.iter().any(|e: &Entry| e.share.is_some()) {
                            return None;
                        }
                        Some(EntryShare {
                            number: r.share()?,
                            fps: r.fps()?,
                        })
                    } else {
                        None
                    };
                    entries.push(Entry {
                        key,
                        slot,
                        rtt_ms: (rtt != NO_RTT).then_some(rtt),
                        is_host: flags & IS_HOST != 0,
                        joined_by_invite: flags & JOINED_BY_INVITE != 0,
                        reconnecting: flags & RECONNECTING != 0,
                        share,
                        controlling: flags & CONTROLLING != 0,
                        name: clean(r.text()?, PERSON_FALLBACK),
                    });
                }
                // One controller, of a share someone else has.
                let mut controlling = entries.iter().filter(|e| e.controlling);
                if let Some(controller) = controlling.next()
                    && (controlling.next().is_some()
                        || controller.share.is_some()
                        || !entries.iter().any(|e| e.share.is_some()))
                {
                    return None;
                }
                Message::Roster(Roster { room, entries })
            }
            BYE => Message::Bye,
            HOST_ADDRESSES => {
                let count = usize::from(r.u8()?);
                if count > MAX_CANDIDATES {
                    return None;
                }
                let mut candidates = Vec::with_capacity(count);
                for _ in 0..count {
                    candidates.push(r.candidate()?);
                }
                Message::HostAddresses {
                    candidates,
                    address_name: r.hostname()?,
                }
            }
            VOICE_LOSS => {
                let count = usize::from(r.u8()?);
                if count > MAX_ROSTER {
                    return None;
                }
                let mut heard = Vec::with_capacity(count);
                for _ in 0..count {
                    let slot = r.u8()?;
                    // One report per talker, as a client sends: each can
                    // send that talker a WorstLoss.
                    if usize::from(slot) >= MAX_ROSTER
                        || heard.iter().any(|&(seen, _)| seen == slot)
                    {
                        return None;
                    }
                    heard.push((slot, r.lost()?));
                }
                Message::VoiceLoss(heard)
            }
            WORST_LOSS => Message::WorstLoss(r.lost()?),
            PERIODS => Message::Periods(Periods {
                input: period(r.array()?)?,
                output: period(r.array()?)?,
                render_latency: period(r.array()?)?,
            }),
            SHARE_START => Message::ShareStart { fps: r.fps()? },
            SHARE_ANSWER => Message::ShareAnswer(match r.u8()? {
                GRANTED => ShareAnswer::Granted { share: r.share()? },
                BUSY => ShareAnswer::Busy {
                    key: r.array()?,
                    name: clean(r.text()?, PERSON_FALLBACK),
                },
                TOO_SOON => ShareAnswer::TooSoon,
                _ => return None,
            }),
            SHARE_STOP => Message::ShareStop,
            WATCH => {
                let share = r.share()?;
                let on = r.yes_no()?;
                let flags = r.u8()?;
                if flags & !TAKES_HEVC != 0 {
                    return None;
                }
                Message::Watch {
                    share,
                    on,
                    hevc: flags & TAKES_HEVC != 0,
                }
            }
            RECOVER => {
                let share = r.share()?;
                let first = u32::from_le_bytes(r.array()?);
                let last = u32::from_le_bytes(r.array()?);
                if last.wrapping_sub(first) >= MAX_RECOVER_SPAN {
                    return None;
                }
                Message::Recover { share, first, last }
            }
            IDR => Message::Idr {
                share: r.share()?,
                seen: if r.yes_no()? {
                    Some(u32::from_le_bytes(r.array()?))
                } else {
                    None
                },
            },
            VIDEO_LOSS => {
                let share = r.share()?;
                let loss = match u16::from_le_bytes(r.array()?) {
                    NO_LOSS_YET => None,
                    loss if loss <= PERMILLE => Some(loss),
                    _ => return None,
                };
                Message::VideoLoss { share, loss }
            }
            SHARE_FACTS => {
                let share = r.share()?;
                let watchers = r.u8()?;
                let internet = r.u8()?;
                let cap_kbps = u32::from_le_bytes(r.array()?);
                let flags = r.u8()?;
                let idr = flags & WATCHER_IDR != 0;
                let fits = usize::from(watchers) <= MAX_CLIENTS
                    && internet <= watchers
                    && (internet == 0) == (cap_kbps == 0)
                    && flags & !(ALL_LAN | ALL_HEVC | WATCHER_IDR) == 0
                    && (!idr || watchers > 0);
                if !fits {
                    return None;
                }
                Message::ShareFacts(Facts {
                    share,
                    watchers,
                    internet,
                    cap_kbps,
                    lan: flags & ALL_LAN != 0,
                    hevc: flags & ALL_HEVC != 0,
                    idr,
                })
            }
            SHARER_CLOCK => Message::SharerClock {
                share: r.share()?,
                offset_us: i64::from_le_bytes(r.array()?),
                about: r.yes_no()?,
            },
            SHAPE => Message::Shape(ShapeChunk::read(&mut r)?),
            CONTROL_ASK => Message::ControlAsk {
                share: r.share()?,
                ask: r.number()?,
            },
            CONTROL_ASKED => {
                let share = r.share()?;
                let control = r.number()?;
                let slot = r.u8()?;
                if usize::from(slot) >= MAX_ROSTER {
                    return None;
                }
                Message::ControlAsked {
                    share,
                    control,
                    slot,
                    name: clean(r.text()?, PERSON_FALLBACK),
                }
            }
            CONTROL_ANSWER => Message::ControlAnswer {
                share: r.share()?,
                number: r.number()?,
                answer: match r.u8()? {
                    ALLOW => ControlAnswer::Allow,
                    DONT_ALLOW => ControlAnswer::DontAllow,
                    CONTROL_BUSY => ControlAnswer::Busy {
                        key: r.array()?,
                        name: clean(r.text()?, PERSON_FALLBACK),
                    },
                    CONTROL_TOO_SOON => ControlAnswer::TooSoon,
                    _ => return None,
                },
            },
            CONTROL_END => Message::ControlEnd {
                share: r.share()?,
                number: r.number()?,
                why: match r.u8()? {
                    RELEASED => ControlEnd::Released,
                    STOPPED => ControlEnd::Stopped,
                    PANIC => ControlEnd::Panic,
                    ENDED_BY_HOST => ControlEnd::EndedByHost,
                    SHARE_ENDED => ControlEnd::ShareEnded,
                    SESSION_LOST => ControlEnd::SessionLost,
                    CLOSED => ControlEnd::Closed,
                    _ => return None,
                },
            },
            CONTROL_PAUSED => Message::ControlPaused {
                share: r.share()?,
                number: r.number()?,
                paused: r.yes_no()?,
            },
            _ => return None,
        };
        r.0.is_empty().then_some(message)
    }
}

impl fmt::Debug for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Message::Hello {
                version,
                name,
                reached,
            } => f
                .debug_struct("Hello")
                .field("version", version)
                .field("name", name)
                .field("reached", reached)
                .finish(),
            Message::OtherHello {
                protocol,
                version,
                name,
            } => f
                .debug_struct("OtherHello")
                .field("protocol", protocol)
                .field("version", version)
                .field("name", name)
                .finish(),
            Message::UnversionedHello { name } => f
                .debug_struct("UnversionedHello")
                .field("name", name)
                .finish(),
            Message::PeerSecret { .. } => f.write_str("PeerSecret { (hidden) }"),
            Message::Roster(roster) => roster.fmt(f),
            Message::Bye => f.write_str("Bye"),
            Message::HostAddresses {
                candidates,
                address_name,
            } => f
                .debug_struct("HostAddresses")
                .field("candidates", candidates)
                .field("address_name", address_name)
                .finish(),
            Message::VoiceLoss(heard) => f.debug_tuple("VoiceLoss").field(heard).finish(),
            Message::WorstLoss(lost) => f.debug_tuple("WorstLoss").field(lost).finish(),
            Message::Periods(periods) => periods.fmt(f),
            Message::ShareStart { fps } => f.debug_struct("ShareStart").field("fps", fps).finish(),
            Message::ShareAnswer(answer) => answer.fmt(f),
            Message::ShareStop => f.write_str("ShareStop"),
            Message::Watch { share, on, hevc } => f
                .debug_struct("Watch")
                .field("share", share)
                .field("on", on)
                .field("hevc", hevc)
                .finish(),
            Message::Recover { share, first, last } => f
                .debug_struct("Recover")
                .field("share", share)
                .field("first", first)
                .field("last", last)
                .finish(),
            Message::Idr { share, seen } => f
                .debug_struct("Idr")
                .field("share", share)
                .field("seen", seen)
                .finish(),
            Message::VideoLoss { share, loss } => f
                .debug_struct("VideoLoss")
                .field("share", share)
                .field("loss", loss)
                .finish(),
            Message::ShareFacts(facts) => facts.fmt(f),
            Message::SharerClock {
                share,
                offset_us,
                about,
            } => f
                .debug_struct("SharerClock")
                .field("share", share)
                .field("offset_us", offset_us)
                .field("about", about)
                .finish(),
            Message::Shape(chunk) => f
                .debug_struct("Shape")
                .field("share", &chunk.share)
                .field("id", &chunk.id)
                .field("index", &chunk.index)
                .field("bytes", &chunk.bytes.len())
                .finish(),
            Message::ControlAsk { share, ask } => f
                .debug_struct("ControlAsk")
                .field("share", share)
                .field("ask", ask)
                .finish(),
            Message::ControlAsked {
                share,
                control,
                slot,
                name,
            } => f
                .debug_struct("ControlAsked")
                .field("share", share)
                .field("control", control)
                .field("slot", slot)
                .field("name", name)
                .finish(),
            Message::ControlAnswer {
                share,
                number,
                answer,
            } => f
                .debug_struct("ControlAnswer")
                .field("share", share)
                .field("number", number)
                .field("answer", answer)
                .finish(),
            Message::ControlEnd { share, number, why } => f
                .debug_struct("ControlEnd")
                .field("share", share)
                .field("number", number)
                .field("why", why)
                .finish(),
            Message::ControlPaused {
                share,
                number,
                paused,
            } => f
                .debug_struct("ControlPaused")
                .field("share", share)
                .field("number", number)
                .field("paused", paused)
                .finish(),
        }
    }
}

// Loss in tenths of a percent: all of it.
pub(crate) const PERMILLE: u16 = 1000;
const LONGEST_PERIOD: Duration = Duration::from_secs(1);

fn micros(period: Option<Duration>) -> u32 {
    period.map_or(0, |period| period.min(LONGEST_PERIOD).as_micros() as u32)
}

// The outer None is a period no driver has; the inner one is no stream.
fn period(bytes: [u8; 4]) -> Option<Option<Duration>> {
    match u32::from_le_bytes(bytes) {
        0 => Some(None),
        us => {
            let period = Duration::from_micros(u64::from(us));
            (period <= LONGEST_PERIOD).then_some(Some(period))
        }
    }
}

// Trimmed, without characters that draw nothing, at most 32 characters and 64
// bytes. A name made only of invisible characters would show as a blank row,
// one with an invisible character added looks like someone else's name but
// compares as different, so the panel's same-name check misses it, and
// U+202E can make a name read as someone else's.
pub(crate) fn clean(text: &str, fallback: &str) -> String {
    let kept: String = text.chars().filter(|c| !is_hidden(*c)).collect();
    let kept = drop_stacked_marks(&kept);
    let mut out = String::new();
    for c in kept.trim().chars().take(MAX_NAME_CHARS) {
        if out.len() + c.len_utf8() > MAX_NAME_BYTES {
            break;
        }
        out.push(c);
    }
    let out = out.trim_end();
    if out.is_empty() {
        fallback.to_owned()
    } else {
        out.to_owned()
    }
}

// The shaper stacks combining marks, and a letter with dozens of them draws
// far above or below its line, over other people's rows and messages. The
// marks past MAX_MARKS in a row go. Plex Sans Arabic draws the Arabic symbols
// U+FBB2 to U+FBC2 as marks and stacks them too, though Unicode does not
// count them as marks.
pub(crate) fn drop_stacked_marks(text: &str) -> String {
    let mut run = 0;
    text.chars()
        .filter(|c| {
            if unicode_bidi::bidi_class(*c) == unicode_bidi::BidiClass::NSM
                || matches!(c, '\u{FBB2}'..='\u{FBC2}')
            {
                run += 1;
            } else {
                run = 0;
            }
            run <= MAX_MARKS
        })
        .collect()
}

// Control characters, every format character (General_Category Cf), the rest
// of Default_Ignorable_Code_Point (variation selectors, the Hangul fillers and
// the like), the line and paragraph separators, and the blank braille pattern.
// Emoji joined with U+200D come apart and Persian loses its non-joiner; for a
// name that is a fair price.
pub(crate) fn is_hidden(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{AD}'
                | '\u{34F}'
                | '\u{600}'..='\u{605}'
                | '\u{61C}'
                | '\u{6DD}'
                | '\u{70F}'
                | '\u{890}'..='\u{891}'
                | '\u{8E2}'
                | '\u{115F}'..='\u{1160}'
                | '\u{17B4}'..='\u{17B5}'
                | '\u{180B}'..='\u{180F}'
                | '\u{200B}'..='\u{200F}'
                | '\u{2028}'..='\u{202E}'
                | '\u{2060}'..='\u{206F}'
                | '\u{2800}'
                | '\u{3164}'
                | '\u{FE00}'..='\u{FE0F}'
                | '\u{FEFF}'
                | '\u{FFA0}'
                | '\u{FFF0}'..='\u{FFFB}'
                | '\u{110BD}'
                | '\u{110CD}'
                | '\u{13430}'..='\u{1343F}'
                | '\u{1BCA0}'..='\u{1BCA3}'
                | '\u{1D173}'..='\u{1D17A}'
                | '\u{E0000}'..='\u{E0FFF}'
        )
}

// The part of a Hello every protocol keeps: its type, the protocol and the
// Booth version, little-endian like the rest of these messages, and the name.
fn put_hello(out: &mut Vec<u8>, protocol: u16, version: Version, name: &str) {
    out.push(HELLO);
    for part in [protocol, version.major, version.minor, version.patch] {
        out.extend_from_slice(&part.to_le_bytes());
    }
    put_text(out, &clean(name, PERSON_FALLBACK));
}

pub(crate) fn put_text(out: &mut Vec<u8>, text: &str) {
    // clean() already keeps this under 256 bytes; the cut is for safety only.
    let bytes = &text.as_bytes()[..text.len().min(MAX_NAME_BYTES)];
    out.push(bytes.len() as u8);
    out.extend_from_slice(bytes);
}

// A name invite::check_hostname refuses goes as no name, so what is written
// always reads back.
pub(crate) fn put_hostname(out: &mut Vec<u8>, name: Option<&str>) {
    match name.filter(|name| invite::check_hostname(name).is_ok()) {
        Some(name) => {
            out.push(name.len() as u8);
            out.extend_from_slice(name.as_bytes());
        }
        None => out.push(0),
    }
}

pub(crate) fn put_candidate(out: &mut Vec<u8>, candidate: &Candidate) {
    out.push(match candidate.kind {
        CandidateKind::Lan => 0,
        CandidateKind::Vpn => 1,
        CandidateKind::Ipv6 => 2,
        CandidateKind::Public => 3,
    });
    put_addr(out, Some(candidate.addr));
}

// A scope id or flow label means nothing on the other PC, so only the
// address and the port go over.
pub(crate) fn put_addr(out: &mut Vec<u8>, addr: Option<SocketAddr>) {
    match addr {
        None => out.push(NO_ADDR),
        Some(SocketAddr::V4(v4)) => {
            out.push(V4);
            out.extend_from_slice(&v4.ip().octets());
            out.extend_from_slice(&v4.port().to_le_bytes());
        }
        Some(SocketAddr::V6(v6)) => {
            out.push(V6);
            out.extend_from_slice(&v6.ip().octets());
            out.extend_from_slice(&v6.port().to_le_bytes());
        }
    }
}

pub(crate) struct Reader<'a>(pub(crate) &'a [u8]);

impl<'a> Reader<'a> {
    pub(crate) fn u8(&mut self) -> Option<u8> {
        let (&first, rest) = self.0.split_first()?;
        self.0 = rest;
        Some(first)
    }

    pub(crate) fn bytes(&mut self, len: usize) -> Option<&'a [u8]> {
        if len > self.0.len() {
            return None;
        }
        let (taken, rest) = self.0.split_at(len);
        self.0 = rest;
        Some(taken)
    }

    pub(crate) fn array<const N: usize>(&mut self) -> Option<[u8; N]> {
        self.bytes(N)?.try_into().ok()
    }

    pub(crate) fn text(&mut self) -> Option<&'a str> {
        let len = usize::from(self.u8()?);
        std::str::from_utf8(self.bytes(len)?).ok()
    }

    fn version(&mut self) -> Option<Version> {
        let mut part = || self.array().map(u16::from_le_bytes);
        Some(Version {
            major: part()?,
            minor: part()?,
            patch: part()?,
        })
    }

    // A share number, which is never zero.
    fn share(&mut self) -> Option<u32> {
        let share = u32::from_le_bytes(self.array()?);
        (share != 0).then_some(share)
    }

    // A control's number or an ask's, which is never zero either.
    fn number(&mut self) -> Option<u32> {
        self.share()
    }

    fn fps(&mut self) -> Option<u8> {
        let fps = self.u8()?;
        (1..=MAX_FPS).contains(&fps).then_some(fps)
    }

    fn yes_no(&mut self) -> Option<bool> {
        match self.u8()? {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        }
    }

    fn lost(&mut self) -> Option<LossPermille> {
        let all = u16::from_le_bytes(self.array()?);
        let scattered = u16::from_le_bytes(self.array()?);
        (all <= PERMILLE && scattered <= all).then_some(LossPermille { all, scattered })
    }

    // The outer None is bytes that do not parse, the inner one no name.
    pub(crate) fn hostname(&mut self) -> Option<Option<String>> {
        let name = self.text()?;
        if name.is_empty() {
            return Some(None);
        }
        invite::check_hostname(name).ok()?;
        Some(Some(name.to_owned()))
    }

    // Only what an invite could carry, by the invite's own rules.
    pub(crate) fn candidate(&mut self) -> Option<Candidate> {
        let kind = match self.u8()? {
            0 => CandidateKind::Lan,
            1 => CandidateKind::Vpn,
            2 => CandidateKind::Ipv6,
            3 => CandidateKind::Public,
            _ => return None,
        };
        let candidate = Candidate {
            kind,
            addr: self.addr()??,
        };
        known::usable(&candidate).then_some(candidate)
    }

    // The outer None is a message that does not parse; the inner one is a
    // message that carries no address.
    pub(crate) fn addr(&mut self) -> Option<Option<SocketAddr>> {
        let ip = match self.u8()? {
            NO_ADDR => return Some(None),
            V4 => Ipv4Addr::from(self.array::<4>()?).into(),
            V6 => Ipv6Addr::from(self.array::<16>()?).into(),
            _ => return None,
        };
        let port = u16::from_le_bytes(self.array()?);
        Some(Some(SocketAddr::new(ip, port)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(key: u8, name: &str) -> Entry {
        Entry {
            key: [key; 32],
            slot: key,
            name: name.to_owned(),
            rtt_ms: Some(12),
            is_host: key == 0,
            joined_by_invite: key % 2 == 1,
            reconnecting: key == 3,
            share: (key == 2).then_some(EntryShare {
                number: 7,
                fps: 120,
            }),
            controlling: key == 3,
        }
    }

    fn roster(names: &[&str]) -> Roster {
        Roster {
            room: "Friday night".to_owned(),
            entries: names
                .iter()
                .enumerate()
                .map(|(i, name)| entry(i as u8, name))
                .collect(),
        }
    }

    fn round_trip(message: &Message) -> Message {
        Message::decode(&message.encode()).expect("decodes")
    }

    #[test]
    fn every_message_round_trips() {
        let reached: [Option<SocketAddr>; 3] = [
            None,
            Some("203.0.113.7:41000".parse().unwrap()),
            Some("[2001:db8::7]:52000".parse().unwrap()),
        ];
        let later = Version {
            major: 0,
            minor: 9,
            patch: 1,
        };
        for sent in reached {
            match round_trip(&Message::Hello {
                version: later,
                name: "Ana".to_owned(),
                reached: sent,
            }) {
                Message::Hello {
                    version,
                    name,
                    reached,
                } => {
                    assert_eq!(version, later);
                    assert_eq!(name, "Ana");
                    assert_eq!(reached, sent);
                }
                other => panic!("{other:?}"),
            }
        }
        match round_trip(&Message::OtherHello {
            protocol: PROTOCOL + 1,
            version: later,
            name: "Bo".to_owned(),
        }) {
            Message::OtherHello {
                protocol,
                version,
                name,
            } => {
                assert_eq!(protocol, PROTOCOL + 1);
                assert_eq!(version, later);
                assert_eq!(name, "Bo");
            }
            other => panic!("{other:?}"),
        }
        match round_trip(&Message::UnversionedHello {
            name: "Cy".to_owned(),
        }) {
            Message::UnversionedHello { name } => assert_eq!(name, "Cy"),
            other => panic!("{other:?}"),
        }
        match round_trip(&Message::PeerSecret {
            secret: Zeroizing::new([7; 32]),
        }) {
            Message::PeerSecret { secret } => assert_eq!(*secret, [7; 32]),
            other => panic!("{other:?}"),
        }
        let sent = roster(&["Host", "Ana", "Bo", "Cy"]);
        match round_trip(&Message::Roster(sent.clone())) {
            Message::Roster(got) => assert_eq!(got, sent),
            other => panic!("{other:?}"),
        }
        assert!(matches!(round_trip(&Message::Bye), Message::Bye));
        for sent in [
            addresses(),
            Message::HostAddresses {
                candidates: Vec::new(),
                address_name: None,
            },
        ] {
            match (round_trip(&sent), sent) {
                (
                    Message::HostAddresses {
                        candidates,
                        address_name,
                    },
                    Message::HostAddresses {
                        candidates: want,
                        address_name: want_name,
                    },
                ) => {
                    assert_eq!(candidates, want);
                    assert_eq!(address_name, want_name);
                }
                (other, _) => panic!("{other:?}"),
            }
        }
    }

    fn candidate(kind: CandidateKind, addr: &str) -> Candidate {
        Candidate {
            kind,
            addr: addr.parse().unwrap(),
        }
    }

    // What a home host behind a mapped port has to say.
    fn addresses() -> Message {
        Message::HostAddresses {
            candidates: vec![
                candidate(CandidateKind::Lan, "192.168.1.20:41000"),
                candidate(CandidateKind::Vpn, "100.64.1.2:41000"),
                candidate(CandidateKind::Ipv6, "[2001:db8::20]:41000"),
                candidate(CandidateKind::Public, "203.0.113.9:41000"),
            ],
            address_name: Some(String::from("myroom.example.net")),
        }
    }

    #[test]
    fn host_addresses_follow_invite_rules() {
        let sent = Message::HostAddresses {
            candidates: vec![
                candidate(CandidateKind::Public, "192.168.1.20:41000"),
                candidate(CandidateKind::Lan, "127.0.0.1:41000"),
                candidate(CandidateKind::Lan, "192.168.1.20:41000"),
            ],
            address_name: Some(String::from("localhost")),
        };
        match round_trip(&sent) {
            Message::HostAddresses {
                candidates,
                address_name,
            } => {
                assert_eq!(
                    candidates,
                    [candidate(CandidateKind::Lan, "192.168.1.20:41000")]
                );
                assert_eq!(address_name, None);
            }
            other => panic!("{other:?}"),
        }

        let good = addresses().encode().to_vec();
        // Kind, then family, of the first candidate.
        let (kind_at, family_at) = (2, 3);
        let mut unknown_kind = good.clone();
        unknown_kind[kind_at] = 4;
        let mut unknown_family = good.clone();
        unknown_family[family_at] = 5;
        let mut too_many = vec![HOST_ADDRESSES, MAX_CANDIDATES as u8 + 1];
        for _ in 0..=MAX_CANDIDATES {
            put_candidate(
                &mut too_many,
                &candidate(CandidateKind::Lan, "192.168.1.20:41000"),
            );
        }
        too_many.push(0);
        // A public candidate on a private address, as a forged message would
        // aim the client's handshakes into its own network.
        let mut private = vec![HOST_ADDRESSES, 1];
        put_candidate(
            &mut private,
            &candidate(CandidateKind::Public, "203.0.113.9:41000"),
        );
        private.push(0);
        private[4..8].copy_from_slice(&[192, 168, 1, 20]);
        let mut bad_name = vec![HOST_ADDRESSES, 0, 9];
        bad_name.extend_from_slice(b"localhost");
        let mut trailing = good.clone();
        trailing.push(0);
        for bad in [
            &[HOST_ADDRESSES][..],
            &[HOST_ADDRESSES, 0][..],
            &good[..good.len() - 1],
            &unknown_kind,
            &unknown_family,
            &too_many,
            &private,
            &bad_name,
            &trailing,
        ] {
            assert!(Message::decode(bad).is_none(), "{bad:?}");
        }
    }

    // What every protocol's Hello starts with, written out by hand.
    fn hello_head(protocol: u16, version: [u16; 3], name: &str) -> Vec<u8> {
        let mut out = vec![HELLO];
        out.extend_from_slice(&protocol.to_le_bytes());
        for part in version {
            out.extend_from_slice(&part.to_le_bytes());
        }
        out.push(name.len() as u8);
        out.extend_from_slice(name.as_bytes());
        out
    }

    // Test builds from before version numbers end the Hello after the name,
    // or in the later ones after the address. Both are named.
    #[test]
    fn unversioned_hello_is_read() {
        let mut with_address = vec![UNVERSIONED_HELLO, 3, b'A', b'n', b'a', V4];
        with_address.extend_from_slice(&[203, 0, 113, 7, 0xa8, 0xa0]);
        for raw in [&[UNVERSIONED_HELLO, 3, b'A', b'n', b'a'][..], &with_address] {
            match Message::decode(raw) {
                Some(Message::UnversionedHello { name }) => assert_eq!(name, "Ana"),
                other => panic!("{other:?}"),
            }
        }
        let mut trailing = with_address.clone();
        trailing.push(0);
        assert!(Message::decode(&trailing).is_none());
        // What this build says to such a host is byte for byte what those
        // builds sent with no address.
        let ours = Message::UnversionedHello {
            name: String::from("Ana"),
        };
        assert_eq!(
            ours.encode().as_slice(),
            [UNVERSIONED_HELLO, 3, b'A', b'n', b'a', NO_ADDR]
        );
    }

    // Whatever a later protocol puts after the name, its version is read.
    #[test]
    fn other_protocol_hello_read_to_name() {
        for tail in [&[][..], &[NO_ADDR][..], &[0xff; 300][..]] {
            let mut raw = hello_head(PROTOCOL + 1, [0, 2, 0], "Bo");
            raw.extend_from_slice(tail);
            match Message::decode(&raw) {
                Some(Message::OtherHello {
                    protocol,
                    version,
                    name,
                }) => {
                    assert_eq!(protocol, PROTOCOL + 1);
                    assert_eq!(version.to_string(), "0.2.0");
                    assert_eq!(name, "Bo");
                }
                other => panic!("{other:?}"),
            }
        }
        // This protocol's Hello is held to its layout.
        let mut ours = hello_head(PROTOCOL, [0, 1, 7], "Bo");
        assert!(Message::decode(&ours).is_none(), "no address byte");
        ours.push(NO_ADDR);
        assert!(matches!(
            Message::decode(&ours),
            Some(Message::Hello { reached: None, .. })
        ));
        ours.push(0);
        assert!(Message::decode(&ours).is_none(), "a byte too many");
        let mut zero = hello_head(0, [0, 1, 0], "Bo");
        zero.push(NO_ADDR);
        assert!(Message::decode(&zero).is_none(), "protocol 0");
        let whole = hello_head(PROTOCOL + 1, [0, 2, 0], "Bo");
        for len in 0..whole.len() {
            assert!(Message::decode(&whole[..len]).is_none(), "cut to {len}");
        }
    }

    #[test]
    fn missing_round_trip_survives() {
        let mut sent = roster(&["Host", "Ana"]);
        sent.entries[1].rtt_ms = None;
        sent.entries[0].rtt_ms = Some(u16::MAX);
        let Some(Message::Roster(got)) = Message::decode(&Message::Roster(sent).encode()) else {
            panic!("roster did not decode");
        };
        assert_eq!(got.entries[1].rtt_ms, None);
        assert_eq!(got.entries[0].rtt_ms, Some(u16::MAX - 1));
    }

    #[test]
    fn largest_roster_fits_one_message() {
        let longest = "\u{e9}".repeat(40);
        let names = vec![longest.as_str(); MAX_ROSTER];
        let mut full = roster(&names);
        full.room = longest.clone();
        let encoded = Message::Roster(full).encode();
        assert_eq!(encoded.len(), ROSTER_MAX);
        assert!(encoded.len() <= MAX_MESSAGE);
    }

    #[test]
    fn names_are_cleaned() {
        let cases: &[(&str, &str)] = &[
            ("  Ana  ", "Ana"),
            ("\u{7}Bo\n", "Bo"),
            ("", PERSON_FALLBACK),
            (" \t\r\n ", PERSON_FALLBACK),
            ("\u{202E}", PERSON_FALLBACK),
            ("ev\u{202E}il", "evil"),
            ("a\u{2066}b\u{2069}c", "abc"),
            ("two  words", "two  words"),
            (" \u{0}  padded", "padded"),
            ("\u{200B}", PERSON_FALLBACK),
            ("\u{FEFF}", PERSON_FALLBACK),
            ("\u{2060}", PERSON_FALLBACK),
            ("\u{AD}", PERSON_FALLBACK),
            ("\u{E0041}\u{E0042}", PERSON_FALLBACK),
            ("\u{3164}", PERSON_FALLBACK),
            ("\u{115F}\u{1160}\u{FFA0}", PERSON_FALLBACK),
            ("\u{180E}\u{2800}", PERSON_FALLBACK),
            (" \u{200B} Ana \u{FEFF}", "Ana"),
            ("Ana\u{200D}", "Ana"),
            ("A\u{34F}n\u{FE0F}a\u{E0100}", "Ana"),
        ];
        for (raw, want) in cases {
            assert_eq!(clean(raw, PERSON_FALLBACK), *want, "{raw:?}");
        }

        assert_eq!(clean(&"a".repeat(40), PERSON_FALLBACK), "a".repeat(32));
        // The cut at 32 characters lands on a space, which must not stay.
        let spaced = format!("{} b", "a".repeat(31));
        assert_eq!(clean(&spaced, PERSON_FALLBACK), "a".repeat(31));
        let wide = clean(&"\u{1F600}".repeat(20), PERSON_FALLBACK);
        assert_eq!(wide.chars().count(), 16);
        assert!(wide.len() <= MAX_NAME_BYTES);
        assert_eq!(clean("", ROOM_FALLBACK), ROOM_FALLBACK);
    }

    #[test]
    fn received_names_are_cleaned() {
        let name = format!(" \u{1b}[31m{} ", "x".repeat(50));
        let mut raw = hello_head(PROTOCOL, [0, 1, 0], &name);
        raw.push(NO_ADDR);
        let other = hello_head(PROTOCOL + 1, [0, 2, 0], &name);
        let mut unversioned = vec![UNVERSIONED_HELLO, name.len() as u8];
        unversioned.extend_from_slice(name.as_bytes());
        for raw in [raw, other, unversioned] {
            match Message::decode(&raw) {
                Some(
                    Message::Hello { name, .. }
                    | Message::OtherHello { name, .. }
                    | Message::UnversionedHello { name },
                ) => {
                    assert!(!name.contains('\u{1b}'));
                    assert_eq!(name.chars().count(), MAX_NAME_CHARS);
                }
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn bad_shapes_are_refused() {
        let good = Message::Roster(roster(&["Host", "Ana"])).encode();
        let mut trailing = good.to_vec();
        trailing.push(0);
        let mut bad_flags = good.to_vec();
        let flags_at = 1 + 1 + "Friday night".len() + 1 + 32;
        bad_flags[flags_at] |= 0x80;
        let mut too_many = vec![ROSTER, 0, MAX_ROSTER as u8 + 1];
        too_many.extend(std::iter::repeat_n(0, 40 * (MAX_ROSTER + 1)));
        let mut bad_utf8 = hello_head(PROTOCOL, [0, 1, 0], "ab");
        let name_at = bad_utf8.len() - 2;
        bad_utf8[name_at..].copy_from_slice(&[0xC3, 0x28]);
        bad_utf8.push(NO_ADDR);
        let hello = Message::Hello {
            version: invite::VERSION,
            name: "Ana".to_owned(),
            reached: Some("203.0.113.7:41000".parse().unwrap()),
        }
        .encode();
        let family_at = 1 + 8 + 1 + "Ana".len();
        let mut unknown_family = hello.to_vec();
        unknown_family[family_at] = 5;
        let mut v6_too_short = hello.to_vec();
        v6_too_short[family_at] = V6;
        let mut name_too_long = hello_head(PROTOCOL, [0, 1, 0], "a");
        let len_at = name_too_long.len() - 2;
        name_too_long[len_at] = 5;

        for bad in [
            &[][..],
            &[0][..],
            &[BYE, 0][..],
            &[PEER_SECRET, 1, 2, 3][..],
            &name_too_long,
            &[UNVERSIONED_HELLO, 5, b'a'][..],
            &[UNVERSIONED_HELLO, 1, b'a', NO_ADDR, 0][..],
            &unknown_family,
            &v6_too_short,
            &hello[..hello.len() - 1],
            &trailing,
            &bad_flags,
            &too_many,
            &bad_utf8,
            &good[..good.len() - 1],
        ] {
            assert!(Message::decode(bad).is_none(), "{bad:?}");
        }
    }

    // xorshift64*, seeded, so a failure shows up the same way every run.
    struct Random(u64);

    impl Random {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }

        fn byte(&mut self) -> u8 {
            self.next() as u8
        }
    }

    // Random bodies behind each real kind byte, so the parser gets past the
    // first check most of the time instead of stopping at an unknown kind.
    #[test]
    fn random_bytes_never_panic() {
        let mut random = Random(0x9E37_79B9_7F4A_7C15);
        for _ in 0..50_000 {
            let len = random.below(400);
            let mut buf: Vec<u8> = (0..len).map(|_| random.byte()).collect();
            if let Some(first) = buf.first_mut() {
                *first = match random.below(12) {
                    0 => HELLO,
                    1 => PEER_SECRET,
                    2 | 3 => ROSTER,
                    4 => BYE,
                    5 | 6 => HOST_ADDRESSES,
                    7 => VOICE_LOSS,
                    8 => WORST_LOSS,
                    9 => PERIODS,
                    10 => UNVERSIONED_HELLO,
                    _ => *first,
                };
            }
            match Message::decode(&buf) {
                Some(
                    Message::Hello { name, .. }
                    | Message::OtherHello { name, .. }
                    | Message::UnversionedHello { name },
                ) => assert!(name.len() <= MAX_NAME_BYTES && !name.chars().any(is_hidden)),
                Some(Message::Roster(roster)) => assert!(roster.entries.len() <= MAX_ROSTER),
                Some(Message::VoiceLoss(heard)) => {
                    assert!(heard.len() <= MAX_ROSTER);
                    heard.iter().for_each(|(_, lost)| within_limits(*lost));
                }
                Some(Message::WorstLoss(lost)) => within_limits(lost),
                Some(message @ Message::HostAddresses { .. }) => only_usable(&message),
                _ => {}
            }
        }
    }

    #[test]
    fn edited_messages_never_panic() {
        let mut random = Random(0xD1B5_4A32_D192_ED03);
        let seeds = [
            Message::Roster(roster(&["Host", "Ana", "\u{1F600}\u{1F600}", "Cy"]))
                .encode()
                .to_vec(),
            Message::Hello {
                version: invite::VERSION,
                name: "Ana".to_owned(),
                reached: None,
            }
            .encode()
            .to_vec(),
            Message::Hello {
                version: invite::VERSION,
                name: "Bo".to_owned(),
                reached: Some("203.0.113.7:41000".parse().unwrap()),
            }
            .encode()
            .to_vec(),
            Message::Hello {
                version: invite::VERSION,
                name: "Cy".to_owned(),
                reached: Some("[2001:db8::7]:52000".parse().unwrap()),
            }
            .encode()
            .to_vec(),
            Message::OtherHello {
                protocol: PROTOCOL + 1,
                version: invite::VERSION,
                name: "Di".to_owned(),
            }
            .encode()
            .to_vec(),
            Message::UnversionedHello {
                name: "Ed".to_owned(),
            }
            .encode()
            .to_vec(),
            Message::PeerSecret {
                secret: Zeroizing::new([1; 32]),
            }
            .encode()
            .to_vec(),
            addresses().encode().to_vec(),
        ];
        for _ in 0..50_000 {
            let mut buf = seeds[random.below(seeds.len())].clone();
            for _ in 0..1 + random.below(6) {
                let at = random.below(buf.len() + 1);
                match random.below(3) {
                    0 if at < buf.len() => buf[at] = random.byte(),
                    1 => buf.insert(at, random.byte()),
                    _ if at < buf.len() => {
                        buf.remove(at);
                    }
                    _ => {}
                }
            }
            match Message::decode(&buf) {
                Some(
                    Message::Hello { name, .. }
                    | Message::OtherHello { name, .. }
                    | Message::UnversionedHello { name },
                ) => assert!(name.len() <= MAX_NAME_BYTES && !name.chars().any(is_hidden)),
                Some(Message::Roster(roster)) => {
                    for entry in &roster.entries {
                        assert!(entry.name.chars().count() <= MAX_NAME_CHARS);
                        assert!(!entry.name.chars().any(is_hidden));
                    }
                }
                Some(message @ Message::HostAddresses { .. }) => only_usable(&message),
                _ => {}
            }
        }
    }

    fn within_limits(lost: LossPermille) {
        assert!(
            lost.all <= PERMILLE && lost.scattered <= lost.all,
            "{lost:?}"
        );
    }

    // Whatever gets through is something an invite could have carried.
    fn only_usable(message: &Message) {
        let Message::HostAddresses {
            candidates,
            address_name,
        } = message
        else {
            return;
        };
        assert!(candidates.len() <= MAX_CANDIDATES);
        assert!(candidates.iter().all(known::usable), "{candidates:?}");
        if let Some(name) = address_name {
            assert!(invite::check_hostname(name).is_ok(), "{name}");
        }
    }

    #[test]
    fn voice_reports_round_trip() {
        let lost = |all, scattered| LossPermille { all, scattered };
        let heard = vec![
            (0, lost(0, 0)),
            (3, lost(125, 40)),
            (7, lost(PERMILLE, PERMILLE)),
        ];
        match round_trip(&Message::VoiceLoss(heard.clone())) {
            Message::VoiceLoss(got) => assert_eq!(got, heard),
            other => panic!("{other:?}"),
        }
        match round_trip(&Message::WorstLoss(lost(55, 0))) {
            Message::WorstLoss(got) => assert_eq!(got, lost(55, 0)),
            other => panic!("{other:?}"),
        }
        let periods = Periods {
            input: Some(Duration::from_micros(2667)),
            output: None,
            render_latency: Some(Duration::from_micros(20_000)),
        };
        match round_trip(&Message::Periods(periods)) {
            Message::Periods(got) => assert_eq!(got, periods),
            other => panic!("{other:?}"),
        }
        // Over the limit on the way out is held to it, and scattered loss
        // is never more than all of it.
        match round_trip(&Message::WorstLoss(lost(5000, 7000))) {
            Message::WorstLoss(got) => assert_eq!(got, lost(PERMILLE, PERMILLE)),
            other => panic!("{other:?}"),
        }
        match round_trip(&Message::WorstLoss(lost(20, 90))) {
            Message::WorstLoss(got) => assert_eq!(got, lost(20, 20)),
            other => panic!("{other:?}"),
        }

        let over = PERMILLE + 1;
        let two = |all: u16, scattered: u16| {
            let mut bytes = all.to_le_bytes().to_vec();
            bytes.extend_from_slice(&scattered.to_le_bytes());
            bytes
        };
        let mut too_much = vec![VOICE_LOSS, 1, 2];
        too_much.extend(two(over, 0));
        let mut more_scattered = vec![VOICE_LOSS, 1, 2];
        more_scattered.extend(two(10, 11));
        let mut no_slot = vec![VOICE_LOSS, 1, MAX_ROSTER as u8];
        no_slot.extend(two(5, 5));
        let mut too_many = vec![VOICE_LOSS, MAX_ROSTER as u8 + 1];
        for slot in 0..=MAX_ROSTER as u8 {
            too_many.push(slot % MAX_ROSTER as u8);
            too_many.extend(two(1, 0));
        }
        let mut worst = vec![WORST_LOSS];
        worst.extend(two(over, 0));
        let mut worst_scattered = vec![WORST_LOSS];
        worst_scattered.extend(two(0, 1));
        // What an older build sent, before scattered loss had a number of its
        // own: one number, not two.
        let mut one_number = vec![WORST_LOSS];
        one_number.extend_from_slice(&55u16.to_le_bytes());
        let mut long_period = vec![PERIODS];
        long_period.extend_from_slice(&2_000_000u32.to_le_bytes());
        long_period.extend_from_slice(&[0; 8]);
        let mut dup_slot = Message::Roster(roster(&["Host", "Ana"])).encode().to_vec();
        let second_slot = 1 + 1 + "Friday night".len() + 1 + (32 + 1 + 1 + 2 + 1 + 4) + 32 + 1;
        dup_slot[second_slot] = 0;
        for bad in [
            &too_much[..],
            &more_scattered,
            &no_slot,
            &too_many,
            &worst,
            &worst_scattered,
            &one_number,
            &long_period,
            &[PERIODS, 0, 0, 0][..],
            &[WORST_LOSS, 1][..],
            &dup_slot,
        ] {
            assert!(Message::decode(bad).is_none(), "{bad:?}");
        }
    }
}

// The messages screen sharing added, kinds 9 to 18.
#[cfg(test)]
mod share_tests {
    use super::*;
    use crate::screen::wire::{self, Shape, ShapeKind};
    use proptest::prelude::*;

    fn round_trip(message: &Message) -> Message {
        Message::decode(&message.encode()).expect("decodes")
    }

    fn same(message: Message) {
        let bytes = message.encode();
        let back = Message::decode(&bytes).expect("decodes");
        assert_eq!(back.encode().to_vec(), bytes.to_vec(), "{message:?}");
    }

    fn arrow() -> Shape {
        Shape {
            kind: ShapeKind::Monochrome,
            width: 32,
            height: 64,
            pitch: 4,
            hotspot_x: 1,
            hotspot_y: 1,
            scale_milli: 1000,
            bytes: vec![0x55; 256],
        }
    }

    fn every() -> Vec<Message> {
        vec![
            Message::ShareStart { fps: 120 },
            Message::ShareAnswer(ShareAnswer::Granted { share: 7 }),
            Message::ShareAnswer(ShareAnswer::Busy {
                key: [9; 32],
                name: String::from("Ines"),
            }),
            Message::ShareAnswer(ShareAnswer::TooSoon),
            Message::ShareStop,
            Message::Watch {
                share: 7,
                on: true,
                hevc: true,
            },
            Message::Watch {
                share: 7,
                on: true,
                hevc: false,
            },
            Message::Watch {
                share: u32::MAX,
                on: false,
                hevc: false,
            },
            Message::Recover {
                share: 7,
                first: u32::MAX - 1,
                last: 2,
            },
            Message::Idr {
                share: 7,
                seen: Some(40),
            },
            Message::Idr {
                share: 7,
                seen: None,
            },
            Message::VideoLoss {
                share: 7,
                loss: Some(PERMILLE),
            },
            Message::VideoLoss {
                share: 7,
                loss: None,
            },
            Message::ShareFacts(Facts {
                share: 7,
                watchers: 3,
                internet: 2,
                cap_kbps: 7500,
                lan: false,
                hevc: false,
                idr: true,
            }),
            Message::ShareFacts(Facts {
                share: 7,
                watchers: 0,
                internet: 0,
                cap_kbps: 0,
                lan: true,
                hevc: true,
                idr: false,
            }),
            Message::SharerClock {
                share: 7,
                offset_us: -4_400,
                about: true,
            },
            Message::Shape(wire::chunks(7, 3, &arrow()).next().unwrap()),
        ]
    }

    #[test]
    fn every_share_message_round_trips() {
        for message in every() {
            same(message);
        }
        match round_trip(&Message::Recover {
            share: 7,
            first: u32::MAX - 1,
            last: 2,
        }) {
            Message::Recover { share, first, last } => {
                assert_eq!((share, first, last), (7, u32::MAX - 1, 2));
            }
            other => panic!("{other:?}"),
        }
        match round_trip(&Message::ShareAnswer(ShareAnswer::Busy {
            key: [9; 32],
            name: String::from(" In\u{202E}es "),
        })) {
            Message::ShareAnswer(ShareAnswer::Busy { key, name }) => {
                assert_eq!((key, name.as_str()), ([9; 32], "Ines"));
            }
            other => panic!("{other:?}"),
        }
    }

    // Kind first, little endian.
    #[test]
    fn share_message_bytes() {
        let bytes = |message: Message| message.encode().to_vec();
        assert_eq!(bytes(Message::ShareStart { fps: 120 }), [9, 120]);
        assert_eq!(
            bytes(Message::ShareAnswer(ShareAnswer::Granted { share: 7 })),
            [10, 0, 7, 0, 0, 0]
        );
        assert_eq!(bytes(Message::ShareAnswer(ShareAnswer::TooSoon)), [10, 2]);
        assert_eq!(bytes(Message::ShareStop), [11]);
        assert_eq!(
            bytes(Message::Watch {
                share: 7,
                on: true,
                hevc: true
            }),
            [12, 7, 0, 0, 0, 1, 1]
        );
        assert_eq!(
            bytes(Message::Recover {
                share: 7,
                first: 1,
                last: 2
            }),
            [13, 7, 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0]
        );
        assert_eq!(
            bytes(Message::Idr {
                share: 7,
                seen: None
            }),
            [14, 7, 0, 0, 0, 0]
        );
        assert_eq!(
            bytes(Message::VideoLoss {
                share: 7,
                loss: None
            }),
            [15, 7, 0, 0, 0, 0xFF, 0xFF]
        );
        assert_eq!(
            bytes(Message::ShareFacts(Facts {
                share: 7,
                watchers: 2,
                internet: 1,
                cap_kbps: 15_000,
                lan: false,
                hevc: true,
                idr: true
            })),
            [16, 7, 0, 0, 0, 2, 1, 0x98, 0x3A, 0, 0, 6]
        );
        assert_eq!(
            bytes(Message::SharerClock {
                share: 7,
                offset_us: -1,
                about: false
            }),
            [17, 7, 0, 0, 0, 255, 255, 255, 255, 255, 255, 255, 255, 0]
        );
        let shape = bytes(Message::Shape(wire::chunks(7, 3, &arrow()).next().unwrap()));
        assert_eq!(shape[..15], [18, 7, 0, 0, 0, 3, 0, 0, 0, 0, 1, 0, 0, 0, 0]);
        assert_eq!(shape.len(), 1 + 14 + 13 + 2 + 256);
    }

    #[test]
    fn roster_has_one_sharer() {
        let entry = |key: u8, share: Option<EntryShare>| Entry {
            key: [key; 32],
            slot: key,
            name: format!("P{key}"),
            rtt_ms: None,
            is_host: key == 0,
            joined_by_invite: false,
            reconnecting: false,
            share,
            controlling: false,
        };
        let sharing = EntryShare {
            number: 12,
            fps: 60,
        };
        let one = Roster {
            room: String::from("Room"),
            entries: vec![entry(0, None), entry(1, Some(sharing)), entry(2, None)],
        };
        match round_trip(&Message::Roster(one.clone())) {
            Message::Roster(got) => assert_eq!(got, one),
            other => panic!("{other:?}"),
        }
        let two = Roster {
            room: String::from("Room"),
            entries: vec![entry(0, Some(sharing)), entry(1, Some(sharing))],
        };
        assert!(Message::decode(&Message::Roster(two).encode()).is_none());
        // Share number zero, and a frame rate past the limit.
        let mut zero = Message::Roster(one.clone()).encode().to_vec();
        let number_at = 1 + 1 + 4 + 1 + (32 + 1 + 1 + 2 + 1 + 2) + 32 + 1 + 1 + 2;
        zero[number_at..number_at + 4].copy_from_slice(&[0; 4]);
        assert!(Message::decode(&zero).is_none());
        let mut fast = Message::Roster(one).encode().to_vec();
        fast[number_at + 4] = MAX_FPS + 1;
        assert!(Message::decode(&fast).is_none());
    }

    #[test]
    fn bent_share_messages_refused() {
        let mut facts_mismatch = vec![SHARE_FACTS, 7, 0, 0, 0, 2, 0];
        facts_mismatch.extend_from_slice(&500u32.to_le_bytes());
        facts_mismatch.push(0);
        let mut facts_more_internet = vec![SHARE_FACTS, 7, 0, 0, 0, 1, 2];
        facts_more_internet.extend_from_slice(&500u32.to_le_bytes());
        facts_more_internet.push(0);
        let mut facts_crowd = vec![SHARE_FACTS, 7, 0, 0, 0, MAX_CLIENTS as u8 + 1, 0];
        facts_crowd.extend_from_slice(&0u32.to_le_bytes());
        facts_crowd.push(0);
        let mut facts_flags = vec![SHARE_FACTS, 7, 0, 0, 0, 1, 0];
        facts_flags.extend_from_slice(&0u32.to_le_bytes());
        facts_flags.push(8);
        // An IDR ask for a watcher the facts do not count.
        let mut facts_idr_nobody = vec![SHARE_FACTS, 7, 0, 0, 0, 0, 0];
        facts_idr_nobody.extend_from_slice(&0u32.to_le_bytes());
        facts_idr_nobody.push(WATCHER_IDR);
        let mut long_range = vec![RECOVER, 7, 0, 0, 0];
        long_range.extend_from_slice(&10u32.to_le_bytes());
        long_range.extend_from_slice(&(10 + MAX_RECOVER_SPAN).to_le_bytes());
        let mut backwards = vec![RECOVER, 7, 0, 0, 0];
        backwards.extend_from_slice(&10u32.to_le_bytes());
        backwards.extend_from_slice(&9u32.to_le_bytes());
        let mut too_lost = vec![VIDEO_LOSS, 7, 0, 0, 0];
        too_lost.extend_from_slice(&(PERMILLE + 1).to_le_bytes());
        let mut trailing = Message::Watch {
            share: 7,
            on: true,
            hevc: true,
        }
        .encode()
        .to_vec();
        trailing.push(0);
        for bad in [
            &[SHARE_START, 0][..],
            &[SHARE_START, MAX_FPS + 1][..],
            &[SHARE_START][..],
            &[SHARE_ANSWER, 3][..],
            &[SHARE_ANSWER, GRANTED, 0, 0, 0, 0][..],
            &[SHARE_ANSWER, BUSY, 1, 2, 3][..],
            &[SHARE_STOP, 0][..],
            &[WATCH, 0, 0, 0, 0, 1, 0][..],
            &[WATCH, 7, 0, 0, 0, 2, 0][..],
            &[WATCH, 7, 0, 0, 0, 1][..],
            &[WATCH, 7, 0, 0, 0, 1, 2][..],
            &[IDR, 7, 0, 0, 0, 2][..],
            &[IDR, 7, 0, 0, 0, 1, 5][..],
            &[SHARER_CLOCK, 7, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8, 2][..],
            &facts_mismatch,
            &facts_more_internet,
            &facts_crowd,
            &facts_flags,
            &facts_idr_nobody,
            &long_range,
            &backwards,
            &too_lost,
            &trailing,
            &[SHAPE, 1, 0, 0, 0][..],
        ] {
            assert!(Message::decode(bad).is_none(), "{bad:?}");
        }
    }

    // xorshift64*, seeded, so a failure shows up the same way every run.
    struct Random(u64);

    impl Random {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    // Whatever gets through is something a peer following the rules could
    // send: it encodes back to the same bytes.
    fn check(bytes: &[u8]) {
        let Some(message) = Message::decode(bytes) else {
            return;
        };
        match &message {
            // Names come back cleaned, so their bytes can differ.
            Message::ShareAnswer(ShareAnswer::Busy { name, .. })
            | Message::ControlAsked { name, .. }
            | Message::ControlAnswer {
                answer: ControlAnswer::Busy { name, .. },
                ..
            } => {
                assert!(name.len() <= MAX_NAME_BYTES && !name.chars().any(is_hidden));
            }
            // Another protocol's Hello is read only up to the name.
            Message::Hello { .. }
            | Message::OtherHello { .. }
            | Message::UnversionedHello { .. }
            | Message::PeerSecret { .. }
            | Message::Roster(_)
            | Message::HostAddresses { .. } => {}
            _ => assert_eq!(message.encode().to_vec(), bytes, "{message:?}"),
        }
    }

    #[test]
    fn share_fuzz_never_panics() {
        let seeds: Vec<Vec<u8>> = every().iter().map(|m| m.encode().to_vec()).collect();
        let mut random = Random(0x9E37_79B9_7F4A_7C15);
        for _ in 0..50_000 {
            let mut bytes = seeds[random.below(seeds.len())].clone();
            for _ in 0..1 + random.below(4) {
                let at = random.below(bytes.len() + 1);
                match random.below(3) {
                    0 if at < bytes.len() => bytes[at] = random.next() as u8,
                    1 => bytes.insert(at, random.next() as u8),
                    _ if at < bytes.len() => {
                        bytes.remove(at);
                    }
                    _ => {}
                }
            }
            check(&bytes);
        }
        for _ in 0..50_000 {
            let len = random.below(1200);
            let mut bytes: Vec<u8> = (0..len).map(|_| random.next() as u8).collect();
            if let Some(first) = bytes.first_mut() {
                *first = SHARE_START + (random.below(10) as u8);
            }
            check(&bytes);
        }
    }

    proptest! {
        #[test]
        fn any_share_start_and_answer_round_trip(
            fps in 1u8..=MAX_FPS,
            share in 1u32..,
            key in any::<[u8; 32]>(),
            name in "[a-zA-Z ]{1,40}",
        ) {
            same(Message::ShareStart { fps });
            same(Message::ShareAnswer(ShareAnswer::Granted { share }));
            same(Message::ShareAnswer(ShareAnswer::Busy { key, name }));
        }

        #[test]
        fn any_watch_recover_and_idr_round_trip(
            share in 1u32..,
            on in any::<bool>(),
            hevc in any::<bool>(),
            first in any::<u32>(),
            span in 0u32..MAX_RECOVER_SPAN,
            seen in prop::option::of(any::<u32>()),
        ) {
            same(Message::Watch { share, on, hevc });
            same(Message::Recover { share, first, last: first.wrapping_add(span) });
            same(Message::Idr { share, seen });
        }

        #[test]
        fn any_loss_facts_and_clock_round_trip(
            share in 1u32..,
            loss in prop::option::of(0u16..=PERMILLE),
            watchers in 0u8..=MAX_CLIENTS as u8,
            internet in 0u8..=MAX_CLIENTS as u8,
            cap_kbps in 1u32..,
            lan in any::<bool>(),
            hevc in any::<bool>(),
            idr in any::<bool>(),
            offset_us in any::<i64>(),
            about in any::<bool>(),
        ) {
            same(Message::VideoLoss { share, loss });
            let internet = internet.min(watchers);
            same(Message::ShareFacts(Facts {
                share,
                watchers,
                internet,
                cap_kbps: if internet == 0 { 0 } else { cap_kbps },
                lan,
                hevc,
                idr: idr && watchers > 0,
            }));
            same(Message::SharerClock { share, offset_us, about });
        }

        #[test]
        fn any_share_bytes_parse_or_refused(
            kind in SHARE_START..=SHAPE,
            body in prop::collection::vec(any::<u8>(), 0..64),
        ) {
            let mut bytes = vec![kind];
            bytes.extend(body);
            check(&bytes);
        }
    }
}

// The messages remote control added, kinds 19 to 23, and the roster's
// controlling flag.
#[cfg(test)]
mod control_tests {
    use super::*;
    use proptest::prelude::*;

    const ENDS: [ControlEnd; 7] = [
        ControlEnd::Released,
        ControlEnd::Stopped,
        ControlEnd::Panic,
        ControlEnd::EndedByHost,
        ControlEnd::ShareEnded,
        ControlEnd::SessionLost,
        ControlEnd::Closed,
    ];

    fn same(message: Message) {
        let bytes = message.encode();
        let back = Message::decode(&bytes).expect("decodes");
        assert_eq!(back.encode().to_vec(), bytes.to_vec(), "{message:?}");
    }

    fn every() -> Vec<Message> {
        let mut all = vec![
            Message::ControlAsk { share: 7, ask: 1 },
            Message::ControlAsked {
                share: 7,
                control: 3,
                slot: 2,
                name: String::from("Mara"),
            },
            Message::ControlAnswer {
                share: 7,
                number: 3,
                answer: ControlAnswer::Allow,
            },
            Message::ControlAnswer {
                share: 7,
                number: 3,
                answer: ControlAnswer::DontAllow,
            },
            Message::ControlAnswer {
                share: 7,
                number: 1,
                answer: ControlAnswer::Busy {
                    key: [4; 32],
                    name: String::from("Tom"),
                },
            },
            Message::ControlAnswer {
                share: 7,
                number: 1,
                answer: ControlAnswer::TooSoon,
            },
            Message::ControlPaused {
                share: 7,
                number: 3,
                paused: true,
            },
        ];
        all.extend(ENDS.map(|why| Message::ControlEnd {
            share: 7,
            number: 3,
            why,
        }));
        all
    }

    #[test]
    fn every_control_message_round_trips() {
        for message in every() {
            same(message);
        }
        match Message::decode(
            &Message::ControlAsked {
                share: 7,
                control: 3,
                slot: 2,
                name: String::from(" Ma\u{202E}ra "),
            }
            .encode(),
        ) {
            Some(Message::ControlAsked {
                share,
                control,
                slot,
                name,
            }) => assert_eq!((share, control, slot, name.as_str()), (7, 3, 2, "Mara")),
            other => panic!("{other:?}"),
        }
    }

    // Kind first, little endian.
    #[test]
    fn control_message_bytes() {
        let bytes = |message: Message| message.encode().to_vec();
        assert_eq!(
            bytes(Message::ControlAsk { share: 7, ask: 1 }),
            [19, 7, 0, 0, 0, 1, 0, 0, 0]
        );
        assert_eq!(
            bytes(Message::ControlAsked {
                share: 7,
                control: 3,
                slot: 2,
                name: String::from("Mara"),
            }),
            [20, 7, 0, 0, 0, 3, 0, 0, 0, 2, 4, b'M', b'a', b'r', b'a']
        );
        assert_eq!(
            bytes(Message::ControlAnswer {
                share: 7,
                number: 3,
                answer: ControlAnswer::Allow
            }),
            [21, 7, 0, 0, 0, 3, 0, 0, 0, 0]
        );
        assert_eq!(
            bytes(Message::ControlAnswer {
                share: 7,
                number: 3,
                answer: ControlAnswer::TooSoon
            }),
            [21, 7, 0, 0, 0, 3, 0, 0, 0, 3]
        );
        let busy = bytes(Message::ControlAnswer {
            share: 7,
            number: 1,
            answer: ControlAnswer::Busy {
                key: [4; 32],
                name: String::from("Tom"),
            },
        });
        assert_eq!(busy[..10], [21, 7, 0, 0, 0, 1, 0, 0, 0, 2]);
        assert_eq!(busy.len(), 10 + 32 + 1 + 3);
        assert_eq!(
            bytes(Message::ControlEnd {
                share: 7,
                number: 3,
                why: ControlEnd::Panic
            }),
            [22, 7, 0, 0, 0, 3, 0, 0, 0, 2]
        );
        assert_eq!(
            bytes(Message::ControlPaused {
                share: 7,
                number: 3,
                paused: true
            }),
            [23, 7, 0, 0, 0, 3, 0, 0, 0, 1]
        );
    }

    #[test]
    fn bent_control_messages_refused() {
        let mut trailing = Message::ControlAsk { share: 7, ask: 1 }.encode().to_vec();
        trailing.push(0);
        for bad in [
            &[CONTROL_ASK, 0, 0, 0, 0, 1, 0, 0, 0][..],
            &[CONTROL_ASK, 7, 0, 0, 0, 0, 0, 0, 0][..],
            &[CONTROL_ASK, 7, 0, 0, 0, 1, 0, 0][..],
            &[
                CONTROL_ASKED,
                7,
                0,
                0,
                0,
                3,
                0,
                0,
                0,
                MAX_ROSTER as u8,
                1,
                b'M',
            ][..],
            &[CONTROL_ASKED, 7, 0, 0, 0, 3, 0, 0, 0, 2, 5, b'M'][..],
            &[CONTROL_ANSWER, 7, 0, 0, 0, 3, 0, 0, 0, 4][..],
            &[CONTROL_ANSWER, 7, 0, 0, 0, 3, 0, 0, 0][..],
            &[CONTROL_ANSWER, 7, 0, 0, 0, 3, 0, 0, 0, CONTROL_BUSY, 1, 2][..],
            &[CONTROL_END, 7, 0, 0, 0, 3, 0, 0, 0, 7][..],
            &[CONTROL_END, 7, 0, 0, 0, 0, 0, 0, 0, 0][..],
            &[CONTROL_PAUSED, 7, 0, 0, 0, 3, 0, 0, 0, 2][..],
            &[CONTROL_PAUSED, 7, 0, 0, 0, 3, 0, 0, 0, 1, 0][..],
            &trailing,
        ] {
            assert!(Message::decode(bad).is_none(), "{bad:?}");
        }
    }

    fn entry(key: u8, share: Option<EntryShare>, controlling: bool) -> Entry {
        Entry {
            key: [key; 32],
            slot: key,
            name: format!("P{key}"),
            rtt_ms: None,
            is_host: key == 0,
            joined_by_invite: false,
            reconnecting: false,
            share,
            controlling,
        }
    }

    #[test]
    fn roster_has_one_controller() {
        let sharing = Some(EntryShare {
            number: 12,
            fps: 60,
        });
        let roster = |entries| {
            Message::Roster(Roster {
                room: String::from("Room"),
                entries,
            })
        };
        // The controller before the sharer in the list, and after it.
        for entries in [
            vec![entry(0, None, true), entry(1, sharing, false)],
            vec![entry(0, sharing, false), entry(1, None, true)],
        ] {
            match Message::decode(&roster(entries.clone()).encode()) {
                Some(Message::Roster(got)) => assert_eq!(got.entries, entries),
                other => panic!("{other:?}"),
            }
        }
        for entries in [
            vec![
                entry(0, sharing, false),
                entry(1, None, true),
                entry(2, None, true),
            ],
            vec![entry(0, sharing, true), entry(1, None, false)],
            vec![entry(0, None, true), entry(1, None, false)],
        ] {
            assert!(
                Message::decode(&roster(entries.clone()).encode()).is_none(),
                "{entries:?}"
            );
        }
    }

    // Whatever gets through is something a peer following the rules could
    // send: it encodes back to the same bytes, names aside.
    fn check(bytes: &[u8]) {
        let Some(message) = Message::decode(bytes) else {
            return;
        };
        match &message {
            Message::ShareAnswer(ShareAnswer::Busy { name, .. })
            | Message::ControlAsked { name, .. }
            | Message::ControlAnswer {
                answer: ControlAnswer::Busy { name, .. },
                ..
            } => {
                assert!(name.len() <= MAX_NAME_BYTES && !name.chars().any(is_hidden));
            }
            // Another protocol's Hello is read only up to the name.
            Message::Hello { .. }
            | Message::OtherHello { .. }
            | Message::UnversionedHello { .. }
            | Message::PeerSecret { .. }
            | Message::Roster(_)
            | Message::HostAddresses { .. } => {}
            _ => assert_eq!(message.encode().to_vec(), bytes, "{message:?}"),
        }
    }

    #[test]
    fn edited_control_messages_never_panic() {
        let seeds: Vec<Vec<u8>> = every().iter().map(|m| m.encode().to_vec()).collect();
        let mut state = 0xD1B5_4A32_D192_ED03u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..50_000 {
            let mut bytes = seeds[(next() % seeds.len() as u64) as usize].clone();
            for _ in 0..1 + next() % 4 {
                let at = (next() as usize) % (bytes.len() + 1);
                match next() % 3 {
                    0 if at < bytes.len() => bytes[at] = next() as u8,
                    1 => bytes.insert(at, next() as u8),
                    _ if at < bytes.len() => {
                        bytes.remove(at);
                    }
                    _ => {}
                }
            }
            check(&bytes);
        }
    }

    fn any_end() -> impl Strategy<Value = ControlEnd> {
        (0..ENDS.len()).prop_map(|at| ENDS[at])
    }

    proptest! {
        #[test]
        fn any_control_message_round_trips(
            share in 1u32..,
            number in 1u32..,
            slot in 0u8..MAX_ROSTER as u8,
            key in any::<[u8; 32]>(),
            name in "[a-zA-Z ]{1,40}",
            why in any_end(),
            paused in any::<bool>(),
        ) {
            same(Message::ControlAsk { share, ask: number });
            same(Message::ControlAsked { share, control: number, slot, name: name.clone() });
            for answer in [
                ControlAnswer::Allow,
                ControlAnswer::DontAllow,
                ControlAnswer::Busy { key, name },
                ControlAnswer::TooSoon,
            ] {
                same(Message::ControlAnswer { share, number, answer });
            }
            same(Message::ControlEnd { share, number, why });
            same(Message::ControlPaused { share, number, paused });
        }

        #[test]
        fn any_control_bytes_parse_or_refused(
            kind in CONTROL_ASK..=CONTROL_PAUSED,
            body in prop::collection::vec(any::<u8>(), 0..64),
        ) {
            let mut bytes = vec![kind];
            bytes.extend(body);
            check(&bytes);
        }
    }
}
