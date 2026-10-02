// The STUN questions from the room socket. Both roles ask one round at start:
// the host's first invite waits for it, and the client's reply code carries
// what it saw. The host then asks one every stun_every to keep the router's
// mapping warm; the client's own tries to the host do that for it. Either
// side asks an extra round when its address may have changed, and every
// answer after the first round is held against what came before it, which
// is how a new outside address shows.

use std::fmt;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::time::{Duration, Instant};

use net::stun::{self, Family, Mapping, StunError};

use crate::log::{Log, list, log, secs};
use crate::socket::Socket;
use crate::view::{MappingWord, RouterState};

// A STUN answer takes one round trip. This is what the first invite still
// waits after slow name lookups have used up stun_wait, for the questions
// sent to the names that resolved last.
const LATE_ANSWER: Duration = Duration::from_millis(300);
// How long an address check goes on asking while no server answers.
const CHECK_FOR: Duration = Duration::from_secs(60);

#[derive(Debug)]
pub(crate) struct Stun {
    // None asks the first round only, and address checks.
    every: Option<Duration>,
    wait: Duration,
    retry: Duration,
    // stun_wait counts from when the room opened, not from when the name
    // lookups finished.
    first_by: Instant,
    servers: Vec<SocketAddr>,
    phase: Phase,
    requests: Vec<([u8; 12], SocketAddr)>,
    answers: Vec<(SocketAddr, SocketAddr)>,
    next_round: Option<Instant>,
    // A round after the first: when it went out, and when it stops waiting
    // for the answers still missing.
    round_started: Option<Instant>,
    round_ends: Option<Instant>,
    mapping: Mapping,
    public_v4: Option<SocketAddrV4>,
    public_v6: Option<SocketAddrV6>,
    public: Option<SocketAddr>,
    // What each server saw last. A router with hard mapping shows every
    // server another port, so a new answer is held against the same
    // server's last one.
    seen: Vec<(SocketAddr, SocketAddr)>,
    // When an answer last showed the outside address in each family as it
    // was, or as it had just become.
    confirmed_v4: Option<Instant>,
    confirmed_v6: Option<Instant>,
    // An address check goes on until then, or until every family this PC
    // had an outside address in answers again.
    check_until: Option<Instant>,
    log: Log,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Resolving,
    Waiting(Instant),
    Settled,
}

pub(crate) enum Answer {
    NotOurs(NotOurs),
    Recorded,
    // The last question of the first round was answered.
    Settled,
    // After the first round: the outside address or port is not what it
    // was.
    Moved(Moved),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Moved {
    pub from: SocketAddr,
    pub to: SocketAddr,
    // When an answer last showed `from` still in place, so the change came
    // after it. Only this family's answers count: the other one can come
    // back unchanged while this one was gone.
    pub from_confirmed: Option<Instant>,
}

pub(crate) enum NotOurs {
    Unreadable(StunError),
    // From a server we asked, to no question still open: an answer to a
    // round that is over, or a second copy.
    Late,
    Stranger,
}

impl fmt::Display for NotOurs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NotOurs::Unreadable(err) => {
                write!(f, "stun, but not an answer this pc can use: {err}")
            }
            NotOurs::Late => {
                f.write_str("a stun answer from a server we asked, to no open question")
            }
            NotOurs::Stranger => f.write_str("stun from someone this pc never asked"),
        }
    }
}

impl Stun {
    pub(crate) fn new(
        every: Option<Duration>,
        wait: Duration,
        retry: Duration,
        opened: Instant,
        log: Log,
    ) -> Stun {
        Stun {
            every,
            wait,
            retry,
            first_by: opened + wait,
            servers: Vec::new(),
            phase: Phase::Resolving,
            requests: Vec::new(),
            answers: Vec::new(),
            next_round: None,
            round_started: None,
            round_ends: None,
            mapping: Mapping::Unknown,
            public_v4: None,
            public_v6: None,
            public: None,
            seen: Vec::new(),
            confirmed_v4: None,
            confirmed_v6: None,
            check_until: None,
            log,
        }
    }

