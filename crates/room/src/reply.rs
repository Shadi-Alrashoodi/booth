// The reply code on both sides. The client makes one when nothing answers
// its tries; the friend sends it back over their chat, and the host pastes it
// and sends a few punch packets toward the address in it, which opens the
// host's own router for that friend. The host's side keeps what it punched,
// and for which key.

use std::fmt;
use std::net::{SocketAddr, SocketAddrV4, SocketAddrV6};
use std::time::{Duration, Instant};

use invite::{Answers, BuildError, Candidate, CandidateKind, Mapping, ReplyCode};

use crate::log::{Log, list, log};
use crate::peer;
use crate::socket::Socket;
use crate::view::ReplyState;

// At most ten packets per address, 200 ms apart: enough to open the host's
// router for the friend, too few for a pasted code to flood anyone with.
pub(crate) const PUNCH_ROUNDS: u32 = 10;
pub(crate) const PUNCH_GAP: Duration = Duration::from_millis(200);
// At most one accepted paste per key this often.
pub(crate) const PASTE_GAP: Duration = Duration::from_secs(10);
const REPLY_LIFETIME: Duration = Duration::from_secs(invite::REPLY_SECS);
// Every record comes from someone pasting a code by hand, so this is far
// above what a real room sees.
const MAX_PUNCHES: usize = 32;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReplyRefused {
    // The room has closed, or this is not a host.
    Closed,
    // This PC's own router changes ports for every connection, so no punch
    // can open a path that stays.
    HostHard,
    Expired,
    // The invite it answers expired, or let in someone else.
    InviteNotLive,
    // A rejoin code from a key this host holds no secret for.
    NotKnown,
    // A code from a key this host blocked, whatever it answers.
    Blocked,
    AlreadyHere { name: String },
    TooSoon,
    // Nothing in the code this PC can send to: no address at all, or only
    // IPv6 on a PC without it.
    NoAddress,
    // The friend's router changes ports for every connection.
    FriendHard,
    // A code ReplyCode::decode would have refused, built some other way.
    BadCode(BuildError),
    // Someone else in the room is at that address, or it was punched open
    // for someone else. Taking it would lock them out of it.
    AddressTaken { addr: SocketAddr },
    // Eight people are in the room already, the most it holds.
    RoomFull,
}