    // One server name resolved. Its questions go out now, so the answers
    // come in while the next name is still being looked up.
    pub(crate) fn found(&mut self, servers: Vec<SocketAddr>, socket: &Socket) {
        for server in servers {
            if !self.servers.contains(&server) {
                self.servers.push(server);
                self.ask(server, socket);
            }
        }
    }

    // Every name is looked up. Returns true when that settles it at once:
    // nothing was asked, or everything asked was already answered.
    pub(crate) fn resolved(&mut self, now: Instant) -> bool {
        if self.phase != Phase::Resolving {
            return false;
        }
        self.next_round = self.after_round(now);
        if self.requests.is_empty() {
            self.settle();
            return true;
        }
        let late = now + self.wait.min(LATE_ANSWER);
        self.phase = Phase::Waiting(self.first_by.max(late));
        false
    }

    pub(crate) fn on_answer(&mut self, packet: &[u8], from: SocketAddr, now: Instant) -> Answer {
        let (txid, mapped) = match stun::parse_binding_response(packet) {
            Ok(answer) => answer,
            Err(err) => return Answer::NotOurs(NotOurs::Unreadable(err)),
        };
        let Some(at) = self
            .requests
            .iter()
            .position(|(id, server)| *id == txid && *server == from)
        else {
            return Answer::NotOurs(if self.servers.contains(&from) {
                NotOurs::Late
            } else {
                NotOurs::Stranger
            });
        };
        self.requests.swap_remove(at);
        self.answers.push((from, mapped));
        log!(self.log, "stun answer from {from}: seen as {mapped}");
        let moved = self.held_against_before(from, mapped, now);
        match mapped {
            SocketAddr::V4(v4) => {
                self.public_v4 = Some(v4);
                self.public = Some(mapped);
            }
            SocketAddr::V6(v6) => {
                self.public_v6 = Some(v6);
                if self.public_v4.is_none() {
                    self.public = Some(mapped);
                }
            }
        }
        if self.requests.is_empty() {
            if matches!(self.phase, Phase::Waiting(_)) {
                self.settle();
                return Answer::Settled;
            }
            if self.round_ends.is_some() {
                self.end_round(now);
            }
        }
        // Later than stun_wait, but before the check asked again: still the
        // answer it was asking for, so the retry and the check go.
        if self.round_ends.is_none()
            && self.check_until.is_some()
            && self.every_family_back()
            && self.end_check()
        {
            log!(
                self.log,
                "stun: answered after all, the address check stops"
            );
        }
        match moved {
            Some(moved) => Answer::Moved(moved),
            None => Answer::Recorded,
        }
    }

    // Returns true when the first wait ran out just now.
    pub(crate) fn tick(&mut self, now: Instant, socket: &Socket) -> bool {
        let mut settled = false;
        if let Phase::Waiting(until) = self.phase
            && now >= until
        {
            self.settle();
            settled = true;
        }
        if self.round_ends.is_some_and(|at| now >= at) {
            self.end_round(now);
        }
        if self.next_round.is_some_and(|at| now >= at) {
            self.round(now, socket);
        }
        settled
    }

    // The address may have changed: every server is asked now, and again
    // every stun_retry while a family stays silent, for up to CHECK_FOR. A
    // round already out, or the first one, answers the same question, so
    // nothing more goes out then, but a round out that ends unanswered is
    // tried again all the same. Without servers there is no one to ask.
    // Returns true when a round went out.
    pub(crate) fn check(&mut self, now: Instant, socket: &Socket) -> bool {
        if self.phase != Phase::Settled || self.servers.is_empty() {
            return false;
        }
        self.check_until = Some(now + CHECK_FOR);
        if self.round_ends.is_some() {
            return false;
        }
        self.round(now, socket);
        true
    }

    // What the check was asking has its answer some other way, such as the
    // host being heard again. A round already out can still land and show a
    // move, but nothing more is asked. Returns true when a check was on.
    pub(crate) fn end_check(&mut self) -> bool {
        if self.check_until.take().is_none() {
            return false;
        }
        // The retry goes, and a host is back to its keepalive rounds.
        self.next_round = self
            .round_started
            .and_then(|started| self.after_round(started));
        true
    }

    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        let wait = match self.phase {
            Phase::Waiting(until) => Some(until),
            _ => None,
        };
        [wait, self.round_ends, self.next_round]
            .into_iter()
            .flatten()
            .min()
    }

    pub(crate) fn is_settled(&self) -> bool {
        self.phase == Phase::Settled
    }

    pub(crate) fn router(&self) -> RouterState {
        match (self.phase, self.mapping) {
            (Phase::Settled, Mapping::Easy) => RouterState::Easy,
            (Phase::Settled, Mapping::Hard) => RouterState::Hard,
            (Phase::Settled, Mapping::Unknown) => RouterState::Unknown,
            _ => RouterState::Testing,
        }
    }

    pub(crate) fn mapping_word(&self) -> Option<MappingWord> {
        self.is_settled().then_some(match self.mapping {
            Mapping::Easy => MappingWord::Easy,
            Mapping::Hard => MappingWord::Hard,
            Mapping::Unknown => MappingWord::Unknown,
        })
    }

    pub(crate) fn invite_mapping(&self) -> invite::Mapping {
        invite_mapping(self.mapping)
    }

    pub(crate) fn public(&self) -> Option<SocketAddr> {
        self.public
    }

    pub(crate) fn public_v4_ip(&self) -> Option<Ipv4Addr> {
        self.public_v4.map(|v4| *v4.ip())
    }

    pub(crate) fn public_v4(&self) -> Option<SocketAddrV4> {
        self.public_v4
    }

    pub(crate) fn public_v6(&self) -> Option<SocketAddrV6> {
        self.public_v6
    }

    // What an answer would have recorded, for tests of the host.
    #[cfg(test)]
    pub(crate) fn set_public_v4(&mut self, seen: SocketAddrV4) {
        self.public_v4 = Some(seen);
        self.public = Some(SocketAddr::V4(seen));
    }

    // A finished first round, for tests of the client's reply code.
    #[cfg(test)]
    pub(crate) fn set_settled(&mut self, mapping: Mapping, seen: Option<SocketAddrV4>) {
        self.mapping = mapping;
        self.public_v4 = seen;
        self.public = seen.map(SocketAddr::V4);
        self.phase = Phase::Settled;
    }

    // With hard mapping the router picks a new outside port per destination,
    // so the port STUN saw is wrong for everyone else. A port forwarded by
    // hand keeps its number, while many routers show STUN a different one
    // for the same socket, so the port this socket is bound to goes in on
    // the public address too, whatever the mapping.
    pub(crate) fn public_for_invite(&self, bound_port: u16) -> Vec<SocketAddrV4> {
        let Some(mapped) = self.public_v4 else {
            return Vec::new();
        };
        let forwarded = SocketAddrV4::new(*mapped.ip(), bound_port);
        let mut out = Vec::with_capacity(2);
        if self.mapping != Mapping::Hard {
            out.push(mapped);
        }
        // A hard-mapping router can keep the port for one destination, and
        // that answer may be the one kept here: forwarded is still wanted.
        if !out.contains(&forwarded) {
            out.push(forwarded);
        }
        out
    }

    fn settle(&mut self) {
        self.mapping = stun::classify(&self.answers, Family::Ipv4);
        self.phase = Phase::Settled;
        if self.log.is_on() {
            self.note_round("first stun round", self.mapping);
        }
    }

    // A later round is written down when its last answer comes in, or
    // stun_wait after its questions went out if some never do, the same way
    // settle() does it for the first round.
    fn round(&mut self, now: Instant, socket: &Socket) {
        // Only when stun_wait is longer than stun_every.
        if self.round_ends.is_some() {
            self.end_round(now);
        }
        self.requests.clear();
        self.answers.clear();
        for server in self.servers.clone() {
            self.ask(server, socket);
        }
        self.next_round = self.after_round(now);
        self.round_started = Some(now);
        if self.requests.is_empty() {
            // Not one question left this PC: it has no network right now.
            self.retry_check(now);
        } else {
            self.round_ends = Some(now + self.wait);
        }
    }

    fn after_round(&self, now: Instant) -> Option<Instant> {
        let every = self.every.filter(|_| !self.servers.is_empty())?;
        Some(now + every)
    }

    fn end_round(&mut self, now: Instant) {
        self.round_ends = None;
        if self.log.is_on() {
            let mapping = stun::classify(&self.answers, Family::Ipv4);
            self.note_round("stun round", mapping);
        }
        if self.check_until.is_some() {
            if self.every_family_back() {
                self.check_until = None;
            } else {
                self.retry_check(now);
            }
        }
    }

    // A router coming back can bring IPv6 back before IPv4, or the other way
    // round, so an address check waits for every family a server asked in
    // this round gave an outside address in before. A family no question
    // could leave this PC for has nothing to wait for.
    fn every_family_back(&self) -> bool {
        let asked = self
            .answers
            .iter()
            .map(|(server, _)| server)
            .chain(self.requests.iter().map(|(_, server)| server));
        // IPv4, then IPv6.
        let mut waiting = [false; 2];
        for server in asked {
            if let Some((_, seen)) = self.seen.iter().find(|(known, _)| known == server) {
                waiting[usize::from(seen.is_ipv6())] = true;
            }
        }
        for (_, mapped) in &self.answers {
            waiting[usize::from(mapped.is_ipv6())] = false;
        }
        !self.answers.is_empty() && waiting == [false; 2]
    }

    // A round of an address check ended with a family still silent.
    fn retry_check(&mut self, now: Instant) {
        let Some(until) = self.check_until else {
            return;
        };
        let at = self
            .round_started
            .map_or(now, |started| started + self.retry)
            .max(now);
        let silent = if self.answers.is_empty() {
            "no server answered"
        } else {
            "some servers answered, but not in every family this pc had an outside address in"
        };
        if at >= until {
            log!(
                self.log,
                "stun: {silent} for {}, the address check stops",
                secs(CHECK_FOR)
            );
            self.check_until = None;
            return;
        }
        // A log that ends after an unanswered round would otherwise read as
        // if nothing more was asked.
        log!(
            self.log,
            "stun: {silent}, asking again in {}",
            secs(at - now)
        );
        self.next_round = Some(self.next_round.map_or(at, |next| next.min(at)));
    }

    fn ask(&mut self, server: SocketAddr, socket: &Socket) {
        let txid = stun::new_transaction_id();
        if socket
            .send_to(&stun::binding_request(&txid), server)
            .is_ok()
        {
            log!(self.log, "stun request sent to {server}");
            self.requests.push((txid, server));
        }
    }

    // Some when the answer shows another outside address or port than the
    // last answer did, and the server that gave it saw something else last
    // time too, so one change is one move however many servers show it. A
    // server with nothing to go by counts only for another address. With
    // hard mapping every server sees its own port, so only the address
    // counts there. The first round is only written down: there is nothing
    // before it to hold it against.
    fn held_against_before(
        &mut self,
        server: SocketAddr,
        now: SocketAddr,
        at: Instant,
    ) -> Option<Moved> {
        let (last, confirmed) = match now {
            SocketAddr::V4(_) => (self.public_v4.map(SocketAddr::V4), &mut self.confirmed_v4),
            SocketAddr::V6(_) => (self.public_v6.map(SocketAddr::V6), &mut self.confirmed_v6),
        };
        let from_confirmed = confirmed.replace(at);
        let history = match self.seen.iter_mut().find(|(asked, _)| *asked == server) {
            Some((_, seen)) if !same_place(*seen, now) => {
                let before = *seen;
                *seen = now;
                log!(
                    self.log,
                    "stun {server} saw {before} last time and {now} now: the router changed the outside address or port"
                );
                Some(true)
            }
            Some(_) => Some(false),
            None => {
                self.seen.push((server, now));
                None
            }
        };
        let last = last.filter(|_| self.phase == Phase::Settled)?;
        let ports_count = now.is_ipv6() || self.mapping != Mapping::Hard;
        let other_address = last.ip() != now.ip();
        let other_port = ports_count && last.port() != now.port();
        let moved = match history {
            Some(changed) => changed && (other_address || other_port),
            None => other_address,
        };
        moved.then_some(Moved {
            from: last,
            to: now,
            from_confirmed,
        })
    }

    fn note_round(&self, what: &str, mapping: Mapping) {
        let public = self
            .public_v4
            .map_or_else(|| String::from("none"), |public| public.to_string());
        log!(
            self.log,
            "{what}: mapping {}, public {public}, {} answers from {} servers asked",
            crate::log::mapping_word(invite_mapping(mapping)),
            self.answers.len(),
            self.servers.len()
        );
        if !self.requests.is_empty() {
            let silent: Vec<SocketAddr> = self.requests.iter().map(|(_, server)| *server).collect();
            log!(self.log, "{what}: no answer from {}", list(&silent));
        }
    }
}