// Words for the log. The panel has its own sentences.
impl fmt::Display for ReplyRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReplyRefused::Closed => f.write_str("the room is not open"),
            ReplyRefused::HostHard => {
                f.write_str("this host's own mapping is hard, so punching cannot help")
            }
            ReplyRefused::Expired => f.write_str("the code has expired on this host's clock"),
            ReplyRefused::InviteNotLive => {
                f.write_str("it answers an invite that is not open for this key")
            }
            ReplyRefused::NotKnown => {
                f.write_str("a rejoin code from a key this host holds no secret for")
            }
            ReplyRefused::Blocked => f.write_str("the key is blocked"),
            ReplyRefused::AlreadyHere { name } => {
                write!(f, "that key is in the room as {}", crate::log::quoted(name))
            }
            ReplyRefused::TooSoon => write!(
                f,
                "a code for that key was accepted less than {} s ago",
                PASTE_GAP.as_secs()
            ),
            ReplyRefused::NoAddress => {
                f.write_str("the code carries no address this host can send to")
            }
            ReplyRefused::FriendHard => f.write_str("the friend's mapping is hard"),
            ReplyRefused::BadCode(err) => write!(f, "decode would have refused it: {err}"),
            ReplyRefused::AddressTaken { addr } => write!(
                f,
                "{addr} belongs to another key, which is in the room there or had it punched open"
            ),
            ReplyRefused::RoomFull => f.write_str("the room is full"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplyAccepted {
    // Where the punch packets go.
    pub to: Vec<SocketAddr>,
}

// The client's side: which screen applies. A code is worth showing only when
// both routers keep one outside port per socket and STUN told this PC what
// its own is. The host's hard mapping comes first because its fix, a
// forward on the host, works whatever the client's router does.
pub(crate) fn choose(
    host: Mapping,
    second_router: bool,
    own: Mapping,
    has_address: bool,
    host_outside: bool,
) -> ReplyState {
    if !host_outside {
        ReplyState::HostNoAddress
    } else if host == Mapping::Hard {
        ReplyState::HostHard
    } else if own == Mapping::Hard {
        ReplyState::OwnHard
    } else if !has_address {
        ReplyState::NoAddress
    } else {
        ReplyState::Code { second_router }
    }
}

// The port the host is bound to. Local candidates carry it, and so does the
// last public one, which is the outside address with the bound port.
pub(crate) fn host_port(candidates: &[Candidate]) -> Option<u16> {
    let local = candidates.iter().find(|c| c.kind != CandidateKind::Public);
    let forwarded = candidates
        .iter()
        .rev()
        .find(|c| c.kind == CandidateKind::Public);
    local.or(forwarded).map(|c| c.addr.port())
}

// An address STUN reported that a code cannot carry (a private one from an
// odd server) is left out and the rest go in. None when nothing is left.
pub(crate) fn build(
    answers: Answers,
    client_key: [u8; 32],
    mut v4: Option<SocketAddrV4>,
    mut v6: Option<SocketAddrV6>,
    mapping: Mapping,
    now_unix: u64,
    log: &Log,
) -> Option<ReplyCode> {
    loop {
        if v4.is_none() && v6.is_none() {
            return None;
        }
        match ReplyCode::new(answers, client_key, v4, v6, mapping, now_unix) {
            Ok(code) => return Some(code),
            Err(BuildError::BadAddress { addr, reason }) => {
                log!(log, "reply code: {addr} left out, {reason}");
                match addr {
                    SocketAddr::V4(_) => v4 = None,
                    SocketAddr::V6(_) => v6 = None,
                }
            }
            Err(err) => {
                log!(log, "reply code: could not make one: {err}");
                return None;
            }
        }
    }
}

// A received IPv6 source can carry a flow label the code's address does not,
// and the rules here are about the place, not the label.
pub(crate) fn same_address(a: SocketAddr, b: SocketAddr) -> bool {
    a.ip() == b.ip() && a.port() == b.port()
}

// The host's side: one record per accepted paste.
struct Punch {
    key: [u8; 32],
    to: Vec<SocketAddr>,
    rounds: u32,
    next_round: Option<Instant>,
    // The code's expiry on this host's clock. Until then an initiation from
    // one of `to` is taken only with this key.
    until: Instant,
    joined: bool,
    // When the last round went out, until the host has said the friend did
    // not come in after it.
    done_at: Option<Instant>,
}

#[derive(Default)]
pub(crate) struct Punches {
    records: Vec<Punch>,
    // When a paste for each key was last accepted.
    accepted: Vec<([u8; 32], Instant)>,
}

impl Punches {
    pub(crate) fn too_soon(&self, key: &[u8; 32], now: Instant) -> bool {
        self.accepted
            .iter()
            .any(|(k, at)| k == key && now.saturating_duration_since(*at) < PASTE_GAP)
    }

    // `left` is how long the code still has on its own clock. The record
    // never outlives a fresh code, whatever expiry a code claims.
    pub(crate) fn start(
        &mut self,
        key: [u8; 32],
        to: Vec<SocketAddr>,
        left: Duration,
        now: Instant,
    ) {
        self.accepted
            .retain(|(k, at)| k != &key && now.saturating_duration_since(*at) < PASTE_GAP);
        self.accepted.push((key, now));
        self.records.retain(|p| p.key != key);
        if self.records.len() >= MAX_PUNCHES {
            self.records.remove(0);
        }
        self.records.push(Punch {
            key,
            to,
            rounds: 0,
            next_round: Some(now),
            until: now + left.min(REPLY_LIFETIME),
            joined: false,
            done_at: None,
        });
    }

    // The key another live record punched `from` open for, when it is not
    // `key`. Everyone else keeps the normal rules.
    pub(crate) fn held_for_other(
        &self,
        from: SocketAddr,
        key: &[u8; 32],
        now: Instant,
    ) -> Option<[u8; 32]> {
        self.records
            .iter()
            .find(|p| {
                now < p.until && p.key != *key && p.to.iter().any(|to| same_address(*to, from))
            })
            .map(|p| p.key)
    }

    // Sends each round that is due. Nothing but the 32-byte packets goes out.
    // The socket writes down why a send failed, once a minute per address,
    // so the round line only names which ones did.
    pub(crate) fn send_due(&mut self, now: Instant, socket: &Socket, log: &Log) {
        for punch in &mut self.records {
            if !punch.next_round.is_some_and(|at| now >= at) {
                continue;
            }
            let (mut sent, mut failed) = (Vec::new(), Vec::new());
            for &to in &punch.to {
                match socket.send_to(&session::punch_packet(&peer::random()), to) {
                    Ok(_) => sent.push(to),
                    Err(_) => failed.push(to),
                }
            }
            punch.rounds += 1;
            if log.is_on() {
                let what = match (sent.is_empty(), failed.is_empty()) {
                    (false, true) => format!(" sent to {}", list(&sent)),
                    (false, false) => format!(
                        " sent to {}, could not send to {}",
                        list(&sent),
                        list(&failed)
                    ),
                    (true, _) => format!(": could not send to {}", list(&failed)),
                };
                log.line(format!(
                    "punch round {} of {PUNCH_ROUNDS} for {}{what}",
                    punch.rounds,
                    keys::fingerprint(&punch.key)
                ));
            }
            punch.next_round = if punch.rounds < PUNCH_ROUNDS {
                Some(now + PUNCH_GAP)
            } else {
                log!(
                    log,
                    "all punch rounds for {} are done; it has not joined yet",
                    keys::fingerprint(&punch.key)
                );
                punch.done_at = Some(now);
                None
            };
        }
    }

    // Lets records go at their code's expiry. Returns the keys that never
    // joined.
    pub(crate) fn expire(&mut self, now: Instant, log: &Log) -> Vec<[u8; 32]> {
        let mut missed = Vec::new();
        self.records.retain(|p| {
            if now < p.until {
                return true;
            }
            if !p.joined {
                log!(
                    log,
                    "the code for {} expired and it never joined",
                    keys::fingerprint(&p.key)
                );
                missed.push(p.key);
            }
            false
        });
        self.accepted
            .retain(|(_, at)| now.saturating_duration_since(*at) < PASTE_GAP);
        missed
    }

    // That key's session was confirmed: no more rounds. Returns how many
    // went out, when it had a record still sending or waiting.
    pub(crate) fn joined(&mut self, key: &[u8; 32]) -> Option<u32> {
        let punch = self
            .records
            .iter_mut()
            .find(|p| p.key == *key && !p.joined)?;
        punch.joined = true;
        punch.next_round = None;
        Some(punch.rounds)
    }

    // The keys whose last round went out at least `after` ago with no
    // handshake since, each given once.
    pub(crate) fn unanswered(&mut self, now: Instant, after: Duration) -> Vec<[u8; 32]> {
        let mut keys = Vec::new();
        for punch in &mut self.records {
            if !punch.joined && punch.done_at.is_some_and(|at| now >= at + after) {
                punch.done_at = None;
                keys.push(punch.key);
            }
        }
        keys
    }

    // `after` as unanswered takes it.
    pub(crate) fn next_deadline(&self, after: Duration) -> Option<Instant> {
        self.records
            .iter()
            .flat_map(|p| {
                let unanswered = p.done_at.filter(|_| !p.joined).map(|at| at + after);
                [p.next_round, Some(p.until), unanswered]
            })
            .flatten()
            .min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Wire;
    use invite::Invite;
    use std::net::Ipv4Addr;

    #[test]
    fn code_needs_two_keeping_routers() {
        use Mapping::{Easy, Hard, Unknown};
        for (host, own, has_address, want) in [
            (
                Easy,
                Easy,
                true,
                ReplyState::Code {
                    second_router: false,
                },
            ),
            (
                Unknown,
                Unknown,
                true,
                ReplyState::Code {
                    second_router: false,
                },
            ),
            (
                Easy,
                Unknown,
                true,
                ReplyState::Code {
                    second_router: false,
                },
            ),
            (Hard, Easy, true, ReplyState::HostHard),
            // A forward on the host helps whatever this router does.
            (Hard, Hard, true, ReplyState::HostHard),
            (Hard, Unknown, false, ReplyState::HostHard),
            (Easy, Hard, true, ReplyState::OwnHard),
            (Unknown, Hard, true, ReplyState::OwnHard),
            (Easy, Unknown, false, ReplyState::NoAddress),
            (Unknown, Unknown, false, ReplyState::NoAddress),
        ] {
            assert_eq!(
                choose(host, false, own, has_address, true),
                want,
                "host {host:?}, own {own:?}, address {has_address}"
            );
        }
        // Behind a second router the code is still the way, with a line more.
        assert_eq!(
            choose(Easy, true, Unknown, true, true),
            ReplyState::Code {
                second_router: true
            }
        );
        assert_eq!(choose(Hard, true, Easy, true, true), ReplyState::HostHard);
        // An invite with no outside address and no name: the host could punch
        // this way, but this PC would not know where to answer, so no code
        // helps whatever the routers do.
        for (host, own) in [(Easy, Easy), (Hard, Easy), (Easy, Hard), (Unknown, Unknown)] {
            assert_eq!(
                choose(host, false, own, true, false),
                ReplyState::HostNoAddress
            );
        }
    }

    fn candidate(kind: CandidateKind, addr: &str) -> invite::Candidate {
        invite::Candidate {
            kind,
            addr: addr.parse().unwrap(),
        }
    }

    #[test]
    fn host_port_from_candidates() {
        let mut invite = Invite {
            host_key: [1; 32],
            invite_id: [2; 8],
            secret: [3; 16],
            multi_use: false,
            expires_at: 1_800_000_000,
            candidates: vec![
                candidate(CandidateKind::Lan, "192.168.1.20:41000"),
                candidate(CandidateKind::Public, "203.0.113.9:52000"),
                candidate(CandidateKind::Public, "203.0.113.9:41000"),
            ],
            mapping: Mapping::Easy,
            mapped: false,
            mapped_verified: false,
            second_router: false,
            hostname: None,
        };
        assert_eq!(host_port(&invite.candidates), Some(41000));
        invite.candidates.remove(0);
        assert_eq!(host_port(&invite.candidates), Some(41000));
        invite.candidates.clear();
        assert_eq!(host_port(&invite.candidates), None);
    }

    #[test]
    fn private_stun_address_left_out() {
        let public = SocketAddrV4::new(Ipv4Addr::new(203, 0, 113, 9), 52000);
        let private = SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 20), 52000);
        let v6: SocketAddrV6 = "[2001:db8::7]:52000".parse().unwrap();
        let answers = Answers::Invite([4; 8]);
        let now = 1_800_000_000;
        let code = build(
            answers,
            [5; 32],
            Some(private),
            Some(v6),
            Mapping::Easy,
            now,
            &Log::off(),
        )
        .expect("the IPv6 address is enough");
        assert_eq!(code.outside_v4, None);
        assert_eq!(code.outside_v6, Some(v6));
        assert_eq!(code.expires_at, now + invite::REPLY_SECS);
        let code = build(
            answers,
            [5; 32],
            Some(public),
            None,
            Mapping::Easy,
            now,
            &Log::off(),
        )
        .expect("a code");
        assert_eq!(code.outside_v4, Some(public));
        assert!(
            build(
                answers,
                [5; 32],
                Some(private),
                None,
                Mapping::Easy,
                now,
                &Log::off()
            )
            .is_none()
        );
        assert!(
            build(
                answers,
                [5; 32],
                None,
                None,
                Mapping::Easy,
                now,
                &Log::off()
            )
            .is_none()
        );
    }

    #[test]
    fn punched_address_held_for_one_key() {
        let start = Instant::now();
        let (ana, bo) = ([1; 32], [2; 32]);
        let at: SocketAddr = "203.0.113.9:52000".parse().unwrap();
        let elsewhere: SocketAddr = "198.51.100.4:52000".parse().unwrap();
        let v6: SocketAddrV6 = "[2001:db8::7]:52000".parse().unwrap();
        let mut punches = Punches::default();
        punches.start(ana, vec![at, v6.into()], Duration::from_secs(60), start);
        assert_eq!(punches.held_for_other(at, &bo, start), Some(ana));
        assert_eq!(punches.held_for_other(at, &ana, start), None);
        assert_eq!(punches.held_for_other(elsewhere, &bo, start), None);
        // A flow label on the packet does not make it another address.
        let labelled = SocketAddrV6::new(*v6.ip(), v6.port(), 0x12345, 0);
        assert_eq!(
            punches.held_for_other(labelled.into(), &bo, start),
            Some(ana)
        );

        // Ana joined; the address stays hers while the code lives.
        assert_eq!(punches.joined(&ana), Some(0));
        let later = start + Duration::from_secs(59);
        assert_eq!(punches.held_for_other(at, &bo, later), Some(ana));
        let over = start + Duration::from_secs(60);
        assert!(punches.expire(over, &Log::off()).is_empty());
        assert_eq!(punches.held_for_other(at, &bo, over), None);
    }

    #[test]
    fn round_line_names_sent_addresses() {
        let (log, captured) = Log::capture(64);
        let socket = Socket::bind(0, Log::off()).expect("bind the host socket");
        let wire = Wire::new();
        // Windows refuses to send to 0.0.0.0.
        let nowhere: SocketAddr = "0.0.0.0:9".parse().unwrap();
        let start = Instant::now();
        let mut punches = Punches::default();
        let (ana, bo) = ([1; 32], [2; 32]);
        let life = Duration::from_secs(60);
        punches.start(ana, vec![wire.addr(), nowhere], life, start);
        punches.start(bo, vec![nowhere], life, start);
        punches.send_due(start, &socket, &log);
        assert_eq!(wire.packets().len(), 1);
        assert_eq!(
            captured.lines(),
            [
                format!(
                    "punch round 1 of 10 for {} sent to {}, could not send to {nowhere}",
                    keys::fingerprint(&ana),
                    wire.addr()
                ),
                format!(
                    "punch round 1 of 10 for {}: could not send to {nowhere}",
                    keys::fingerprint(&bo)
                ),
            ]
        );
    }

    #[test]
    fn long_claimed_life_gets_five_minutes() {
        let start = Instant::now();
        let mut punches = Punches::default();
        let at: SocketAddr = "203.0.113.9:52000".parse().unwrap();
        punches.start([1; 32], vec![at], Duration::from_secs(86_400), start);
        let missed = punches.expire(start + REPLY_LIFETIME, &Log::off());
        assert_eq!(missed, [[1; 32]]);
    }

    #[test]
    fn one_paste_per_key_per_ten_seconds() {
        let start = Instant::now();
        let mut punches = Punches::default();
        let at: SocketAddr = "203.0.113.9:52000".parse().unwrap();
        punches.start([1; 32], vec![at], Duration::from_secs(60), start);
        assert!(punches.too_soon(&[1; 32], start + PASTE_GAP - Duration::from_millis(1)));
        assert!(!punches.too_soon(&[2; 32], start));
        assert!(!punches.too_soon(&[1; 32], start + PASTE_GAP));
    }
}