// A received IPv6 address can carry a flow label or scope the answer's
// place does not depend on.
fn same_place(a: SocketAddr, b: SocketAddr) -> bool {
    a.ip() == b.ip() && a.port() == b.port()
}

fn invite_mapping(mapping: Mapping) -> invite::Mapping {
    match mapping {
        Mapping::Easy => invite::Mapping::Easy,
        Mapping::Hard => invite::Mapping::Hard,
        Mapping::Unknown => invite::Mapping::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{Wire, stun_answer_seeing};
    use std::net::Ipv4Addr;

    const BOUND: u16 = 41000;
    // What Timers::default() has.
    const RETRY: Duration = Duration::from_secs(2);

    fn outside(port: u16) -> SocketAddrV4 {
        SocketAddrV4::new(Ipv4Addr::new(203, 0, 113, 9), port)
    }

    fn settled(mapping: Mapping, public: Option<SocketAddrV4>) -> Stun {
        let now = Instant::now();
        let mut stun = Stun::new(
            Some(Duration::from_secs(20)),
            Duration::ZERO,
            RETRY,
            now,
            Log::off(),
        );
        stun.mapping = mapping;
        stun.public_v4 = public;
        stun.phase = Phase::Settled;
        stun
    }

    #[test]
    fn invite_gets_forwarded_port() {
        let mapped = outside(52000);
        let forwarded = outside(BOUND);
        for (mapping, public, want) in [
            (Mapping::Easy, mapped, vec![mapped, forwarded]),
            (Mapping::Unknown, mapped, vec![mapped, forwarded]),
            (Mapping::Hard, mapped, vec![forwarded]),
            // The router kept the port, so both are the same address.
            (Mapping::Easy, forwarded, vec![forwarded]),
            (Mapping::Unknown, forwarded, vec![forwarded]),
            // It kept it for the server that answered last and not for the
            // other one.
            (Mapping::Hard, forwarded, vec![forwarded]),
        ] {
            let stun = settled(mapping, Some(public));
            assert_eq!(
                stun.public_for_invite(BOUND),
                want,
                "{mapping:?} seen as {public}"
            );
        }
        assert!(
            settled(Mapping::Unknown, None)
                .public_for_invite(BOUND)
                .is_empty()
        );
    }

    fn answer_from(stun: &mut Stun, server: SocketAddr) {
        assert!(!matches!(
            answer_with(stun, server, outside(52000), Instant::now()),
            Answer::NotOurs(_)
        ));
    }

    fn answer_with(
        stun: &mut Stun,
        server: SocketAddr,
        seen: SocketAddrV4,
        now: Instant,
    ) -> Answer {
        answer_seeing(stun, server, SocketAddr::V4(seen), now)
    }

    fn answer_seeing(
        stun: &mut Stun,
        server: SocketAddr,
        seen: SocketAddr,
        now: Instant,
    ) -> Answer {
        let (txid, _) = *stun
            .requests
            .iter()
            .find(|(_, asked)| *asked == server)
            .expect("a question to answer");
        stun.on_answer(&stun_answer_seeing(&txid, seen), server, now)
    }

    fn moved(answer: Answer) -> Option<Moved> {
        match answer {
            Answer::Moved(moved) => Some(moved),
            _ => None,
        }
    }

    // The client's side: the first round and then checks only.
    fn settled_on(server: &Wire, socket: &Socket, start: Instant) -> Stun {
        let mut stun = Stun::new(None, Duration::from_millis(100), RETRY, start, Log::off());
        stun.found(vec![server.addr()], socket);
        assert!(!stun.resolved(start));
        assert!(matches!(
            answer_with(&mut stun, server.addr(), outside(52000), start),
            Answer::Settled
        ));
        stun
    }

    #[test]
    fn check_retries_until_answer() {
        let socket = Socket::bind(0, Log::off()).expect("bind");
        let server = Wire::new();
        let start = Instant::now();
        let mut stun = settled_on(&server, &socket, start);
        assert_eq!(stun.next_deadline(), None);
        server.packets();

        let noticed = start + Duration::from_secs(10);
        assert!(stun.check(noticed, &socket));
        assert!(
            !stun.check(noticed, &socket),
            "a second round while one is out"
        );
        assert_eq!(server.packets().len(), 1);

        // The router is still coming back: nothing answers.
        let wait = Duration::from_millis(100);
        stun.tick(noticed + wait, &socket);
        assert_eq!(stun.next_deadline(), Some(noticed + RETRY));
        stun.tick(noticed + RETRY, &socket);
        assert_eq!(server.packets().len(), 1, "no second question");

        // Then it does, from its new outside port.
        let answered = noticed + RETRY + Duration::from_millis(20);
        let shown = moved(answer_with(
            &mut stun,
            server.addr(),
            outside(52001),
            answered,
        ));
        assert_eq!(
            shown,
            Some(Moved {
                from: SocketAddr::V4(outside(52000)),
                to: SocketAddr::V4(outside(52001)),
                from_confirmed: Some(start),
            })
        );
        assert_eq!(stun.public_v4(), Some(outside(52001)));
        assert_eq!(stun.next_deadline(), None, "the check is over");

        // Held against the new one from now on.
        assert!(stun.check(answered, &socket));
        let same = answer_with(&mut stun, server.addr(), outside(52001), answered);
        assert!(matches!(same, Answer::Recorded));
    }

    // The answer to a round comes after its stun_wait, but before the check
    // asks again. It is the answer the check was waiting for, so the retry
    // goes, and a host is back to its keepalive rounds.
    #[test]
    fn a_late_answer_ends_the_check() {
        let every = Duration::from_secs(20);
        for keepalive in [None, Some(every)] {
            let socket = Socket::bind(0, Log::off()).expect("bind");
            let server = Wire::new();
            let start = Instant::now();
            let wait = Duration::from_millis(100);
            let (log, captured) = Log::capture(64);
            let mut stun = Stun::new(keepalive, wait, RETRY, start, log);
            stun.found(vec![server.addr()], &socket);
            assert!(!stun.resolved(start));
            answer_with(&mut stun, server.addr(), outside(52000), start);
            assert!(stun.is_settled());
            server.packets();

            let noticed = start + Duration::from_secs(10);
            assert!(stun.check(noticed, &socket));
            assert_eq!(server.packets().len(), 1);
            stun.tick(noticed + wait, &socket);
            assert_eq!(stun.next_deadline(), Some(noticed + RETRY));

            let late = noticed + wait * 3;
            let shown = moved(answer_with(&mut stun, server.addr(), outside(52001), late));
            assert_eq!(
                shown.map(|moved| moved.to),
                Some(SocketAddr::V4(outside(52001)))
            );
            let next = keepalive.map(|every| noticed + every);
            assert_eq!(stun.next_deadline(), next, "keepalive {keepalive:?}");
            stun.tick(noticed + RETRY, &socket);
            assert!(server.packets().is_empty(), "asked again after the answer");
            let lines = captured.lines();
            let asked = lines
                .iter()
                .position(|line| line.starts_with("stun: no server answered, asking again in "));
            let stopped = lines
                .iter()
                .position(|line| line == "stun: answered after all, the address check stops");
            assert!(
                asked.is_some() && stopped > asked,
                "keepalive {keepalive:?}: {lines:#?}"
            );
        }
    }

    #[test]
    fn a_check_stops_after_a_minute_of_silence() {
        let socket = Socket::bind(0, Log::off()).expect("bind");
        let server = Wire::new();
        let start = Instant::now();
        let mut stun = settled_on(&server, &socket, start);
        assert!(stun.check(start, &socket));
        let mut now = start;
        let mut rounds = 1;
        while let Some(next) = stun.next_deadline() {
            assert!(next > now, "the deadline stands still");
            now = next;
            stun.tick(now, &socket);
            if server.packets().len() == 1 {
                rounds += 1;
            }
        }
        assert!(now < start + CHECK_FOR);
        assert_eq!(rounds, 30);
    }

    // Every friend went quiet just after a keepalive round went out, into a
    // router that had already dropped. That round stands for the check, and
    // when it goes unanswered the check asks again like any other.
    #[test]
    fn check_during_round_retries() {
        let socket = Socket::bind(0, Log::off()).expect("bind");
        let server = Wire::new();
        let start = Instant::now();
        let every = Duration::from_secs(20);
        let wait = Duration::from_millis(1500);
        let mut stun = Stun::new(Some(every), wait, RETRY, start, Log::off());
        stun.found(vec![server.addr()], &socket);
        assert!(!stun.resolved(start));
        answer_with(&mut stun, server.addr(), outside(52000), start);
        assert!(stun.is_settled());
        server.packets();

        let keepalive = start + every;
        stun.tick(keepalive, &socket);
        assert_eq!(server.packets().len(), 1);
        let noticed = keepalive + Duration::from_millis(500);
        assert!(!stun.check(noticed, &socket));
        assert!(server.packets().is_empty(), "the round out answers it");

        stun.tick(keepalive + wait, &socket);
        assert_eq!(stun.next_deadline(), Some(keepalive + RETRY));
        stun.tick(keepalive + RETRY, &socket);
        assert_eq!(server.packets().len(), 1, "asked again");
    }

    // A dual-stack router comes back with IPv6 first. The check goes on
    // until IPv4 answers too, which is where the new address shows.
    #[test]
    fn check_waits_for_every_family() {
        let socket = Socket::bind(0, Log::off()).expect("bind");
        let (v4, v6) = (Wire::new(), Wire::new());
        let start = Instant::now();
        let wait = Duration::from_millis(100);
        let mut stun = Stun::new(None, wait, RETRY, start, Log::off());
        stun.found(vec![v4.addr(), v6.addr()], &socket);
        assert!(!stun.resolved(start));
        let outside_v6: SocketAddr = "[2001:db8::7]:52000".parse().unwrap();
        answer_with(&mut stun, v4.addr(), outside(52000), start);
        let settled = answer_seeing(&mut stun, v6.addr(), outside_v6, start);
        assert!(matches!(settled, Answer::Settled));

        let noticed = start + Duration::from_secs(10);
        assert!(stun.check(noticed, &socket));
        let back = noticed + Duration::from_millis(20);
        let same = answer_seeing(&mut stun, v6.addr(), outside_v6, back);
        assert!(matches!(same, Answer::Recorded));
        stun.tick(noticed + wait, &socket);
        assert_eq!(
            stun.next_deadline(),
            Some(noticed + RETRY),
            "IPv4 is still silent"
        );

        let retried = noticed + RETRY;
        stun.tick(retried, &socket);
        let shown = moved(answer_with(&mut stun, v4.addr(), outside(52001), retried));
        assert_eq!(
            shown,
            Some(Moved {
                from: SocketAddr::V4(outside(52000)),
                to: SocketAddr::V4(outside(52001)),
                from_confirmed: Some(start),
            }),
            "held against the last IPv4 answer, not the IPv6 one"
        );
        answer_seeing(&mut stun, v6.addr(), outside_v6, retried);
        assert_eq!(stun.next_deadline(), None, "the check is over");
    }

    // With hard mapping every server sees another port, and that is no move.
    #[test]
    fn each_server_against_itself() {
        let socket = Socket::bind(0, Log::off()).expect("bind");
        let (one, two) = (Wire::new(), Wire::new());
        let start = Instant::now();
        let mut stun = Stun::new(None, Duration::from_millis(100), RETRY, start, Log::off());
        stun.found(vec![one.addr(), two.addr()], &socket);
        assert!(!stun.resolved(start));
        answer_with(&mut stun, one.addr(), outside(52000), start);
        answer_with(&mut stun, two.addr(), outside(52007), start);
        assert!(stun.is_settled());

        assert!(stun.check(start, &socket));
        let first = answer_with(&mut stun, one.addr(), outside(52000), start);
        let second = answer_with(&mut stun, two.addr(), outside(52007), start);
        assert!(matches!(
            (first, second),
            (Answer::Recorded, Answer::Recorded)
        ));

        // A new address shows on whichever server answers first.
        let elsewhere = SocketAddrV4::new(Ipv4Addr::new(198, 51, 100, 4), 52007);
        assert!(stun.check(start, &socket));
        let shown = moved(answer_with(&mut stun, two.addr(), elsewhere, start));
        assert_eq!(shown.map(|moved| moved.to), Some(SocketAddr::V4(elsewhere)));
        let later = answer_with(
            &mut stun,
            one.addr(),
            SocketAddrV4::new(*elsewhere.ip(), 52000),
            start,
        );
        assert!(
            matches!(later, Answer::Recorded),
            "one move, not one per server"
        );
    }

    #[test]
    fn new_port_is_one_move() {
        let socket = Socket::bind(0, Log::off()).expect("bind");
        let (one, two) = (Wire::new(), Wire::new());
        let start = Instant::now();
        let mut stun = Stun::new(None, Duration::from_millis(100), RETRY, start, Log::off());
        stun.found(vec![one.addr(), two.addr()], &socket);
        assert!(!stun.resolved(start));
        answer_with(&mut stun, one.addr(), outside(52000), start);
        answer_with(&mut stun, two.addr(), outside(52000), start);
        assert_ne!(stun.mapping, Mapping::Hard);

        assert!(stun.check(start, &socket));
        let first = moved(answer_with(&mut stun, one.addr(), outside(52001), start));
        assert_eq!(
            first,
            Some(Moved {
                from: SocketAddr::V4(outside(52000)),
                to: SocketAddr::V4(outside(52001)),
                from_confirmed: Some(start),
            })
        );
        let second = answer_with(&mut stun, two.addr(), outside(52001), start);
        assert!(matches!(second, Answer::Recorded));
    }

    fn rounds(captured: &crate::log::Captured) -> Vec<String> {
        captured
            .lines()
            .into_iter()
            .filter(|line| line.contains("stun round"))
            .collect()
    }

    // Each round is written down when it ends, not when the next one starts.
    #[test]
    fn keepalive_round_logged_at_end() {
        let (log, captured) = Log::capture(256);
        let socket = Socket::bind(0, Log::off()).expect("bind");
        let (one, two) = (Wire::new(), Wire::new());
        let every = Duration::from_secs(20);
        let wait = Duration::from_millis(1500);
        let start = Instant::now();
        let mut stun = Stun::new(Some(every), wait, RETRY, start, log);
        stun.found(vec![one.addr(), two.addr()], &socket);
        assert!(!stun.resolved(start));
        answer_from(&mut stun, one.addr());
        answer_from(&mut stun, two.addr());
        assert!(stun.is_settled());
        assert_eq!(
            rounds(&captured),
            [
                "first stun round: mapping unknown, public 203.0.113.9:52000, 2 answers from 2 servers asked"
            ]
        );

        // Starting the next round repeats nothing about the first.
        let second = start + every;
        stun.tick(second, &socket);
        assert!(rounds(&captured).is_empty());
        answer_from(&mut stun, one.addr());
        answer_from(&mut stun, two.addr());
        assert_eq!(
            rounds(&captured),
            [
                "stun round: mapping unknown, public 203.0.113.9:52000, 2 answers from 2 servers asked"
            ]
        );
        assert_eq!(stun.next_deadline(), Some(second + every));

        // One server stays silent: the round is written down at stun_wait.
        let third = second + every;
        stun.tick(third, &socket);
        answer_from(&mut stun, one.addr());
        assert!(rounds(&captured).is_empty());
        assert_eq!(stun.next_deadline(), Some(third + wait));
        stun.tick(third + wait, &socket);
        assert_eq!(
            rounds(&captured),
            [
                "stun round: mapping unknown, public 203.0.113.9:52000, 1 answers from 2 servers asked"
                    .to_owned(),
                format!("stun round: no answer from {}", two.addr()),
            ]
        );
        assert_eq!(stun.next_deadline(), Some(third + every));
    }
}
