// The client: the handshake ladder, rekeys, pings, and following the host
// through silence and address changes.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use channels::{Channel, PingMessage};
use crossbeam_channel::Sender;
use invite::{Answers, Candidate, CandidateKind, Invite, Mapping, Version};
use keys::Identity;
use session::{InitKind, Initiation, PacketType, Tai64N, TimestampSource};
use stats::Thresholds;
use zeroize::{Zeroize, Zeroizing};

use crate::Soonest;
use crate::chat::{self, ChatMessage, ChatRefused, Delivery, History};
use crate::config::{Lookup, Timers};
use crate::control::{self, LossPermille, Message, Periods, Roster};
use crate::cookies::{COOKIE_KEPT, Jar};
use crate::known::{self, Entry, HostBook, KnownHost, ListProblem, Save};
use crate::log::{
    self, Log, PerSource, counted, init_word, list, log, mapping_word, path_text, secs, yes_no,
};
use crate::names::{AddressName, Outcome, Ports, Request};
use crate::numbers::{self, LinkNumbers};
use crate::peer::{self, Clock, Drops, Link, MediaPath, Opened, Plain, Sessions, Traffic, Which};
use crate::reply;
use crate::screen::{Screen, ScreenSetup, Sharing, Watching};
use crate::socket::Socket;
use crate::stun::{Answer, Moved, Stun};
use crate::talk::{self, Outlet, Sent, Talk, ToSpeaker};
use crate::view::{
    AddressChange, ChatLine, LineKind, LinkState, Notice, Numbers, PathWord, Person, ReplyState,
    ReplyView, Role, View,
};

mod remote;
mod sharing;

// 5 s is when a person gives up waiting, so that is when the ladder slows
// down.
const FAST_PHASE: Duration = Duration::from_secs(5);
const REPLY_LIFETIME: Duration = Duration::from_secs(invite::REPLY_SECS);
// An answer to a try older than this is not waited for.
const TRIES_KEPT: Duration = Duration::from_secs(3);
// Also how long a rekey may go unconfirmed: the host answers on the new
// session within a round trip of our first ping on it, unless it lost the
// session it was holding for us.
const REKEY_RETRY: Duration = Duration::from_secs(1);
const SUMMARY_EVERY: Duration = Duration::from_secs(5);
// While the host is silent its address name is looked up this often, for up
// to NAME_FOR, in case its address changed.
const NAME_EVERY: Duration = Duration::from_secs(2);
const NAME_FOR: Duration = Duration::from_secs(60);

struct Attempt {
    initiation: Initiation,
    kind: InitKind,
    // The timer pass that made it, which is what TRIES_KEPT counts from.
    tried_at: Instant,
    // Read after our own key math, right before sending, so handshake_ms is
    // the network and the host and nothing on this PC.
    sent_at: Instant,
}

struct Reply {
    state: ReplyState,
    code: String,
    expires_at_unix: u64,
    // When a code runs out on this PC's clock. None once it has, and for the
    // screens without a code.
    expires: Option<Instant>,
    // A no-code screen made while STUN was still out is made again when it
    // finishes: a late answer can still give the address a code needs.
    stun_finished: bool,
}

// What a client starts from: an invite, or the record of a host it joined
// before, which needs no invite at all.
pub(crate) enum Ticket<'a> {
    Invite(&'a Invite),
    Known(&'a KnownHost),
}

pub(crate) struct ClientSetup<'a> {
    pub identity: Arc<Identity>,
    pub name: String,
    pub ticket: Ticket<'a>,
    pub timers: Timers,
    pub port: u16,
    pub has_ipv6: bool,
    pub lookup: Lookup,
    pub joined: Instant,
    // Read from hosts.bin when the room opened.
    pub hosts: HostBook,
    pub list_problem: Option<ListProblem>,
    // Voice: what the audio threads share with the room, and the way to the
    // render thread's mixer.
    pub voice: Arc<talk::Shared>,
    pub speaker: Sender<ToSpeaker>,
    pub screen: ScreenSetup,
    pub log: Log,
}

// The invite's part of a first join.
struct InviteUse {
    id: [u8; 8],
    psk: Zeroizing<[u8; 32]>,
    expires_at: u64,
}

// What this host's record is made of once it gives a secret.
struct Stored {
    candidates: Vec<Candidate>,
    address_name: Option<String>,
    // An invite's addresses replace what a record held; a rejoin's are the
    // record's own.
    from_invite: bool,
}

// The ladder a ticket sets up.
struct Start {
    host_key: [u8; 32],
    // What the panel calls the room until the host says, on a rejoin.
    room_name: String,
    invite: Option<InviteUse>,
    secret: Option<Zeroizing<[u8; 32]>>,
    // Tried at once, in this order.
    candidates: Vec<SocketAddr>,
    // What the name's addresses are paired with, and the forward port.
    ports_from: Vec<Candidate>,
    also_port: Option<u16>,
    address_name: Option<String>,
    // A name typed in by hand is looked up at once; the invite's or the
    // record's only once the fast round has gone unanswered.
    name_now: bool,
    mapped_addr: Option<SocketAddr>,
    mapped_verified: bool,
    host_mapping: Mapping,
    second_router: bool,
    stored: Stored,
}

fn from_invite(identity: &Identity, invite: &Invite, has_ipv6: bool, log: &Log) -> Start {
    let candidates = usable_here(invite.candidates.iter().map(|c| c.addr), has_ipv6);
    if log.is_on() {
        let uses = if invite.multi_use {
            "multi use"
        } else {
            "single use"
        };
        log!(
            log,
            "joining host {}: {uses} invite, expires {}, mapping {}, mapped {}, verified {}, second router {}, {} candidates: {}",
            keys::fingerprint(&invite.host_key),
            log::utc(invite.expires_at),
            mapping_word(invite.mapping),
            yes_no(invite.mapped),
            yes_no(invite.mapped_verified),
            yes_no(invite.second_router),
            invite.candidates.len(),
            log::candidates(&invite.candidates)
        );
        if candidates.len() < invite.candidates.len() {
            log!(
                log,
                "trying {} of them: {}",
                candidates.len(),
                list(&candidates)
            );
        }
    }
    Start {
        host_key: invite.host_key,
        room_name: String::new(),
        invite: Some(InviteUse {
            id: invite.invite_id,
            psk: session::invite_psk(&invite.secret, &invite.host_key, identity.public()),
            expires_at: invite.expires_at,
        }),
        secret: None,
        candidates,
        ports_from: invite.candidates.clone(),
        also_port: None,
        address_name: invite.hostname.clone(),
        name_now: false,
        // The mapped address goes first among the outside ones.
        mapped_addr: invite
            .candidates
            .iter()
            .find(|c| c.kind == CandidateKind::Public)
            .map(|c| c.addr)
            .filter(|_| invite.mapped),
        mapped_verified: invite.mapped && invite.mapped_verified,
        host_mapping: invite.mapping,
        second_router: invite.second_router,
        stored: Stored {
            candidates: known::clean_candidates(&invite.candidates),
            address_name: invite.hostname.clone(),
            from_invite: true,
        },
    }
}

// The same ladder as a join from an invite, with the record in place of the
// invite: the address typed in by hand, where the host was reached last, what
// it said its addresses are, and then its address name. The reply code step
// names this PC's key instead of an invite.
fn from_known(known: &KnownHost, has_ipv6: bool, log: &Log) -> Start {
    let (typed_addr, typed_name) = match known.manual.as_ref().map(|manual| &manual.0) {
        Some(Entry::Addr(addr)) => (Some(*addr), None),
        Some(Entry::Name(name)) => (None, Some(name.clone())),
        None => (None, None),
    };
    let first = typed_addr.into_iter().chain(known.last_reached);
    let candidates = usable_here(
        first.chain(known.candidates.iter().map(|c| c.addr)),
        has_ipv6,
    );
    let name_now = typed_name.is_some();
    // A name typed in by hand says where the host is now; the one it gave
    // earlier may be the reason it was typed.
    let address_name = typed_name.or_else(|| known.address_name.clone());
    if log.is_on() {
        log!(
            log,
            "rejoining known host {} without an invite, {} to try: {}",
            keys::fingerprint(&known.host_key),
            counted(candidates.len() as u64, "address", "addresses"),
            list(&candidates)
        );
        if let Some(manual) = &known.manual {
            log!(log, "typed in for it: {manual}");
        }
    }
    Start {
        host_key: known.host_key,
        room_name: known.room_name.clone(),
        invite: None,
        secret: Some(known.secret.clone()),
        candidates,
        ports_from: known.candidates.clone(),
        also_port: known.last_reached.map(|addr| addr.port()),
        address_name,
        name_now,
        mapped_addr: None,
        mapped_verified: false,
        host_mapping: Mapping::Unknown,
        second_router: false,
        stored: Stored {
            candidates: known.candidates.clone(),
            address_name: known.address_name.clone(),
            from_invite: false,
        },
    }
}

// Each address once, and IPv6 only on a PC that can send it.
fn usable_here(addrs: impl Iterator<Item = SocketAddr>, has_ipv6: bool) -> Vec<SocketAddr> {
    let mut out: Vec<SocketAddr> = Vec::new();
    for addr in addrs.filter(|addr| has_ipv6 || addr.is_ipv4()) {
        if !out.contains(&addr) {
            out.push(addr);
        }
    }
    out
}

pub(crate) struct Client {
    identity: Arc<Identity>,
    name: String,
    host_key: [u8; 32],
    // None on a rejoin, which has the per-peer secret from the start.
    invite: Option<InviteUse>,
    // The invite's addresses, or the known host's.
    candidates: Vec<SocketAddr>,
    has_ipv6: bool,
    // The invite's address name or the known host's, looked up when the
    // fast round goes unanswered and while the host is silent, the ports to
    // try what it gives on, and where its last answer said to try.
    address_name: Option<AddressName>,
    name_ports: Ports,
    from_name: Vec<SocketAddr>,
    // Looking the name up again while the host is silent: the next time,
    // and until when.
    next_name: Option<Instant>,
    name_until: Option<Instant>,
    // The name gave an address not tried before in this silence, first
    // tried then.
    name_new_since: Option<Instant>,
    // From the invite: the address the host's router mapped, and whether a
    // friend came in through it. The host takes this PC back through it
    // after an address change, so no code is needed then.
    mapped_addr: Option<SocketAddr>,
    mapped_verified: bool,
    reached_mapped: bool,
    timers: Timers,
    session_timers: session::Timers,
    thresholds: Thresholds,
    clock: Clock,
    port: u16,
    joined: Instant,

    state: LinkState,
    notice: Option<Notice>,
    // Where control and pings go: where the host was last heard from. Voice
    // goes where `media` says.
    host_addr: Option<SocketAddr>,
    media: Option<MediaPath>,
    path: Option<PathWord>,
    sessions: Sessions,
    // Set once the host has sent anything on the current session. Until then
    // the host may not have taken the session at all.
    heard_on_session: bool,
    // The host's Hello on this link has come and its protocol is this PC's.
    // A numbered host sends it before anything else on every new link.
    host_hello: bool,
    last_heard: Instant,
    // The silence under way: when the host was last heard before it.
    silent_since: Option<Instant>,
    // STUN says this PC's own outside address changed at this time, on the
    // ping clock, and no pong has come back yet for a ping sent after it.
    // Packets from the host alone prove nothing: with a second network
    // plugged in, they can still come in on the old one while everything
    // this PC sends leaves by the new one.
    own_moved: Option<u64>,
    // The reply code on show is the block above the people list.
    reply_in_room: bool,
    address_change: Option<AddressChange>,
    reconnect_ms: Option<f32>,

    attempts: Vec<Attempt>,
    next_attempt: Option<Instant>,
    fast_until: Option<Instant>,
    // When Notice::StillTrying shows if nothing answers before then.
    still_trying_at: Option<Instant>,
    secret: Option<Zeroizing<[u8; 32]>>,
    stamps: TimestampSource,
    // What a host under load sent back, for the tries that follow.
    cookies: Jar,
    // What this PC's own router looks like from outside, for the reply code.
    stun: Stun,
    // From the invite, for the reply code screen.
    host_mapping: Mapping,
    second_router: bool,
    host_port: Option<u16>,
    reply: Option<Reply>,
    // The code is due but STUN has not finished: it goes out at this time
    // with whatever STUN has by then.
    reply_wait: Option<Instant>,
    // The addresses the host's punch packets can come from, and whether the
    // first one was written down.
    punch_sources: Vec<IpAddr>,
    punch_seen: bool,

    link: Link,
    traffic: Traffic,
    roster: Roster,
    handshake_ms: Option<f32>,
    connect_ms: Option<f32>,
    rekeys: u32,
    drops: Drops,
    scratch: Vec<u8>,

    log: Log,
    strays: PerSource,
    // Ladder rounds since the last answer, and when the log next says that
    // nothing has answered yet.
    tries: u32,
    next_summary: Option<Instant>,
    // An answer came in and the first pong on its session has not.
    connecting: bool,

    // Every host this PC knows, this one among them once it gives a secret.
    hosts: HostBook,
    stored: Stored,
    list_problem: Option<ListProblem>,

    // This PC's own lines as it wrote them, everyone else's in the order
    // the host handed them on.
    history: History,
    delivery: Delivery,
    talk: Talk,
    // What the capture thread sent to the host.
    voice_out: Arc<Sent>,
    // What the host said of its audio, and what it was last told of this
    // PC's.
    host_periods: Option<Periods>,
    told_periods: Option<Periods>,
    // The link's jitter is past 5 ms, so a time through the clock offset is
    // only about right. Worked out once a timer pass.
    jittery: bool,
    screen: Screen,
    // What this PC's share sent to the host, from the sharer's thread.
    video_out: Arc<Sent>,
    // The host's word on the clock of the friend whose share this PC
    // watches: the share, the friend's clock minus the host's, and "about".
    sharer_clock: Option<(u32, i64, bool)>,
    // The frame rate the host has for this PC's share, which its roster
    // shows: the one asked with, then what the share's thread said it runs
    // at. Kept here because the roster copy can be a step behind.
    told_fps: Option<(u32, u8)>,
    // An input packet's events on their way to this PC's injector, kept so
    // taking one allocates nothing.
    input_events: Vec<crate::remote::InputEvent>,
}

// Initiation::start refuses a weak host key, so a dry run tells join() before
// anything is bound or sent.
pub(crate) fn host_key_usable(identity: &Identity, host_key: &[u8; 32]) -> bool {
    Initiation::start(
        &identity.private_bytes(),
        identity.public(),
        host_key,
        &[0; 32],
        InitKind::Known,
        Tai64N::now(),
        1,
    )
    .is_ok()
}

impl Client {
    pub(crate) fn new(setup: ClientSetup) -> Client {
        let start = match setup.ticket {
            Ticket::Invite(invite) => {
                from_invite(&setup.identity, invite, setup.has_ipv6, &setup.log)
            }
            Ticket::Known(known) => from_known(known, setup.has_ipv6, &setup.log),
        };
        if let Some(name) = &start.address_name {
            let whose = if start.invite.is_some() {
                "the invite carries"
            } else {
                "the known host has"
            };
            if start.name_now {
                log!(
                    setup.log,
                    "address name: looking up {name}, typed in for this host"
                );
            } else {
                log!(
                    setup.log,
                    "{whose} the address name {name}, looked up if nothing answers in {}",
                    secs(setup.timers.still_trying_after)
                );
            }
        }
        let address_name = start.address_name.map(|name| {
            let mut name = AddressName::new(name, setup.lookup);
            if start.name_now {
                name.ask();
            }
            name
        });
        let clock = Clock::new(setup.joined);
        let talk = Talk::new(
            setup.voice,
            setup.speaker,
            clock,
            setup.timers,
            setup.log.clone(),
            setup.joined,
        );
        Client {
            identity: setup.identity,
            name: control::clean(&setup.name, control::PERSON_FALLBACK),
            host_key: start.host_key,
            invite: start.invite,
            punch_sources: start.candidates.iter().map(SocketAddr::ip).collect(),
            candidates: start.candidates,
            has_ipv6: setup.has_ipv6,
            address_name,
            name_ports: Ports::of(&start.ports_from, start.also_port),
            from_name: Vec::new(),
            next_name: None,
            name_until: None,
            name_new_since: None,
            mapped_addr: start.mapped_addr,
            mapped_verified: start.mapped_verified,
            reached_mapped: false,
            session_timers: setup.timers.session(),
            timers: setup.timers,
            thresholds: Thresholds::default(),
            clock,
            port: setup.port,
            joined: setup.joined,
            state: LinkState::Connecting,
            notice: None,
            host_addr: None,
            media: None,
            path: None,
            sessions: Sessions::default(),
            heard_on_session: false,
            host_hello: false,
            last_heard: setup.joined,
            silent_since: None,
            own_moved: None,
            reply_in_room: false,
            address_change: None,
            reconnect_ms: None,
            attempts: Vec::new(),
            next_attempt: Some(setup.joined),
            fast_until: Some(setup.joined + FAST_PHASE),
            still_trying_at: Some(setup.joined + setup.timers.still_trying_after),
            secret: start.secret,
            stamps: TimestampSource::new(),
            cookies: Jar::default(),
            stun: Stun::new(
                None,
                setup.timers.stun_wait,
                setup.timers.stun_retry,
                setup.joined,
                setup.log.clone(),
            ),
            host_mapping: start.host_mapping,
            second_router: start.second_router,
            host_port: reply::host_port(&start.ports_from).or(start.also_port),
            reply: None,
            reply_wait: None,
            punch_seen: false,
            link: Link::new(setup.joined),
            traffic: Traffic::default(),
            roster: Roster {
                room: start.room_name,
                entries: Vec::new(),
            },
            handshake_ms: None,
            connect_ms: None,
            rekeys: 0,
            drops: Drops::default(),
            scratch: Vec::new(),
            log: setup.log,
            strays: PerSource::new(setup.joined),
            tries: 0,
            next_summary: None,
            connecting: false,
            hosts: setup.hosts,
            stored: start.stored,
            list_problem: setup.list_problem,
            history: History::default(),
            delivery: Delivery::default(),
            talk,
            voice_out: Arc::default(),
            host_periods: None,
            told_periods: None,
            jittery: false,
            screen: Screen::new(clock, false, setup.screen, setup.joined),
            video_out: Arc::default(),
            sharer_clock: None,
            told_fps: None,
            input_events: Vec::with_capacity(crate::remote::MAX_EVENTS),
        }
    }

    pub(crate) fn take_save(&mut self, now: Instant) -> Option<Save> {
        self.hosts.take_save(now)
    }

    pub(crate) fn take_last_save(&mut self) -> Option<Save> {
        self.hosts.take_last_save()
    }

    pub(crate) fn save_due(&self) -> Option<Instant> {
        self.hosts.save_due()
    }

    pub(crate) fn on_packet(
        &mut self,
        packet: &[u8],
        from: SocketAddr,
        now: Instant,
        socket: &Socket,
    ) -> bool {
        if self.state == LinkState::Closed {
            return false;
        }
        if net::stun::is_stun(packet) {
            return self.on_stun(packet, from, now);
        }
        match session::packet_type(packet) {
            Some(PacketType::Response) => self.on_response(packet, from, now, socket),
            Some(PacketType::Data) => {
                let mut plain = std::mem::take(&mut self.scratch);
                let changed = self.on_data(packet, from, now, socket, &mut plain);
                // The PeerSecret arrives through this buffer.
                plain.zeroize();
                self.scratch = plain;
                changed
            }
            Some(PacketType::Punch) => self.on_punch(packet, from, now),
            Some(PacketType::CookieReply) => self.on_cookie_reply(packet, from, now),
            _ => self.drops.bad(now),
        }
    }

    pub(crate) fn on_timer(&mut self, now: Instant, socket: &Socket) -> bool {
        if self.state == LinkState::Closed {
            return false;
        }
        let mut changed = false;
        self.attempts
            .retain(|a| now.saturating_duration_since(a.tried_at) < TRIES_KEPT);

        if self.log.is_on() {
            self.strays.tick(now, &self.log);
        }
        self.stun.tick(now, socket);
        if self.sessions.expire(now) {
            // Past reject_after without a rekey: the host has dropped it too.
            log!(
                self.log,
                "the session ran out without a rekey, back to the invite addresses"
            );
            self.back_to_ladder(now);
            self.fast_until = Some(now + FAST_PHASE);
            changed = true;
        }

        let silent = now.saturating_duration_since(self.last_heard);
        match self.state {
            LinkState::Live | LinkState::Reconnecting if silent >= self.timers.lost_after => {
                log!(
                    self.log,
                    "nothing heard from the host for {}, lost, back to the invite addresses",
                    secs(self.timers.lost_after)
                );
                if self.silent_since.is_none() {
                    self.silence_began(now, socket);
                }
                self.state = LinkState::Lost;
                self.notice = Some(self.lost_notice(now));
                self.back_to_ladder(now);
                self.share_over();
                self.fast_until = None;
                changed = true;
            }
            LinkState::Live if silent >= self.timers.reconnecting_after => {
                log!(
                    self.log,
                    "nothing heard from the host for {}, reconnecting",
                    secs(self.timers.reconnecting_after)
                );
                self.state = LinkState::Reconnecting;
                self.silence_began(now, socket);
                changed = true;
            }
            LinkState::Connecting | LinkState::Lost
                if self.sessions.current.is_some()
                    && !self.heard_on_session
                    && silent >= self.timers.reconnecting_after =>
            {
                // The host answered but never took the session: it may have
                // dropped it, so start over rather than wait on it.
                log!(
                    self.log,
                    "the host answered but sent nothing on the session for {}, starting over",
                    secs(self.timers.reconnecting_after)
                );
                self.back_to_ladder(now);
                if self.state == LinkState::Connecting {
                    self.still_trying_at = Some(now + self.timers.still_trying_after);
                }
            }
            _ => {}
        }

        if self.state == LinkState::Connecting
            && self.notice.is_none()
            && self.still_trying_at.is_some_and(|at| now >= at)
        {
            log!(
                self.log,
                "no answer after {}, the panel says it is still trying",
                secs(self.timers.still_trying_after)
            );
            self.notice = Some(Notice::StillTrying);
            self.still_trying_at = None;
            changed = true;
            // The host's address may have changed since the invite was made.
            if let Some(name) = self.address_name.as_mut().filter(|name| !name.is_asked()) {
                log!(
                    self.log,
                    "address name: looking up {}, since none of the invite's addresses answered",
                    name.name()
                );
                name.ask();
            }
        }
        changed |= self.reply_step(now);
        changed |= self.silence_step(now);

        let due = self.next_attempt.is_some_and(|at| now >= at);
        let rekey_due = self
            .sessions
            .current
            .as_ref()
            .is_some_and(|current| current.needs_rekey(now) || self.rekey_unconfirmed())
            && self.next_attempt.is_none_or(|at| now >= at)
            && self.can_rekey();
        if self.sessions.current.is_none() && due {
            self.try_handshake(now, socket, false);
            let fast = self.fast_until.is_some_and(|until| now < until);
            let wait = if fast {
                self.timers.handshake_fast_retry
            } else {
                self.timers.handshake_slow_retry
            };
            self.next_attempt = Some(now + wait);
        } else if rekey_due {
            self.try_handshake(now, socket, true);
            self.next_attempt = Some(now + REKEY_RETRY);
        }
        if self.next_summary.is_some_and(|at| now >= at) {
            log!(
                self.log,
                "no answer yet after {} tries to {}",
                self.tries,
                list(&self.everywhere())
            );
            self.next_summary = Some(now + SUMMARY_EVERY);
        }

        if self.sessions.current.is_some() {
            // This PC may have started talking since the ping was planned.
            let every = self.ping_every(now);
            self.link.next_ping = self.link.next_ping.min(now + every);
            if now >= self.link.next_ping {
                self.send_ping(now, socket);
                changed = true;
            }
            self.own_shape(now);
            self.flush(now, socket);
            if self.log.is_on()
                && let Some(line) = self.link.minute_line(now)
            {
                self.log.line(format!(
                    "reliable to {}: {line}",
                    keys::fingerprint(&self.host_key)
                ));
            }
        }
        self.link.stats.tick(now);
        self.jittery = chat::is_about(self.link.stats.snapshot().jitter_ms);
        self.share_step();
        self.control_step(now);
        if self.talk.report_due(now) && self.sessions.current.is_some() {
            self.share_round_trip(now);
            self.voice_reports(now, socket);
        }
        changed |= self.talk.tick(now);
        changed
    }

    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        if self.state == LinkState::Closed {
            return None;
        }
        let mut soonest = Soonest::default();
        match &self.sessions.current {
            None => soonest.add(self.next_attempt),
            Some(current) => {
                if self.can_rekey() {
                    // A retry time left over from a rekey that is no longer
                    // wanted must not become a deadline in the past.
                    let wanted = if self.rekey_unconfirmed() {
                        self.next_attempt
                    } else {
                        Some(current.created() + self.session_timers.rekey_after)
                    };
                    soonest.add(wanted.map(|at| self.next_attempt.map_or(at, |next| at.max(next))));
                }
                soonest.add(Some(self.link.next_ping));
                soonest.add(self.link.next_timeout());
                if self.log.is_on() {
                    soonest.add(Some(self.link.next_minute_line()));
                }
                soonest.add(self.share_deadline());
            }
        }
        self.sessions
            .deadlines(self.session_timers.reject_after)
            .for_each(|at| soonest.add(Some(at)));
        match self.state {
            LinkState::Live => {
                soonest.add(Some(self.last_heard + self.timers.reconnecting_after));
                soonest.add(Some(self.last_heard + self.timers.lost_after));
            }
            LinkState::Reconnecting => soonest.add(Some(self.last_heard + self.timers.lost_after)),
            LinkState::Connecting | LinkState::Lost
                if self.sessions.current.is_some() && !self.heard_on_session =>
            {
                soonest.add(Some(self.last_heard + self.timers.reconnecting_after));
            }
            _ => {}
        }
        if self.state == LinkState::Connecting && self.notice.is_none() {
            soonest.add(self.still_trying_at);
        }
        if self.notice == Some(Notice::StillTrying) {
            match &self.reply {
                None => soonest.add(self.reply_wait),
                Some(reply) => soonest.add(reply.expires),
            }
        }
        if self.reply_in_room {
            soonest.add(self.reply.as_ref().and_then(|reply| reply.expires));
        }
        soonest.add(self.next_name);
        if self.notice == Some(Notice::LostHost) {
            soonest.add(
                self.name_new_since
                    .map(|since| since + self.timers.lost_after),
            );
        }
        soonest.add(self.strays.next_deadline());
        soonest.add(self.stun.next_deadline());
        // Set only while logging and nothing has answered.
        soonest.add(self.next_summary);
        soonest.add(self.talk.next_deadline(self.sessions.current.is_some()));
        // Even with the host gone quiet: the cutoff is for exactly that.
        soonest.add(self.screen.remote.deadline());
        soonest.0
    }

    pub(crate) fn stun_found(&mut self, servers: Vec<SocketAddr>, socket: &Socket) {
        if self.state != LinkState::Closed {
            self.stun.found(servers, socket);
        }
    }

    pub(crate) fn stun_resolved(&mut self, now: Instant) -> bool {
        self.state != LinkState::Closed && self.stun.resolved(now) && self.reply_step(now)
    }

    // The timer thread starts the lookup once the fast round has gone
    // unanswered.
    pub(crate) fn name_wanted(&mut self) -> Option<Request> {
        if self.state == LinkState::Closed {
            return None;
        }
        self.address_name.as_mut()?.request()
    }

    // The addresses go on the ladder next to the invite's, in place of what
    // the name gave before. A new one gets a fresh fast round, so it has the
    // same chances the invite's had, or in a room a ping on the session
    // right away.
    pub(crate) fn name_found(&mut self, outcome: Outcome, now: Instant) -> bool {
        if self.state == LinkState::Closed {
            return false;
        }
        let Some(name) = self.address_name.as_mut() else {
            return false;
        };
        let ips = name.found(outcome);
        // A lookup that timed out, or a nameserver that has nothing for the
        // name while the host's dynamic DNS client updates it, says nothing
        // about where the host is, so what the name gave before is kept.
        if !name.has_answered() {
            return true;
        }
        let mut known = self.candidates.clone();
        known.extend(self.host_addr);
        let fresh = self
            .name_ports
            .targets(&ips, self.has_ipv6, &known, &self.log);
        let new: Vec<SocketAddr> = fresh
            .iter()
            .filter(|addr| !self.from_name.contains(addr))
            .copied()
            .collect();
        // Looked up every NAME_EVERY while the host is silent, so the same
        // answer is written down once.
        let worth_a_line = self.silent_since.is_none() || !self.from_name.is_empty();
        if fresh.is_empty() && !ips.is_empty() && worth_a_line {
            log!(self.log, "address name: nothing new to try");
        }
        self.from_name = fresh;
        if new.is_empty() {
            return true;
        }
        log!(self.log, "address name: trying {} too", list(&new));
        for ip in ips {
            if !self.punch_sources.contains(&ip) {
                self.punch_sources.push(ip);
            }
        }
        if self.silent_since.is_some() && self.name_new_since.is_none() {
            self.name_new_since = Some(now);
        }
        if self.sessions.current.is_none() {
            self.next_attempt = Some(now);
            self.fast_until = Some(now + FAST_PHASE);
        } else {
            self.link.next_ping = now;
        }
        true
    }

    // Windows says an address on this PC changed.
    pub(crate) fn address_changed(&mut self, now: Instant, socket: &Socket) -> bool {
        if self.state == LinkState::Closed {
            return false;
        }
        if self.stun.check(now, socket) {
            log!(
                self.log,
                "windows reported an address change on this pc, asking stun whether the outside address changed"
            );
        }
        false
    }

    // The New code button: the same screen again with a fresh expiry.
    pub(crate) fn new_code(&mut self, now: Instant) -> bool {
        let wanted = self.reply.as_ref().is_some_and(|reply| {
            matches!(
                reply.state,
                ReplyState::Code { .. } | ReplyState::Expired { .. }
            )
        });
        if !wanted || (self.state != LinkState::Connecting && !self.reply_in_room) {
            return false;
        }
        log!(self.log, "new reply code asked for");
        if self.reply_in_room {
            // Above the people list there is a code or nothing.
            self.reply = None;
            self.reply_in_room = false;
            self.in_room_code(now);
        } else {
            self.make_reply(now);
        }
        true
    }

    pub(crate) fn leave(&mut self, now: Instant, socket: &Socket) {
        if self.state == LinkState::Closed {
            return;
        }
        if self.log.is_on() {
            self.strays.flush(now, &self.log);
        }
        if let (Some(session), Some(addr)) = (self.sessions.current.as_mut(), self.host_addr) {
            log!(self.log, "leaving, bye sent to the host at {addr}");
            self.link.queue(&Message::Bye);
            self.link
                .flush_twice(socket, session, &[addr], now, &mut self.traffic);
        } else {
            log!(self.log, "leaving with no session to the host");
        }
        self.stop_sending();
    }

    pub(crate) fn socket_failed(&mut self) {
        self.stop_sending();
        self.notice = Some(Notice::SocketFailed);
    }

    // The host is of another protocol, or None, a test build from before
    // version numbers, so nothing more it sends can be read. This PC leaves
    // at once with a Bye, which every build knows, and says why.
    fn other_version(&mut self, host: Option<(u16, Version)>, now: Instant, socket: &Socket) {
        match host {
            Some((protocol, version)) => log!(
                self.log,
                "the host runs booth {version}, protocol {protocol}; this pc booth {}, protocol {}",
                invite::VERSION,
                invite::PROTOCOL
            ),
            None => {
                log!(
                    self.log,
                    "the host sent something before its hello: a test build from before version numbers, told why in its chat"
                );
                self.tell_unversioned_host(now, socket);
            }
        }
        self.leave(now, socket);
        self.notice = Some(match host {
            Some((protocol, version)) => Notice::OtherVersion { protocol, version },
            None => Notice::UnversionedHost,
        });
    }

    // Such a host reads nothing of this PC's Hello, and without more its room
    // would only see a friend come and go. Its chat takes lines once a Hello
    // it can read has come, so that goes first, then one line for everyone
    // there, all ahead of the Bye.
    fn tell_unversioned_host(&mut self, now: Instant, socket: &Socket) {
        let (Some(session), Some(addr)) = (self.sessions.current.as_mut(), self.host_addr) else {
            return;
        };
        self.link.queue(&Message::UnversionedHello {
            name: self.name.clone(),
        });
        let say = ChatMessage::Say {
            text: unversioned_host_line(),
            sent_at: self.clock.micros(now),
        };
        let _ = self.link.queue_chat(&say.encode());
        self.link
            .flush_all_twice(socket, session, &[addr], now, &mut self.traffic);
    }

    // The line shows here at once, before the host has it. While the host is
    // only quiet it waits in the stream and goes out when the host is heard
    // again, or on the next session if the host is lost first (on_response
    // carries it over). Once the host is lost nothing new is taken, since
    // nobody can tell when it would go. `text` has been through
    // chat::clean_text.
    pub(crate) fn say(
        &mut self,
        text: String,
        now: Instant,
        socket: &Socket,
    ) -> Result<(), ChatRefused> {
        if !matches!(self.state, LinkState::Live | LinkState::Reconnecting) {
            return Err(ChatRefused::NotLive);
        }
        let say = ChatMessage::Say {
            text: text.clone(),
            sent_at: self.clock.micros(now),
        };
        // Full only after a long silence, with a thousand lines waiting.
        if self.link.queue_chat(&say.encode()).is_err() {
            return Err(ChatRefused::NotLive);
        }
        let author = *self.identity.public();
        let len = text.len();
        self.history.push(ChatLine {
            author,
            name: self.name.clone(),
            text,
            at_unix_ms: crate::unix_now_ms(),
            mine: true,
            kind: LineKind::Said,
        });
        self.flush(now, socket);
        log!(
            self.log,
            "chat: {} wrote {len} bytes, this pc's own so no delivery time",
            keys::fingerprint(&author)
        );
        Ok(())
    }

    // `oversized` is what net dropped for not fitting the receive buffer.
    pub(crate) fn view(&self, now: Instant, oversized: u64) -> View {
        let snapshot = self.link.stats.snapshot();
        let host_name = self
            .roster
            .entries
            .iter()
            .find(|entry| entry.is_host)
            .map(|entry| entry.name.clone());
        let mut numbers = Numbers {
            local_port: self.port,
            ping_interval: self.timers.ping_idle,
            handshake_ms: self.handshake_ms,
            connect_ms: self.connect_ms,
            dropped_bad: self.drops.bad + oversized,
            dropped_replay: self.drops.replayed,
            address_name: self.address_name.as_ref().map(|name| name.view(None)),
            address_change: self.address_change,
            reconnect_ms: self.reconnect_ms,
            ..Numbers::default()
        };
        let voice = self.talk.on_link(&self.host_key, now);
        let video = self.screen.on_link(&self.host_key, now);
        LinkNumbers {
            name: host_name,
            reconnecting: self.state == LinkState::Reconnecting,
            snapshot: &snapshot,
            voice,
            video,
            link: &self.link,
            sessions: &self.sessions,
            traffic: &self.traffic,
            path: self.path,
            peer_addr: self.host_addr,
            rekeys: self.rekeys,
            ping_interval: self.ping_every(now),
        }
        .fill(&mut numbers, now);
        self.voice_out.add_to(&mut numbers);
        self.video_out.add_to(&mut numbers);
        self.screen.fill(&mut numbers);
        self.delivery.fill(&mut numbers, now);
        let roster = &self.roster;
        let name = |key: &[u8; 32]| {
            let entry = roster.entries.iter().find(|entry| entry.key == *key)?;
            Some(entry.name.clone())
        };
        self.talk
            .fill(&mut numbers, name, |_, loss| loss, self.host_periods, now);

        let own = self.identity.public();
        let people = self
            .roster
            .entries
            .iter()
            .map(|entry| {
                let rtt_ms = entry.rtt_ms.map(f32::from);
                Person {
                    key: entry.key,
                    name: entry.name.clone(),
                    fingerprint: keys::fingerprint(&entry.key),
                    rtt_ms,
                    rtt_level: numbers::rtt_level(rtt_ms, &self.thresholds),
                    is_you: entry.key == *own,
                    is_host: entry.is_host,
                    joined_by_invite: entry.joined_by_invite,
                    reconnecting: entry.reconnecting,
                    talking: if entry.key == *own {
                        self.talk.shared().sending()
                    } else {
                        self.talk.talking(&entry.key, now)
                    },
                    sharing: entry.share.is_some(),
                }
            })
            .collect();

        View {
            role: Role::Client,
            room_name: self.roster.room.clone(),
            strip: numbers::strip(
                self.state,
                &snapshot,
                voice,
                video,
                self.path,
                &self.thresholds,
            ),
            people,
            invite: None,
            numbers,
            chat: self.history.shared(),
            notice: self.notice.clone(),
            reply: self.reply.as_ref().map(|reply| ReplyView {
                state: reply.state,
                code: reply.code.clone(),
                expires_at_unix: reply.expires_at_unix,
                host_port: self.host_port,
            }),
            paste: None,
            address_changed: None,
            list_problem: self.list_problem.clone(),
            share: self.share_view(),
            voice: self.talk.shared().view(),
        }
    }

    // A rekey with the invite psk is really a new start, which the host
    // accepts only while the invite lives; past that there is nothing to try
    // until the session runs out and the ladder takes over.
    fn can_rekey(&self) -> bool {
        self.state != LinkState::Lost
            && (self.secret.is_some()
                || self
                    .invite
                    .as_ref()
                    .is_some_and(|invite| crate::unix_now() < invite.expires_at))
    }

    // We switched to a rekeyed session but the host still only sends on the
    // one before it: it never got our first packet on the new one before it
    // let the session go. It still has a live link on the old one, so another
    // rekey will be accepted.
    fn rekey_unconfirmed(&self) -> bool {
        self.sessions.previous.is_some() && !self.heard_on_session
    }

    fn try_handshake(&mut self, now: Instant, socket: &Socket, rekey: bool) {
        let (kind, psk) = match (&self.secret, &self.invite, rekey) {
            (Some(secret), _, true) => (InitKind::Rekey, secret.clone()),
            (Some(secret), _, false) => (InitKind::Known, secret.clone()),
            (None, Some(invite), _) => (InitKind::Invite(invite.id), invite.psk.clone()),
            // A rejoin starts with the secret, so there is always one or
            // the other.
            (None, None, _) => return,
        };
        let targets = if rekey {
            self.host_addr.into_iter().collect()
        } else {
            self.everywhere()
        };
        if self.log.is_on() {
            self.note_try(kind, &targets, now);
        }
        // One initiation to every address, cookie or not, so that a host
        // reached over two paths sees the same packet twice and answers it
        // once. A cookie is good only at the address it came from, so under
        // load only that path gets key math and the others get cookie
        // replies; otherwise the host ignores mac2.
        let cookie = self.cookies.pick(self.host_addr, &targets, now);
        let index = peer::random_index(|index| {
            self.sessions.holds(index)
                || self
                    .attempts
                    .iter()
                    .any(|a| a.initiation.sender_index() == index)
        });
        let started = Initiation::start_with_cookie(
            &self.identity.private_bytes(),
            self.identity.public(),
            &self.host_key,
            &psk,
            kind,
            self.stamps.next_stamp(),
            index,
            cookie.as_ref(),
        );
        // join() already checked the one thing that makes this fail.
        let Ok((initiation, packet)) = started else {
            return;
        };
        let sent_at = Instant::now();
        for addr in &targets {
            self.traffic.send(socket, &packet, *addr);
        }
        self.attempts.push(Attempt {
            initiation,
            kind,
            tried_at: now,
            sent_at,
        });
    }

    fn on_stun(&mut self, packet: &[u8], from: SocketAddr, now: Instant) -> bool {
        match self.stun.on_answer(packet, from, now) {
            Answer::NotOurs(why) => {
                if self.log.is_on() && self.strays.allow(from.ip(), now, &self.log) {
                    self.log.line(format!(
                        "from {from}, {} bytes: {why}, dropped",
                        packet.len()
                    ));
                }
                self.drops.bad(now)
            }
            Answer::Recorded => false,
            Answer::Settled => self.reply_step(now),
            Answer::Moved(moved) => self.own_address_moved(moved, now),
        }
    }

    // The tries to the host go on from the new address either way; a host
    // whose port is mapped, or whose router lets anyone in, takes this PC
    // back by the roaming rule. Any other host never sent to the new address,
    // so it needs the code again.
    fn own_address_moved(&mut self, moved: Moved, now: Instant) -> bool {
        log!(
            self.log,
            "this pc's outside address changed from {} to {}",
            moved.from,
            moved.to
        );
        self.address_change = Some(AddressChange {
            at_unix: crate::unix_now(),
            this_pc: true,
            from: moved.from,
            to: moved.to,
        });
        if self.state != LinkState::Connecting {
            // Stamped the way Link::ping stamps a ping, when it is built.
            self.own_moved = Some(self.clock.micros(Instant::now()));
            if matches!(self.state, LinkState::Reconnecting | LinkState::Lost) {
                self.in_room_code(now);
            }
        }
        true
    }

    // The code block above the people list, while the host is silent and
    // this PC's own outside address changed.
    fn in_room_code(&mut self, now: Instant) {
        if self.reply.is_some() {
            return;
        }
        if self.mapped_verified {
            log!(
                self.log,
                "no code above the people list: the invite says the host's port is mapped and a friend came in through it, so the host takes this pc back as it is"
            );
            return;
        }
        if self.reached_mapped {
            log!(
                self.log,
                "no code above the people list: this pc reached the host through its mapped port, which takes it back as it is"
            );
            return;
        }
        if let Some(addr) = self
            .host_addr
            .filter(|addr| net::addrs::is_inside(addr.ip()))
        {
            log!(
                self.log,
                "no code above the people list: the host is at {addr}, on the lan or through a tunnel, which this pc's outside address does not touch"
            );
            return;
        }
        self.make_reply(now);
        let code = self
            .reply
            .as_ref()
            .is_some_and(|reply| matches!(reply.state, ReplyState::Code { .. }));
        if code {
            self.reply_in_room = true;
            log!(
                self.log,
                "the code shows above the people list, for the host to paste"
            );
        } else {
            self.reply = None;
            log!(
                self.log,
                "no code above the people list: a code back cannot help here"
            );
        }
    }

    // While the host is silent the name is looked up again every NAME_EVERY
    // for NAME_FOR, and a new address from it that stays silent for
    // lost_after turns the lost notice into the one that says the host
    // moved.
    fn silence_step(&mut self, now: Instant) -> bool {
        if let Some(next) = self.next_name
            && now >= next
        {
            if self.name_until.is_some_and(|until| now < until) {
                if let Some(name) = self.address_name.as_mut() {
                    name.ask();
                }
                self.next_name = Some(now + NAME_EVERY);
            } else {
                log!(
                    self.log,
                    "address name: looked up for {} while the host was silent, no more",
                    secs(NAME_FOR)
                );
                self.next_name = None;
                self.name_until = None;
            }
        }
        if self.notice == Some(Notice::LostHost) && self.lost_notice(now) == Notice::HostMoved {
            log!(
                self.log,
                "the address name gave the host a new address and nothing answered there for {}",
                secs(self.timers.lost_after)
            );
            self.notice = Some(Notice::HostMoved);
            return true;
        }
        false
    }

    fn lost_notice(&self, now: Instant) -> Notice {
        let moved = self
            .name_new_since
            .is_some_and(|since| now >= since + self.timers.lost_after);
        if moved {
            Notice::HostMoved
        } else {
            Notice::LostHost
        }
    }

    // Silence can mean either side's address changed: STUN says whether this
    // PC moved, and the name, when the invite has one, where the host may
    // have gone.
    fn silence_began(&mut self, now: Instant, socket: &Socket) {
        self.silent_since = Some(self.last_heard);
        if self.stun.check(now, socket) {
            log!(
                self.log,
                "asking stun whether this pc's outside address changed"
            );
        }
        if let Some(name) = &self.address_name {
            log!(
                self.log,
                "address name: looking up {} every {} for up to {} while the host is silent",
                name.name(),
                secs(NAME_EVERY),
                secs(NAME_FOR)
            );
            self.next_name = Some(now);
            self.name_until = Some(now + NAME_FOR);
        }
        if self.own_moved.is_some() {
            self.in_room_code(now);
        }
    }

    // The host is heard again.
    fn silence_over(&mut self, from: SocketAddr, now: Instant) {
        if let Some(since) = self.silent_since.take() {
            let silence = now.saturating_duration_since(since);
            log!(
                self.log,
                "the host is heard again at {from}, after {} of silence",
                secs(silence)
            );
            self.reconnect_ms = Some(numbers::millis(silence));
        }
        if self.stun.end_check() {
            log!(self.log, "stun: the host answered, the address check stops");
        }
        self.next_name = None;
        self.name_until = None;
        self.name_new_since = None;
        if self.reply_in_room {
            self.reply = None;
            self.reply_in_room = false;
            log!(
                self.log,
                "the code above the people list goes, the host answered"
            );
        }
    }

    // The host sends these only after its user pasted our reply code. They
    // open its router for us and need no answer; ours are the initiations
    // that keep going out.
    fn on_punch(&mut self, packet: &[u8], from: SocketAddr, now: Instant) -> bool {
        let known = self.punch_sources.contains(&from.ip())
            || self.host_addr.is_some_and(|addr| addr.ip() == from.ip());
        if !session::is_punch(packet) || !known {
            return self.drops.bad(now);
        }
        if !self.punch_seen {
            self.punch_seen = true;
            log!(
                self.log,
                "the host's punch packets are arriving from {from}"
            );
        }
        false
    }

    // Called on every timer pass and whenever STUN finishes. Makes the code
    // once the panel says it is still trying and STUN has finished, or has
    // had stun_wait more; then watches the code's expiry, or STUN finishing
    // late.
    fn reply_step(&mut self, now: Instant) -> bool {
        if self.reply_in_room {
            return self.reply_expiry(now);
        }
        if self.notice != Some(Notice::StillTrying) || self.sessions.current.is_some() {
            return false;
        }
        let made_early = self
            .reply
            .as_ref()
            .is_some_and(|reply| reply.state == ReplyState::NoAddress && !reply.stun_finished);
        if made_early && self.stun.is_settled() {
            log!(
                self.log,
                "stun finished after the reply screen was made without it, so it is made again"
            );
            self.make_reply(now);
            return true;
        }
        if self.reply.is_none() {
            if !self.stun.is_settled() {
                let wait = *self.reply_wait.get_or_insert(now + self.timers.stun_wait);
                if now < wait {
                    return false;
                }
                log!(
                    self.log,
                    "stun has not finished, the reply code is made with what it has"
                );
            }
            self.make_reply(now);
            return true;
        }
        self.reply_expiry(now)
    }

    fn reply_expiry(&mut self, now: Instant) -> bool {
        let Some(reply) = &mut self.reply else {
            return false;
        };
        if !reply.expires.is_some_and(|at| now >= at) {
            return false;
        }
        let rung = if self.second_router {
            "the second router in front of the host"
        } else {
            "easy or unknown mapping"
        };
        log!(
            self.log,
            "the reply code expired with no answer from the host; the rung that failed: {rung}"
        );
        reply.state = ReplyState::Expired {
            second_router: self.second_router,
        };
        reply.code.clear();
        reply.expires = None;
        true
    }

    fn make_reply(&mut self, now: Instant) {
        self.reply_wait = None;
        let own = self.stun.invite_mapping();
        let (v4, v6) = (self.stun.public_v4(), self.stun.public_v6());
        let has_address = v4.is_some() || v6.is_some();
        let mut state = reply::choose(self.host_mapping, self.second_router, own, has_address);
        let mut made = None;
        if let ReplyState::Code { .. } = state {
            let answers = match (&self.secret, &self.invite) {
                (None, Some(invite)) => Answers::Invite(invite.id),
                _ => Answers::Rejoin,
            };
            let key = *self.identity.public();
            let now_unix = crate::unix_now();
            made = reply::build(answers, key, v4, v6, own, now_unix, &self.log);
            if made.is_none() {
                state = ReplyState::NoAddress;
            }
        }
        match (&made, state) {
            (Some(code), _) => {
                let mut to: Vec<SocketAddr> = Vec::new();
                to.extend(code.outside_v4.map(SocketAddr::V4));
                to.extend(code.outside_v6.map(SocketAddr::V6));
                log!(
                    self.log,
                    "reply code made: this pc's mapping {}, outside {}, expires {}",
                    mapping_word(own),
                    list(&to),
                    log::utc(code.expires_at)
                );
            }
            (None, ReplyState::HostHard) => log!(
                self.log,
                "no reply code: the invite says the host's mapping is hard"
            ),
            (None, ReplyState::OwnHard) => log!(
                self.log,
                "no reply code: stun says this pc's mapping is hard"
            ),
            (None, _) => log!(
                self.log,
                "no reply code: stun gave this pc no outside address"
            ),
        }
        self.reply = Some(Reply {
            state,
            code: made.as_ref().map(|code| code.encode()).unwrap_or_default(),
            expires_at_unix: made.as_ref().map_or(0, |code| code.expires_at),
            expires: made.is_some().then(|| now + REPLY_LIFETIME),
            stun_finished: self.stun.is_settled(),
        });
    }

    fn everywhere(&self) -> Vec<SocketAddr> {
        let mut targets = self.candidates.clone();
        for &addr in self.from_name.iter().chain(&self.host_addr) {
            if !targets.contains(&addr) {
                targets.push(addr);
            }
        }
        targets
    }

    fn on_response(
        &mut self,
        packet: &[u8],
        from: SocketAddr,
        now: Instant,
        socket: &Socket,
    ) -> bool {
        let Some(index) = session::response_receiver_index(packet) else {
            self.refused("handshake response", packet, from, now, "bad length");
            return self.drops.bad(now);
        };
        let Some(at) = self
            .attempts
            .iter()
            .position(|a| a.initiation.sender_index() == index)
        else {
            self.refused(
                "handshake response",
                packet,
                from,
                now,
                "it answers no try still open, a late or second answer",
            );
            return self.drops.bad(now);
        };
        let session = match self.attempts[at].initiation.finish(packet, now) {
            Ok(session) => session.with_timers(self.session_timers),
            Err(err) => {
                if self.log.is_on() {
                    self.refused("handshake response", packet, from, now, &err.to_string());
                }
                return self.drops.bad(now);
            }
        };
        let attempt = self.attempts.swap_remove(at);
        self.attempts.clear();
        self.next_attempt = None;
        self.traffic.received(packet.len());
        let took = now.saturating_duration_since(attempt.sent_at);
        self.handshake_ms = Some(numbers::millis(took));
        self.still_trying_at = None;
        if self.notice == Some(Notice::StillTrying) {
            self.notice = None;
        }
        let had_reply = self.reply.take().is_some();
        self.reply_in_room = false;
        self.reply_wait = None;
        log!(
            self.log,
            "from {from}: {} answer taken, handshake {:.1} ms",
            init_word(attempt.kind),
            numbers::millis(took)
        );
        if had_reply {
            log!(self.log, "the reply code screen closes, the host answered");
        }
        if self
            .from_name
            .iter()
            .any(|addr| reply::same_address(*addr, from))
        {
            log!(
                self.log,
                "address name: the host answered at {from}, which only the name gave"
            );
        }
        self.tries = 0;
        self.next_summary = None;

        if attempt.kind == InitKind::Rekey {
            // Attempts are dropped with the session, so a rekey always has one
            // to replace. Starting the reliable stream over here would put
            // this side out of step with the host, which keeps its stream.
            if self.sessions.current.is_none() {
                return false;
            }
            if self.heard_on_session {
                self.sessions.previous = self.sessions.current.replace(session);
            } else {
                // The host never took the session this replaces and still
                // sends on the one before it, so that one has to stay.
                self.sessions.current = Some(session);
            }
            self.heard_on_session = false;
            self.next_attempt = Some(now + REKEY_RETRY);
            self.rekeys += 1;
            // The host moves to the new session on the first packet it gets on
            // it, so one right away keeps its old session from running out.
            self.send_ping(now, socket);
            return true;
        }

        self.sessions.previous = None;
        self.sessions.current = Some(session);
        self.set_host_addr(from);
        self.media = Some(MediaPath::new(from));
        self.reached_mapped = self
            .mapped_addr
            .is_some_and(|mapped| reply::same_address(mapped, from));
        self.connecting = true;
        self.traffic.earlier_retransmits += self.link.retransmissions();
        let old = std::mem::replace(&mut self.link, Link::new(now));
        // A Periods still unacked on the old stream is gone with it, and a
        // host that let this PC go has none: they go again on the new one.
        self.told_periods = None;
        self.heard_on_session = false;
        self.host_hello = false;
        self.last_heard = now;
        self.link.queue(&Message::Hello {
            version: invite::VERSION,
            name: self.name.clone(),
            reached: Some(from),
        });
        // Lines said while the host was quiet already show here as said.
        // The host starts its stream over at this handshake too, so they go
        // again on the new one, after the Hello the host waits for before it
        // takes any. One whose ack alone was lost reaches the room twice,
        // which beats never.
        for say in old.chat.unacked() {
            let _ = self.link.queue_chat(say);
        }
        self.share_restarted();
        self.flush(now, socket);
        self.send_ping(now, socket);
        true
    }

    // A host under load answers a try with a cookie and wants it back in mac2
    // before it does any key math. The reply is sealed against the mac1 of
    // the try it names, so only someone who saw that try can make one, and it
    // is taken only from an address the tries go to.
    fn on_cookie_reply(&mut self, packet: &[u8], from: SocketAddr, now: Instant) -> bool {
        const WHAT: &str = "cookie reply";
        let Some(index) = session::cookie_reply_receiver_index(packet) else {
            self.refused(WHAT, packet, from, now, "bad length");
            return self.drops.bad(now);
        };
        let Some(mac1) = self
            .attempts
            .iter()
            .find(|a| a.initiation.sender_index() == index)
            .map(|a| a.initiation.mac1())
        else {
            self.refused(WHAT, packet, from, now, "it answers no try still open");
            return self.drops.bad(now);
        };
        let tried_there = self
            .everywhere()
            .iter()
            .any(|addr| reply::same_address(*addr, from));
        if !tried_there {
            self.refused(WHAT, packet, from, now, "no try went to that address");
            return self.drops.bad(now);
        }
        let Ok((_, cookie)) = session::read_cookie_reply(packet, &self.host_key, &mac1) else {
            self.refused(
                WHAT,
                packet,
                from,
                now,
                "it does not open with the host key and the mac1 of the try it names",
            );
            return self.drops.bad(now);
        };
        self.traffic.received(packet.len());
        self.cookies.keep(from, cookie, now);
        if self.log.is_on() && self.strays.allow(from.ip(), now, &self.log) {
            self.log.line(format!(
                "from {from}: cookie reply taken, the host is under load; kept {} for the mac2 of the tries that follow",
                secs(COOKIE_KEPT)
            ));
        }
        false
    }

    fn on_data(
        &mut self,
        packet: &[u8],
        from: SocketAddr,
        now: Instant,
        socket: &Socket,
        plain: &mut Vec<u8>,
    ) -> bool {
        let found =
            session::data_receiver_index(packet).and_then(|index| self.sessions.which(index));
        let Some(which) = found else {
            return self.drops.bad(now);
        };
        let Some(session) = self.sessions.get_mut(which) else {
            return false;
        };
        let received = match peer::open(session, packet, plain) {
            Opened::Data(received) => received,
            Opened::Replayed => {
                return self.drops.replayed(now);
            }
            Opened::Bad => {
                return self.drops.bad(now);
            }
        };
        self.traffic.received(packet.len());

        // Voice arrives 200 times a second a talker, and the view is made
        // again only when something in it changed.
        let mut changed = false;
        let quiet = matches!(self.state, LinkState::Reconnecting | LinkState::Lost)
            || now.saturating_duration_since(self.last_heard) >= self.timers.quiet_after();
        if which == Which::Current && received.newest && Some(from) != self.host_addr {
            changed = true;
            if let Some(was_at) = self.host_addr {
                log!(self.log, "the host moved from {was_at} to {from}");
                if quiet {
                    self.address_change = Some(AddressChange {
                        at_unix: crate::unix_now(),
                        this_pc: false,
                        from: was_at,
                        to: from,
                    });
                }
            }
            self.set_host_addr(from);
            self.link.path_changed(now);
            // A ping goes there at once, so voice follows a real move within
            // a round trip. The time is taken now, under the state lock, so
            // no ping built before the move can count.
            if let Some(media) = self.media.as_mut()
                && media.moved(from, self.clock.micros(Instant::now()))
            {
                self.link.next_ping = now;
            }
            if !self.connecting {
                self.note_reached();
            }
        } else if quiet {
            self.link.path_recovered(now);
        }
        self.last_heard = now;
        if which == Which::Current && !self.heard_on_session {
            self.heard_on_session = true;
            changed = true;
            // The host took the session, so no rekey is waiting to be retried.
            self.next_attempt = None;
        }
        if self.state != LinkState::Live {
            self.silence_over(from, now);
            self.state = LinkState::Live;
            self.notice = None;
            changed = true;
        }
        self.on_plain(plain, from, now, socket) || changed
    }

    // True unless it was voice or video that changed nothing the view shows.
    fn on_plain(&mut self, plain: &[u8], from: SocketAddr, now: Instant, socket: &Socket) -> bool {
        match peer::read_plain(plain) {
            Some(Plain::Ping(PingMessage::Ping { seq, t1 })) => {
                let pong = self.link.answer(seq, t1, now, self.clock);
                if let (Some(session), Some(addr)) =
                    (self.sessions.current.as_mut(), self.host_addr)
                {
                    peer::send_on(
                        socket,
                        session,
                        Channel::Ping,
                        &pong,
                        &[addr],
                        &mut self.traffic,
                    );
                }
            }
            Some(Plain::Ping(pong)) => {
                if !self.link.pong(pong, now, self.clock) {
                    self.drops.bad(now);
                    return true;
                }
                if let PingMessage::Pong { t1, .. } = pong
                    && self.own_moved.is_some_and(|moved| t1 >= moved)
                {
                    self.own_moved = None;
                    log!(
                        self.log,
                        "the host answered a ping sent after this pc's outside address changed, so it hears the new one"
                    );
                }
                if let PingMessage::Pong { t1, .. } = pong
                    && let Some(media) = self.media.as_mut()
                    && media.answered(from, t1)
                {
                    log!(
                        self.log,
                        "the host answered a ping at {from}, voice and video go there now"
                    );
                }
                let first = self.connect_ms.is_none();
                if first {
                    self.connect_ms =
                        Some(numbers::millis(now.saturating_duration_since(self.joined)));
                }
                if self.connecting {
                    self.connecting = false;
                    self.note_connected(first);
                    self.note_reached();
                }
            }
            Some(Plain::Control(frame)) => self.on_control(frame, now, socket),
            Some(Plain::Chat(frame)) => self.on_chat(frame, now, socket),
            Some(Plain::Voice(payload)) => return self.on_voice(payload, now),
            Some(Plain::Video(payload)) => return self.on_video(payload, now),
            Some(Plain::Cursor(payload)) => return self.on_cursor(payload, now),
            Some(Plain::Input(payload)) => return self.on_input(payload, now),
            None => {
                self.drops.bad(now);
            }
        }
        true
    }

    // Someone's voice, handed on by the host. The host is only a friend's PC
    // too, so it is checked as closely as the host checks a friend's, and it
    // is played under the name the roster gives its slot. This PC's own
    // voice coming back is not played.
    fn on_voice(&mut self, payload: &[u8], now: Instant) -> bool {
        let heard = match talk::read_relayed(payload) {
            Ok(heard) => {
                self.link.media_passed(now, &self.timers);
                heard
            }
            Err(why) => {
                self.talk.dropped();
                if self.log.is_on()
                    && let Some(from) = self.host_addr
                    && self.strays.allow(from.ip(), now, &self.log)
                {
                    self.log.line(format!(
                        "voice from the host, {} bytes, dropped: {why}",
                        payload.len()
                    ));
                }
                self.drops.bad(now);
                return false;
            }
        };
        let own = *self.identity.public();
        let Some(key) = self
            .roster
            .entries
            .iter()
            .find(|entry| entry.slot == heard.slot)
            .map(|entry| entry.key)
            .filter(|key| *key != own)
        else {
            return false;
        };
        // A friend's voice came through the host, so its time rests on two
        // clock offsets, as chat delivery does: "about" if either link is
        // jittery.
        let offset = self.link.offset.best().map(|sample| sample.offset_us);
        let (captured, jittery) = match heard.captured() {
            Some(at) => talk::our_time(at, offset, self.jittery),
            None => (None, false),
        };
        self.talk
            .hear(key, &heard.frame, captured, heard.about || jittery, now)
    }

    // Once a second: what this PC lost of each person it heard, by the slot
    // the host gave them, and this PC's audio periods when the host has not
    // heard them yet.
    fn voice_reports(&mut self, now: Instant, socket: &Socket) {
        let heard: Vec<(u8, LossPermille)> = self
            .talk
            .losses(now)
            .into_iter()
            .filter_map(|(key, loss)| {
                let entry = self.roster.entries.iter().find(|entry| entry.key == key)?;
                Some((entry.slot, talk::to_wire(loss)))
            })
            .collect();
        if !heard.is_empty() {
            self.link.queue(&Message::VoiceLoss(heard));
        }
        let periods = self.talk.periods();
        if self.told_periods != Some(periods) {
            self.link.queue(&Message::Periods(periods));
            self.told_periods = Some(periods);
        }
        self.flush(now, socket);
    }

    // The link the capture thread sends this PC's voice on: the host's
    // current session, at the address media goes to.
    pub(crate) fn publish_voice(&mut self) {
        let session = self
            .sessions
            .current
            .as_ref()
            .filter(|_| self.state != LinkState::Closed);
        let to = self.media.map(|media| media.to());
        let links = session
            .zip(to)
            .map(|(session, addr)| (session.remote_index(), addr))
            .into_iter();
        let sent = &self.voice_out;
        self.talk.publish(links, || match (session, to) {
            (Some(session), Some(to)) => session
                .sealer()
                .map(|sealer| Outlet {
                    sealer,
                    to,
                    sent: Arc::clone(sent),
                })
                .into_iter()
                .collect(),
            _ => Vec::new(),
        });
    }

    pub(crate) fn clock(&self) -> Clock {
        self.clock
    }

    pub(crate) fn screen_hooks(&self) -> (Arc<Sharing>, Arc<Watching>) {
        (
            Arc::clone(&self.screen.sharing),
            Arc::clone(&self.screen.watching),
        )
    }

    // This PC's own voice or video went out lately.
    fn sent_lately(&self, now: Instant) -> bool {
        self.talk.sent_lately(now) || self.screen.sharing.sent_lately(now)
    }

    fn on_chat(&mut self, frame: &[u8], now: Instant, socket: &Socket) {
        if self.link.receive_chat(frame, now).is_err() {
            self.drops.bad(now);
            return;
        }
        let mut delivered = Vec::new();
        while let Some(message) = self.link.chat.next_delivered() {
            delivered.push(message);
        }
        self.flush(now, socket);
        for bytes in delivered {
            self.took_said(&bytes, now);
        }
    }

    // The host cleaned it, and it is cleaned again here: the text is shown,
    // and a host is only a friend's PC too.
    fn took_said(&mut self, bytes: &[u8], now: Instant) {
        let Some(ChatMessage::Said {
            author,
            name,
            text,
            sent_at_host,
            about,
        }) = ChatMessage::decode(bytes)
        else {
            self.drops.bad(now);
            return;
        };
        let Ok(text) = chat::clean_text(&text) else {
            self.drops.bad(now);
            return;
        };
        let own = *self.identity.public();
        let offset = self.link.offset.best().map(|sample| sample.offset_us);
        // A friend's line came through the host, so the time rests on two
        // clock offsets: this PC's to the host and the host's to the author.
        // Either link being jittery makes it "about".
        let delivery = match (sent_at_host, offset) {
            (Some(at), Some(offset)) if author != own => {
                chat::delivery_ms(at, offset, self.clock.micros(now))
            }
            _ => None,
        }
        .map(|ms| {
            let jittery = chat::is_about(self.link.stats.snapshot().jitter_ms);
            (ms, about || jittery)
        });
        if let Some((ms, about)) = delivery {
            self.delivery.record(now, ms, about);
        }
        let len = text.len();
        self.history.push(ChatLine {
            author,
            name,
            text,
            at_unix_ms: crate::unix_now_ms(),
            mine: author == own,
            kind: LineKind::Said,
        });
        if self.log.is_on()
            && let Some(from) = self.host_addr
            && self.strays.allow(from.ip(), now, &self.log)
        {
            self.log.line(format!(
                "chat: {} wrote {len} bytes, {}",
                keys::fingerprint(&author),
                log::delivery(delivery)
            ));
        }
    }

    fn on_control(&mut self, frame: &[u8], now: Instant, socket: &Socket) {
        if self.link.receive(frame, now).is_err() {
            self.drops.bad(now);
            return;
        }
        let mut delivered = Vec::new();
        while let Some(message) = self.link.reliable.next_delivered() {
            delivered.push(Zeroizing::new(message));
        }
        self.flush(now, socket);
        for bytes in delivered {
            let message = Message::decode(&bytes);
            if !self.host_hello {
                match message {
                    Some(Message::Hello { version, .. }) => {
                        self.host_hello = true;
                        if version != invite::VERSION {
                            log!(
                                self.log,
                                "the host runs booth {version}, this pc {}, the same protocol",
                                invite::VERSION
                            );
                        }
                        continue;
                    }
                    Some(Message::OtherHello {
                        protocol, version, ..
                    }) => self.other_version(Some((protocol, version)), now, socket),
                    // Only builds from before version numbers open a link
                    // with anything else.
                    _ => self.other_version(None, now, socket),
                }
                return;
            }
            match message {
                Some(Message::PeerSecret { secret }) => {
                    self.keep_host(&secret);
                    self.secret = Some(secret);
                }
                Some(Message::HostAddresses {
                    candidates,
                    address_name,
                }) => self.host_addresses(&candidates, address_name, now),
                Some(Message::Roster(roster)) => {
                    let old = std::mem::replace(&mut self.roster, roster);
                    self.note_names();
                    let keys: Vec<[u8; 32]> =
                        self.roster.entries.iter().map(|entry| entry.key).collect();
                    self.talk.keep_only(&keys, now);
                    self.roster_arrived(&old);
                }
                Some(Message::WorstLoss(lost)) => {
                    self.talk.listeners_lost(talk::from_wire(lost), now);
                }
                Some(Message::Periods(periods)) => self.host_periods = Some(periods),
                Some(Message::Bye) => {
                    log!(self.log, "the host closed the room, said bye");
                    self.stop_sending();
                    self.notice = Some(Notice::RoomClosed);
                    return;
                }
                Some(
                    message @ (Message::ShareAnswer(_)
                    | Message::Recover { .. }
                    | Message::Idr { .. }
                    | Message::VideoLoss { .. }
                    | Message::ShareFacts(_)
                    | Message::SharerClock { .. }
                    | Message::Shape(_)),
                ) => self.on_share_message(message, now),
                Some(
                    message @ (Message::ControlAsk { .. }
                    | Message::ControlAsked { .. }
                    | Message::ControlAnswer { .. }
                    | Message::ControlEnd { .. }
                    | Message::ControlPaused { .. }),
                ) => {
                    self.on_control_message(message, now);
                }
                _ => {
                    self.drops.bad(now);
                }
            }
        }
    }

    // With the secret this host is one this PC can rejoin without an invite,
    // so it goes on the list with every way to reach it known now.
    fn keep_host(&mut self, secret: &Zeroizing<[u8; 32]>) {
        let now_unix = crate::unix_now();
        let reached = self.reached();
        let stored = &self.stored;
        if self.hosts.get(&self.host_key).is_some() {
            self.hosts.update(&self.host_key, |host| {
                host.secret = secret.clone();
                if stored.from_invite {
                    host.candidates.clone_from(&stored.candidates);
                    host.address_name.clone_from(&stored.address_name);
                }
                host.last_reached = reached.or(host.last_reached);
                host.last_seen = now_unix;
                true
            });
            return;
        }
        let (room_name, host_name) = self.roster_names();
        let host = KnownHost {
            host_key: self.host_key,
            room_name,
            host_name,
            secret: secret.clone(),
            candidates: stored.candidates.clone(),
            address_name: stored.address_name.clone(),
            last_reached: reached,
            manual: None,
            last_seen: now_unix,
        };
        log!(
            self.log,
            "the host is a known host now, and can be joined again without an invite"
        );
        if let Some(gone) = self.hosts.add(host) {
            log!(
                self.log,
                "known host {} made way for it, the one seen longest ago",
                keys::fingerprint(&gone.host_key)
            );
        }
    }

    // What the host says its addresses are now, kept for the next rejoin. A
    // host can send new ones in every packet, so the line about them is
    // held to the per-source limit.
    fn host_addresses(
        &mut self,
        candidates: &[Candidate],
        address_name: Option<String>,
        now: Instant,
    ) {
        let candidates = known::clean_candidates(candidates);
        let mut changed = false;
        self.hosts.update(&self.host_key, |host| {
            if host.candidates == candidates && host.address_name == address_name {
                return false;
            }
            host.candidates.clone_from(&candidates);
            host.address_name.clone_from(&address_name);
            changed = true;
            true
        });
        if changed
            && self.log.is_on()
            && let Some(from) = self.host_addr
            && self.strays.allow(from.ip(), now, &self.log)
        {
            log!(
                self.log,
                "the host's addresses, kept for rejoining: {}; address name {}",
                log::candidates(&candidates),
                address_name.as_deref().unwrap_or("none")
            );
        }
    }

    fn note_names(&mut self) {
        let (room_name, host_name) = self.roster_names();
        self.hosts.update(&self.host_key, |host| {
            let changed = host.room_name != room_name || host.host_name != host_name;
            host.room_name = room_name;
            host.host_name = host_name;
            changed
        });
    }

    // A session the host took, or the host heard at a new address.
    fn note_reached(&mut self) {
        let Some(addr) = self.reached() else {
            return;
        };
        let now_unix = crate::unix_now();
        self.hosts.update(&self.host_key, |host| {
            let changed = host.last_reached != Some(addr) || host.last_seen != now_unix;
            host.last_reached = Some(addr);
            host.last_seen = now_unix;
            changed
        });
    }

    fn reached(&self) -> Option<SocketAddr> {
        self.host_addr
            .map(known::plain)
            .filter(|addr| known::reachable(*addr))
    }

    fn roster_names(&self) -> (String, String) {
        let host_name = self
            .roster
            .entries
            .iter()
            .find(|entry| entry.is_host)
            .map_or(control::PERSON_FALLBACK, |entry| entry.name.as_str());
        (
            control::clean(&self.roster.room, control::ROOM_FALLBACK),
            control::clean(host_name, control::PERSON_FALLBACK),
        )
    }

    fn send_ping(&mut self, now: Instant, socket: &Socket) {
        // While the host is quiet its address may have changed; a copy to
        // every address from the invite and its name lets it follow by the
        // roaming rule.
        let mut targets: Vec<SocketAddr> = self.host_addr.into_iter().collect();
        if self.state == LinkState::Reconnecting {
            for addr in self.candidates.iter().chain(&self.from_name) {
                if !targets.contains(addr) {
                    targets.push(*addr);
                }
            }
        }
        let Some(session) = self.sessions.current.as_mut() else {
            return;
        };
        let ping = self.link.ping(self.clock);
        peer::send_on(
            socket,
            session,
            Channel::Ping,
            &ping,
            &targets,
            &mut self.traffic,
        );
        self.link.next_ping = now + self.ping_every(now);
    }

    fn ping_every(&self, now: Instant) -> Duration {
        let sending = self.sent_lately(now);
        self.link.ping_every(sending, now, &self.timers)
    }

    fn flush(&mut self, now: Instant, socket: &Socket) {
        if let (Some(session), Some(addr)) = (self.sessions.current.as_mut(), self.host_addr) {
            self.link
                .flush(socket, session, &[addr], now, &mut self.traffic);
        }
    }

    fn set_host_addr(&mut self, addr: SocketAddr) {
        self.host_addr = Some(addr);
        self.path = Some(numbers::path_word(net::addrs::path_of(addr)));
    }

    fn back_to_ladder(&mut self, now: Instant) {
        self.sessions.clear();
        self.attempts.clear();
        self.next_attempt = Some(now);
    }

    fn stop_sending(&mut self) {
        self.state = LinkState::Closed;
        self.control_over(crate::remote::ControlEnd::Closed);
        self.share_over();
        self.sessions.clear();
        self.talk.keep_only(&[], Instant::now());
        self.talk.shared().room_over();
        self.attempts.clear();
        self.next_attempt = None;
    }

    // The first round of a ladder gets a line, then on_timer writes one
    // every SUMMARY_EVERY while nothing answers. It keeps its own time:
    // checked only when a try goes out, the 2 s slow retry stretches 5 s
    // into 6. A rekey has its answer written down instead.
    fn note_try(&mut self, kind: InitKind, targets: &[SocketAddr], now: Instant) {
        if kind == InitKind::Rekey {
            return;
        }
        self.tries += 1;
        if self.tries == 1 {
            log!(
                self.log,
                "{} initiation sent to {}",
                init_word(kind),
                list(targets)
            );
            self.next_summary = Some(now + SUMMARY_EVERY);
        }
    }

    // A response or cookie reply that was not taken. Anyone can send one, so
    // these stay within the per-source limit.
    fn refused(&mut self, what: &str, packet: &[u8], from: SocketAddr, now: Instant, why: &str) {
        if self.log.is_on() && self.strays.allow(from.ip(), now, &self.log) {
            self.log.line(format!(
                "from {from}, {} bytes: {what}, refused: {why}",
                packet.len()
            ));
        }
    }

    fn note_connected(&self, first: bool) {
        let Some(addr) = self.host_addr else {
            return;
        };
        let path = self.path.map_or("unknown", path_text);
        let handshake = self.handshake_ms.unwrap_or_default();
        match self.connect_ms {
            Some(connect) if first => log!(
                self.log,
                "connected to the host at {addr}, path {path}, handshake {handshake:.1} ms, connect {connect:.1} ms"
            ),
            _ => log!(
                self.log,
                "connected again to the host at {addr}, path {path}, handshake {handshake:.1} ms"
            ),
        }
    }
}

// Shown under this PC's name in the chat of a host on a test build from
// before version numbers.
fn unversioned_host_line() -> String {
    let this = invite::VERSION;
    format!(
        "I have Booth {this} and this room runs a test build of Booth made before the first release, so I could not join. Get Booth {this} from {}.",
        invite::RELEASES_PAGE
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::known::Manual;
    use crate::testing::{Wire, ping, seal};
    use channels::Reliable;
    use invite::{Candidate, CandidateKind, Mapping, ReplyCode};
    use session::{CookieChecker, Session};
    use std::net::{Ipv4Addr, SocketAddrV4};

    // The host, played by hand.
    struct FakeHost {
        identity: Identity,
        wire: Wire,
        invite_secret: [u8; 16],
        peer_secret: [u8; 32],
        reliable: Reliable,
        chat: Reliable,
    }

    impl FakeHost {
        fn new() -> FakeHost {
            FakeHost {
                identity: Identity::generate(),
                wire: Wire::new(),
                invite_secret: [3; 16],
                peer_secret: [5; 32],
                reliable: Reliable::new(),
                chat: Reliable::new(),
            }
        }

        fn invite(&self) -> Invite {
            Invite {
                host_key: *self.identity.public(),
                invite_id: [1; 8],
                secret: self.invite_secret,
                multi_use: false,
                expires_at: crate::unix_now() + 600,
                candidates: vec![Candidate {
                    kind: CandidateKind::Lan,
                    addr: self.wire.addr(),
                }],
                mapping: Mapping::Unknown,
                mapped: false,
                mapped_verified: false,
                second_router: false,
                hostname: None,
            }
        }

        // Answers every initiation that reached it, oldest first.
        fn answer_all(&mut self, now: Instant) -> Vec<(InitKind, Session, Vec<u8>)> {
            let own = *self.identity.public();
            let mut out = Vec::new();
            for packet in self.wire.packets() {
                let Ok(incoming) =
                    session::read_initiation(&self.identity.private_bytes(), &own, &packet)
                else {
                    continue;
                };
                let kind = incoming.kind;
                let psk = match kind {
                    InitKind::Invite(_) => {
                        session::invite_psk(&self.invite_secret, &own, &incoming.remote_public)
                    }
                    InitKind::Known | InitKind::Rekey => Zeroizing::new(self.peer_secret),
                };
                let index = peer::random_index(|_| false);
                let (session, response) = incoming.accept(&psk, index, now).expect("accept");
                out.push((kind, session, response));
            }
            out
        }

        // How many of the packets that reached it open on `session`. The
        // first one that does lets the host send on it.
        fn open_all(&self, session: &mut Session) -> usize {
            let mut plain = Vec::new();
            self.wire
                .packets()
                .iter()
                .filter(|packet| session.decrypt(packet, &mut plain).is_ok())
                .count()
        }

        // What a host sends first on every new link.
        fn hello(&mut self, session: &mut Session, now: Instant) -> Vec<u8> {
            let hello = Message::Hello {
                version: invite::VERSION,
                name: String::from("Mara"),
                reached: None,
            };
            self.say(session, &hello, now)
        }

        fn send_secret(&mut self, session: &mut Session, now: Instant) -> Vec<u8> {
            let secret = Message::PeerSecret {
                secret: Zeroizing::new(self.peer_secret),
            };
            self.say(session, &secret, now)
        }

        fn say(&mut self, session: &mut Session, message: &Message, now: Instant) -> Vec<u8> {
            self.reliable.send(&message.encode()).expect("queued");
            let frame = self.reliable.poll_transmit(now, None).expect("a frame");
            seal(session, Channel::Control, &frame)
        }
    }

    struct Rig {
        client: Client,
        socket: Socket,
        host: FakeHost,
    }

    impl Rig {
        fn new(timers: Timers, start: Instant) -> Rig {
            Rig::with_log(timers, start, Log::off())
        }

        fn with_log(timers: Timers, start: Instant, log: Log) -> Rig {
            Rig::editing(timers, start, log, |_| {})
        }

        fn editing(
            timers: Timers,
            start: Instant,
            log: Log,
            edit: impl FnOnce(&mut Invite),
        ) -> Rig {
            let host = FakeHost::new();
            let socket = Socket::bind(0, Log::off()).expect("bind the client socket");
            let mut invite = host.invite();
            edit(&mut invite);
            let client = Client::new(ClientSetup {
                identity: Arc::new(Identity::generate()),
                name: "Ana".to_owned(),
                ticket: Ticket::Invite(&invite),
                timers,
                port: socket.local_port(),
                has_ipv6: socket.has_ipv6(),
                lookup: Lookup::default(),
                joined: start,
                hosts: HostBook::new(Vec::new(), true),
                list_problem: None,
                voice: crate::testing::quiet_voice(false),
                speaker: crate::testing::no_speaker(),
                screen: crate::testing::quiet_screen(),
                log,
            });
            Rig {
                client,
                socket,
                host,
            }
        }

        fn deliver(&mut self, packet: &[u8], now: Instant) {
            let from = self.host.wire.addr();
            self.client.on_packet(packet, from, now, &self.socket);
        }

        fn tick(&mut self, now: Instant) {
            self.client.on_timer(now, &self.socket);
        }

        // Joined, holding the PeerSecret, and heard from on its session.
        fn connected(timers: Timers, start: Instant) -> (Rig, Session) {
            Rig::joined(Rig::new(timers, start), start)
        }

        fn joined(mut rig: Rig, start: Instant) -> (Rig, Session) {
            rig.tick(start);
            let (_, mut session, response) = rig.host.answer_all(start).pop().expect("a try");
            rig.deliver(&response, start);
            assert!(rig.host.open_all(&mut session) > 0);
            let hello = rig.host.hello(&mut session, start);
            rig.deliver(&hello, start);
            let secret = rig.host.send_secret(&mut session, start);
            rig.deliver(&secret, start);
            rig.deliver(&ping(&mut session, 0), start);
            assert_eq!(rig.client.state, LinkState::Live);
            assert!(rig.client.secret.is_some());
            (rig, session)
        }
    }

    #[test]
    fn untaken_rekey_is_retried() {
        let timers = Timers {
            rekey_after: Duration::from_secs(10),
            reject_after: Duration::from_secs(30),
            ..Timers::default()
        };
        let start = Instant::now();
        let (mut rig, mut first) = Rig::connected(timers, start);

        let rekey_at = start + timers.rekey_after;
        let mut now = start;
        let mut seq = 1;
        while now < rekey_at {
            now += timers.ping_idle;
            rig.deliver(&ping(&mut first, seq), now);
            seq += 1;
            rig.tick(now);
        }
        let (kind, _, response) = rig.host.answer_all(now).pop().expect("a rekey");
        assert_eq!(kind, InitKind::Rekey);
        rig.deliver(&response, now);

        // Nothing the client sends on the new session reaches the host, which
        // lets that session go and keeps pinging on the first one.
        while now < rekey_at + REKEY_RETRY {
            now += Duration::from_millis(250);
            rig.deliver(&ping(&mut first, seq), now);
            seq += 1;
            rig.tick(now);
        }
        assert_eq!(rig.client.state, LinkState::Live);
        let (kind, mut second, response) = rig
            .host
            .answer_all(now)
            .pop()
            .expect("the rekey is tried again");
        assert_eq!(kind, InitKind::Rekey);
        rig.deliver(&response, now);

        // The first session is still what the host sends on.
        let bad = rig.client.drops.bad;
        rig.deliver(&ping(&mut first, seq), now);
        assert_eq!(rig.client.drops.bad, bad, "the first session was dropped");

        rig.tick(now + timers.ping_idle);
        assert!(
            rig.host.open_all(&mut second) > 0,
            "the client does not send on the new session"
        );
    }

    #[test]
    fn unconfirmed_rekey_leaves_no_past_deadline() {
        let timers = Timers {
            rekey_after: Duration::from_secs(10),
            reject_after: Duration::from_secs(12),
            ..Timers::default()
        };
        let start = Instant::now();
        let (mut rig, mut first) = Rig::connected(timers, start);
        let mut now = start;
        let mut seq = 1;
        while now < start + timers.rekey_after {
            now += timers.ping_idle;
            rig.deliver(&ping(&mut first, seq), now);
            seq += 1;
            rig.tick(now);
        }
        let (_, _, response) = rig.host.answer_all(now).pop().expect("a rekey");
        rig.deliver(&response, now);

        // The host never takes the new session and the first one runs out
        // while the client is still retrying.
        while now < start + timers.reject_after {
            now += Duration::from_millis(500);
            rig.tick(now);
        }
        assert!(rig.client.sessions.previous.is_none());
        let next = rig.client.next_deadline().expect("a deadline");
        assert!(
            next > now,
            "the timer thread would spin: deadline {next:?}, now {now:?}"
        );
    }

    #[test]
    fn still_trying_after_dead_answer() {
        let timers = Timers::default();
        let start = Instant::now();
        let mut rig = Rig::new(timers, start);
        rig.tick(start);
        let (_, _, response) = rig.host.answer_all(start).pop().expect("a try");
        rig.deliver(&response, start);

        // The host answered but never sends on the session, then ignores
        // every try after it.
        let fell_back = start + timers.reconnecting_after;
        let mut now = start;
        while now < fell_back + FAST_PHASE {
            now += Duration::from_millis(100);
            rig.tick(now);
            if now < fell_back + FAST_PHASE - Duration::from_millis(100) {
                assert_eq!(rig.client.notice, None, "at {:?}", now - start);
            }
        }
        assert_eq!(rig.client.state, LinkState::Connecting);
        assert_eq!(rig.client.notice, Some(Notice::StillTrying));
    }

    // Driven deadline to deadline, as the timer thread does.
    #[test]
    fn no_answer_summed_up_every_5_s() {
        let (log, captured) = Log::capture(1024);
        let start = Instant::now();
        let mut rig = Rig::with_log(Timers::default(), start, log);
        let mut now = start;
        let mut summaries = Vec::new();
        while now < start + Duration::from_millis(10_500) {
            rig.tick(now);
            for line in captured.lines() {
                if line.starts_with("no answer yet after ") {
                    summaries.push(now - start);
                }
            }
            now = rig
                .client
                .next_deadline()
                .expect("the ladder has a next step")
                .max(now + Duration::from_millis(1));
        }
        assert_eq!(summaries, [SUMMARY_EVERY, SUMMARY_EVERY * 2]);
    }

    #[test]
    fn a_failed_socket_closes_the_link() {
        let start = Instant::now();
        let (mut rig, _) = Rig::connected(Timers::default(), start);
        rig.client.socket_failed();
        let view = rig.client.view(start, 0);
        assert_eq!(view.notice, Some(Notice::SocketFailed));
        assert_eq!(view.strip.state, LinkState::Closed);
        assert_eq!(rig.client.next_deadline(), None);
    }

    // Where the host sees this client: a loopback socket's port.
    fn client_addr(rig: &Rig) -> SocketAddr {
        SocketAddr::from((Ipv4Addr::LOCALHOST, rig.socket.local_port()))
    }

    fn stranger_initiation(host_key: &[u8; 32], index: u32) -> Vec<u8> {
        let stranger = Identity::generate();
        Initiation::start(
            &stranger.private_bytes(),
            stranger.public(),
            host_key,
            &[0; 32],
            InitKind::Known,
            Tai64N::now(),
            index,
        )
        .expect("start an initiation")
        .1
    }

    // The host at a second address in the invite, and a second address in
    // the client's name for it.
    fn two_addresses(start: Instant) -> (Rig, Wire) {
        let other = Wire::new();
        let other_addr = other.addr();
        let rig = Rig::editing(Timers::default(), start, Log::off(), |invite| {
            invite.candidates.push(Candidate {
                kind: CandidateKind::Lan,
                addr: other_addr,
            });
        });
        (rig, other)
    }

    #[test]
    fn cookie_used_until_it_runs_out() {
        let start = Instant::now();
        let (mut rig, other) = two_addresses(start);
        let mut checker = CookieChecker::new(rig.host.identity.public(), start);
        let client = client_addr(&rig);
        rig.tick(start);
        let tried = rig.host.wire.packets();
        assert_eq!(tried.len(), 1);
        assert_eq!(other.packets(), tried, "one initiation to both addresses");
        let reply = checker
            .cookie_reply(&tried[0], client, start)
            .expect("a cookie reply");
        rig.deliver(&reply, start);

        // Still one to both, so a host that hears it over both paths
        // answers it once.
        let next = start + Timers::default().handshake_fast_retry;
        rig.tick(next);
        let here = rig.host.wire.packets();
        assert_eq!(here.len(), 1);
        assert_eq!(other.packets(), here);
        assert!(checker.has_valid_mac2(&here[0], client, next));
        assert_eq!(rig.client.attempts.len(), 2);

        // Kept 115 s, then left off.
        rig.client.attempts.clear();
        let stale = start + COOKIE_KEPT;
        rig.client.next_attempt = Some(stale);
        rig.tick(stale);
        let here = rig.host.wire.packets();
        assert_eq!(here.len(), 1);
        assert!(!checker.has_valid_mac2(&here[0], client, stale));
        assert_eq!(other.packets(), here);
    }

    // A host that got a different initiation over each of two paths would
    // do the key math for both and answer both, and the second answer would
    // be dropped here as malformed.
    #[test]
    fn two_cookies_still_one_initiation() {
        let start = Instant::now();
        let (mut rig, other) = two_addresses(start);
        // Two secrets stand in for the host seeing this PC at a different
        // address on each path.
        let host_key = *rig.host.identity.public();
        let mut here_checker = CookieChecker::new(&host_key, start);
        let mut there_checker = CookieChecker::new(&host_key, start);
        let client = client_addr(&rig);
        rig.tick(start);
        let tried = rig.host.wire.packets().pop().expect("a try");
        let here_reply = here_checker
            .cookie_reply(&tried, client, start)
            .expect("a reply");
        let there_reply = there_checker
            .cookie_reply(&tried, client, start)
            .expect("a reply");
        rig.deliver(&here_reply, start);
        rig.client
            .on_packet(&there_reply, other.addr(), start, &rig.socket);
        other.packets();

        // Before either path has answered, the newest cookie goes.
        let timers = Timers::default();
        let next = start + timers.handshake_fast_retry;
        rig.tick(next);
        let here = rig.host.wire.packets();
        assert_eq!(here.len(), 1);
        assert_eq!(other.packets(), here, "one initiation to both addresses");
        assert_eq!(rig.client.attempts.len(), 2);
        assert!(there_checker.has_valid_mac2(&here[0], client, next));
        assert!(!here_checker.has_valid_mac2(&here[0], client, next));

        // After, the cookie of the address that answered.
        rig.client.host_addr = Some(rig.host.wire.addr());
        let after = next + timers.handshake_fast_retry;
        rig.client.next_attempt = Some(after);
        rig.tick(after);
        let here = rig.host.wire.packets();
        assert_eq!(here.len(), 1);
        assert_eq!(other.packets(), here);
        assert!(here_checker.has_valid_mac2(&here[0], client, after));
    }

    #[test]
    fn untrusted_cookie_replies_dropped() {
        let start = Instant::now();
        let (log, captured) = Log::capture(256);
        let mut rig = Rig::with_log(Timers::default(), start, log);
        let host_key = *rig.host.identity.public();
        let mut checker = CookieChecker::new(&host_key, start);
        let client = client_addr(&rig);
        rig.tick(start);
        let tried = rig.host.wire.packets().pop().expect("a try");
        let index = rig.client.attempts[0].initiation.sender_index();

        // Someone else's try: its reply names an index no try of ours has.
        let theirs = stranger_initiation(&host_key, index.wrapping_add(1));
        let unknown = checker
            .cookie_reply(&theirs, client, start)
            .expect("a reply");
        // Our index, sealed against the mac1 of another initiation, as made
        // by someone who never saw ours.
        let forged = stranger_initiation(&host_key, index);
        let wrong_ad = checker
            .cookie_reply(&forged, client, start)
            .expect("a reply");
        let mut tampered = checker
            .cookie_reply(&tried, client, start)
            .expect("a reply");
        tampered[40] ^= 1;
        let bad = rig.client.drops.bad;
        for reply in [&unknown, &wrong_ad, &tampered] {
            rig.deliver(reply, start);
        }
        // A good one, from an address no try went to.
        let good = checker
            .cookie_reply(&tried, client, start)
            .expect("a reply");
        let elsewhere = SocketAddr::from((Ipv4Addr::LOCALHOST, 9));
        rig.client.on_packet(&good, elsewhere, start, &rig.socket);
        assert_eq!(rig.client.drops.bad, bad + 4);

        let next = start + Timers::default().handshake_fast_retry;
        rig.tick(next);
        let again = rig.host.wire.packets().pop().expect("the next try");
        assert!(
            !checker.has_valid_mac2(&again, client, next),
            "a refused reply left a cookie behind"
        );
        let lines = captured.lines();
        let refused = |why: &str| {
            lines
                .iter()
                .filter(|line| line.ends_with(&format!("cookie reply, refused: {why}")))
                .count()
        };
        assert_eq!(refused("it answers no try still open"), 1, "{lines:?}");
        assert_eq!(
            refused("it does not open with the host key and the mac1 of the try it names"),
            2,
            "{lines:?}"
        );
        assert_eq!(refused("no try went to that address"), 1, "{lines:?}");
    }

    // The host learns from this that a friend got in through its mapped port.
    #[test]
    fn hello_names_reached_address() {
        let start = Instant::now();
        let mut rig = Rig::new(Timers::default(), start);
        rig.tick(start);
        let (_, mut session, response) = rig.host.answer_all(start).pop().expect("a try");
        rig.deliver(&response, start);
        let mut reliable = Reliable::new();
        let mut plain = Vec::new();
        for packet in rig.host.wire.packets() {
            if session.decrypt(&packet, &mut plain).is_ok()
                && let Some(Plain::Control(frame)) = peer::read_plain(&plain)
            {
                reliable.receive(frame, start).expect("a good frame");
            }
        }
        let hello = reliable
            .next_delivered()
            .and_then(|bytes| Message::decode(&bytes));
        match hello {
            Some(Message::Hello {
                version,
                name,
                reached,
            }) => {
                assert_eq!(version, invite::VERSION);
                assert_eq!(name, "Ana");
                assert_eq!(reached, Some(rig.host.wire.addr()));
            }
            other => panic!("{other:?}"),
        }
    }

    #[derive(Debug)]
    enum Took {
        Control(Message),
        Chat(ChatMessage),
    }

    // Both streams this PC sent since the last look, in the order the host
    // takes them.
    fn host_took(host: &mut FakeHost, session: &mut Session) -> Vec<Took> {
        let mut plain = Vec::new();
        let mut out = Vec::new();
        for packet in host.wire.packets() {
            if session.decrypt(&packet, &mut plain).is_err() {
                continue;
            }
            let stream = match peer::read_plain(&plain) {
                Some(Plain::Control(frame)) => host.reliable.receive(frame, Instant::now()),
                Some(Plain::Chat(frame)) => host.chat.receive(frame, Instant::now()),
                _ => continue,
            };
            stream.expect("a good frame");
            while let Some(bytes) = host.reliable.next_delivered() {
                out.extend(Message::decode(&bytes).map(Took::Control));
            }
            while let Some(bytes) = host.chat.next_delivered() {
                out.extend(ChatMessage::decode(&bytes).map(Took::Chat));
            }
        }
        out
    }

    // The control messages this PC sent since the last look, as the host
    // reads them.
    fn told_host(host: &mut FakeHost, session: &mut Session) -> Vec<Message> {
        let took = host_took(host, session).into_iter();
        took.filter_map(|took| match took {
            Took::Control(message) => Some(message),
            Took::Chat(_) => None,
        })
        .collect()
    }

    // Up to the point where the host would say its Hello.
    fn answered(start: Instant) -> (Rig, Session) {
        let mut rig = Rig::new(Timers::default(), start);
        rig.tick(start);
        let (_, mut session, response) = rig.host.answer_all(start).pop().expect("a try");
        rig.deliver(&response, start);
        let said = told_host(&mut rig.host, &mut session);
        assert!(matches!(said[..], [Message::Hello { .. }]), "{said:?}");
        (rig, session)
    }

    #[test]
    fn other_protocol_host_is_left() {
        let start = Instant::now();
        let (mut rig, mut session) = answered(start);
        let later = Version {
            major: 0,
            minor: 9,
            patch: 0,
        };
        let hello = Message::OtherHello {
            protocol: invite::PROTOCOL + 1,
            version: later,
            name: String::from("Mara"),
        };
        let packet = rig.host.say(&mut session, &hello, start);
        rig.deliver(&packet, start);
        let secret = rig.host.send_secret(&mut session, start);
        rig.deliver(&secret, start);
        assert_eq!(
            rig.client.notice,
            Some(Notice::OtherVersion {
                protocol: invite::PROTOCOL + 1,
                version: later
            })
        );
        assert_eq!(rig.client.state, LinkState::Closed);
        assert!(
            rig.client.secret.is_none(),
            "nothing after that Hello is read"
        );
        let said = told_host(&mut rig.host, &mut session);
        assert!(matches!(said[..], [Message::Bye]), "{said:?}");
    }

    // A test build from before version numbers opens with the secret. It
    // takes chat only after a Hello it can read, and its room is told why
    // this PC came and went, ahead of the Bye.
    #[test]
    fn unversioned_host_is_left() {
        let start = Instant::now();
        let (mut rig, mut session) = answered(start);
        let secret = rig.host.send_secret(&mut session, start);
        rig.deliver(&secret, start);
        assert_eq!(rig.client.notice, Some(Notice::UnversionedHost));
        assert_eq!(rig.client.state, LinkState::Closed);
        assert!(rig.client.secret.is_none());
        let took = host_took(&mut rig.host, &mut session);
        let this = invite::VERSION;
        let why = format!(
            "I have Booth {this} and this room runs a test build of Booth made before the first release, so I could not join. Get Booth {this} from {}.",
            invite::RELEASES_PAGE
        );
        assert!(
            matches!(
                &took[..],
                [
                    Took::Control(Message::UnversionedHello { name }),
                    Took::Chat(ChatMessage::Say { text, .. }),
                    Took::Control(Message::Bye),
                ] if name == "Ana" && *text == why
            ),
            "{took:?}"
        );
    }

    // Only the protocol decides; another Booth version of it is welcome.
    #[test]
    fn same_protocol_other_version_joins() {
        let start = Instant::now();
        let (mut rig, mut session) = answered(start);
        let hello = Message::Hello {
            version: Version {
                major: 0,
                minor: 1,
                patch: 99,
            },
            name: String::from("Mara"),
            reached: None,
        };
        let packet = rig.host.say(&mut session, &hello, start);
        rig.deliver(&packet, start);
        let secret = rig.host.send_secret(&mut session, start);
        rig.deliver(&secret, start);
        rig.deliver(&ping(&mut session, 0), start);
        assert_eq!(rig.client.notice, None);
        assert_eq!(rig.client.state, LinkState::Live);
        assert!(rig.client.secret.is_some());
    }

    const OUTSIDE: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(203, 0, 113, 9), 52000);
    const DUE: Duration = Duration::from_secs(5);

    type StunSaw = Option<(net::stun::Mapping, Option<SocketAddrV4>)>;

    struct Joining {
        client: Client,
        socket: Socket,
        host: Wire,
        start: Instant,
    }

    impl Joining {
        // A client whose tries go to `host`, which never answers, and whose
        // STUN round has finished with `stun`, or not at all.
        fn new(stun: StunSaw, edit: impl FnOnce(&mut Invite)) -> Joining {
            let host = Wire::new();
            let socket = Socket::bind(0, Log::off()).expect("bind the client socket");
            let mut invite = Invite {
                host_key: *Identity::generate().public(),
                invite_id: [1; 8],
                secret: [3; 16],
                multi_use: false,
                expires_at: crate::unix_now() + 600,
                candidates: vec![Candidate {
                    kind: CandidateKind::Lan,
                    addr: host.addr(),
                }],
                mapping: Mapping::Unknown,
                mapped: false,
                mapped_verified: false,
                second_router: false,
                hostname: None,
            };
            edit(&mut invite);
            let start = Instant::now();
            let mut client = Client::new(ClientSetup {
                identity: Arc::new(Identity::generate()),
                name: "Ana".to_owned(),
                ticket: Ticket::Invite(&invite),
                timers: Timers::default(),
                port: socket.local_port(),
                has_ipv6: socket.has_ipv6(),
                lookup: Lookup::default(),
                joined: start,
                hosts: HostBook::new(Vec::new(), true),
                list_problem: None,
                voice: crate::testing::quiet_voice(false),
                speaker: crate::testing::no_speaker(),
                screen: crate::testing::quiet_screen(),
                log: Log::off(),
            });
            if let Some((mapping, seen)) = stun {
                client.stun.set_settled(mapping, seen);
            }
            Joining {
                client,
                socket,
                host,
                start,
            }
        }

        fn tick(&mut self, after: Duration) -> View {
            let now = self.start + after;
            self.client.on_timer(now, &self.socket);
            self.client.view(now, 0)
        }
    }

    fn easy() -> StunSaw {
        Some((net::stun::Mapping::Easy, Some(OUTSIDE)))
    }

    #[test]
    fn code_shows_with_still_trying() {
        let mut joining = Joining::new(easy(), |_| {});
        let early = joining.tick(DUE - Duration::from_millis(1));
        assert_eq!(early.notice, None);
        assert_eq!(early.reply, None);

        let view = joining.tick(DUE);
        assert_eq!(view.notice, Some(Notice::StillTrying));
        let reply = view.reply.expect("a reply code");
        assert_eq!(
            reply.state,
            ReplyState::Code {
                second_router: false
            }
        );
        assert_eq!(reply.host_port, Some(joining.host.addr().port()));
        let code = ReplyCode::decode(&reply.code).expect("the code decodes");
        assert_eq!(code.answers, Answers::Invite([1; 8]));
        assert_eq!(code.client_key, *joining.client.identity.public());
        assert_eq!(code.outside_v4, Some(OUTSIDE));
        assert_eq!(code.mapping, Mapping::Easy);
        assert_eq!(code.expires_at, reply.expires_at_unix);
        let left = reply.expires_at_unix.saturating_sub(crate::unix_now());
        assert!((299..=300).contains(&left), "{left} s left");
        // The tries go on while the code is shown.
        assert!(!joining.host.packets().is_empty());
    }

    #[test]
    fn expired_code_and_new_code() {
        for second_router in [false, true] {
            let mut joining = Joining::new(easy(), |invite| invite.second_router = second_router);
            let first = joining.tick(DUE).reply.expect("a reply code");
            assert_eq!(first.state, ReplyState::Code { second_router });

            let almost = joining.tick(DUE + REPLY_LIFETIME - Duration::from_millis(1));
            assert_eq!(
                almost.reply.expect("still the code").state,
                ReplyState::Code { second_router }
            );
            let expired = joining
                .tick(DUE + REPLY_LIFETIME)
                .reply
                .expect("the expired screen");
            assert_eq!(expired.state, ReplyState::Expired { second_router });
            assert!(expired.code.is_empty());

            let pressed = joining.start + DUE + REPLY_LIFETIME + Duration::from_secs(1);
            assert!(joining.client.new_code(pressed));
            let fresh = joining.client.view(pressed, 0).reply.expect("a new code");
            assert_eq!(fresh.state, ReplyState::Code { second_router });
            assert!(!fresh.code.is_empty());
            // Its own five minutes, counted from the press.
            let still = joining.tick(DUE + REPLY_LIFETIME * 2);
            assert_eq!(
                still.reply.expect("the new code").state,
                ReplyState::Code { second_router }
            );
        }
    }

    #[test]
    fn no_code_when_it_cannot_help() {
        let hard = Some((net::stun::Mapping::Hard, Some(OUTSIDE)));
        let nothing = Some((net::stun::Mapping::Unknown, None));
        for (stun, host_hard, want) in [
            (easy(), true, ReplyState::HostHard),
            (hard, false, ReplyState::OwnHard),
            (nothing, false, ReplyState::NoAddress),
        ] {
            let mut joining = Joining::new(stun, |invite| {
                if host_hard {
                    invite.mapping = Mapping::Hard;
                }
            });
            let reply = joining.tick(DUE).reply.expect("a reply screen");
            assert_eq!(reply.state, want);
            assert!(reply.code.is_empty());
            assert!(!joining.client.new_code(joining.start + DUE));
            // Nothing to expire.
            let later = joining.tick(DUE + REPLY_LIFETIME);
            assert_eq!(later.reply.expect("the same screen").state, want);
        }
    }

    // STUN names that take long to look up hold the code up by stun_wait at
    // most, and then it goes out with what STUN has. An answer after that
    // still brings the code.
    #[test]
    fn slow_stun_delays_code_by_stun_wait() {
        let mut joining = Joining::new(None, |_| {});
        let wait = Timers::default().stun_wait;
        let view = joining.tick(DUE);
        assert_eq!(view.notice, Some(Notice::StillTrying));
        assert_eq!(view.reply, None);
        let next = joining.client.next_deadline().expect("a deadline");
        assert!(next <= joining.start + DUE + wait);
        let view = joining.tick(DUE + wait);
        assert_eq!(
            view.reply.expect("a reply screen").state,
            ReplyState::NoAddress
        );

        // The names resolve 2 s later and the server answers at once.
        let late = joining.start + DUE + wait + Duration::from_secs(2);
        let server = Wire::new();
        joining
            .client
            .stun_found(vec![server.addr()], &joining.socket);
        assert!(!joining.client.stun_resolved(late));
        let request = server.packets().pop().expect("a stun request");
        let txid: [u8; 12] = request[8..20].try_into().expect("a transaction id");
        let answer = crate::testing::stun_answer(&txid, OUTSIDE);
        assert!(
            joining
                .client
                .on_packet(&answer, server.addr(), late, &joining.socket)
        );
        let reply = joining.client.view(late, 0).reply.expect("a reply screen");
        assert_eq!(
            reply.state,
            ReplyState::Code {
                second_router: false
            }
        );
        let code = ReplyCode::decode(&reply.code).expect("the code decodes");
        assert_eq!(code.outside_v4, Some(OUTSIDE));
        let left = reply.expires_at_unix.saturating_sub(crate::unix_now());
        assert!((299..=300).contains(&left), "{left} s left");
    }

    #[test]
    fn name_looked_up_after_fast_round() {
        let mut joining = Joining::new(easy(), |invite| {
            invite.hostname = Some(String::from("myroom.duckdns.org"));
        });
        joining.tick(DUE - Duration::from_millis(1));
        assert!(joining.client.name_wanted().is_none());
        let view = joining.tick(DUE);
        assert_eq!(view.notice, Some(Notice::StillTrying));
        let request = joining.client.name_wanted().expect("the lookup starts");
        assert_eq!(request.name, "myroom.duckdns.org");
        joining.tick(DUE + Duration::from_secs(1));
        assert!(joining.client.name_wanted().is_none(), "asked twice");

        // Another loopback address: nothing listens there, and the try
        // stays on this PC.
        let moved: IpAddr = "127.0.0.2".parse().unwrap();
        let outcome = Outcome {
            servers: None,
            result: Ok(net::dns::Resolved {
                addrs: vec![net::dns::Found {
                    ip: moved,
                    source: net::dns::Source::Authoritative,
                }],
                refused: Vec::new(),
            }),
        };
        let now = joining.start + DUE + Duration::from_secs(2);
        assert!(joining.client.name_found(outcome, now));
        let port = joining.host.addr().port();
        let target = SocketAddr::new(moved, port);
        assert_eq!(joining.client.from_name, [target]);
        assert!(joining.client.everywhere().contains(&target));
        assert_eq!(joining.client.next_attempt, Some(now));
        assert!(joining.client.punch_sources.contains(&moved));
    }

    #[test]
    fn the_answer_closes_the_code_screen() {
        let start = Instant::now();
        let mut rig = Rig::new(Timers::default(), start);
        rig.client
            .stun
            .set_settled(net::stun::Mapping::Easy, Some(OUTSIDE));
        rig.tick(start + DUE);
        assert!(rig.client.view(start + DUE, 0).reply.is_some());
        let (_, _, response) = rig.host.answer_all(start + DUE).pop().expect("a try");
        rig.deliver(&response, start + DUE);
        let view = rig.client.view(start + DUE, 0);
        assert_eq!(view.reply, None);
        assert_eq!(view.notice, None);
    }

    #[test]
    fn punches_only_from_host_unanswered() {
        let (log, captured) = Log::capture(64);
        let mut joining = Joining::new(easy(), |_| {});
        joining.client.log = log;
        let now = joining.start;
        joining.client.on_timer(now, &joining.socket);
        joining.host.packets();

        let punch = session::punch_packet(&[7; session::PUNCH_RANDOM_LEN]);
        let from_host = joining.host.addr();
        for _ in 0..3 {
            joining
                .client
                .on_packet(&punch, from_host, now, &joining.socket);
        }
        assert_eq!(joining.client.drops.bad, 0);
        let said: Vec<String> = captured
            .lines()
            .into_iter()
            .filter(|line| line.contains("punch"))
            .collect();
        assert_eq!(
            said,
            [format!(
                "the host's punch packets are arriving from {from_host}"
            )]
        );
        assert!(joining.host.packets().is_empty(), "a punch was answered");

        let stranger: SocketAddr = "198.51.100.4:41000".parse().unwrap();
        joining
            .client
            .on_packet(&punch, stranger, now, &joining.socket);
        joining
            .client
            .on_packet(&punch[..31], from_host, now, &joining.socket);
        assert_eq!(joining.client.drops.bad, 2);
    }

    const NAME: &str = "myroom.duckdns.org";

    fn name_gave(ips: &[&str]) -> Outcome {
        Outcome {
            servers: None,
            result: Ok(net::dns::Resolved {
                addrs: ips
                    .iter()
                    .map(|ip| net::dns::Found {
                        ip: ip.parse().unwrap(),
                        source: net::dns::Source::Authoritative,
                    })
                    .collect(),
                refused: Vec::new(),
            }),
        }
    }

    fn named(timers: Timers, start: Instant) -> (Rig, Session) {
        let rig = Rig::editing(timers, start, Log::off(), |invite| {
            invite.hostname = Some(NAME.to_owned());
        });
        Rig::joined(rig, start)
    }

    // With no name and only silence, the client says it lost the host, and
    // keeps trying underneath with the per-peer secret.
    #[test]
    fn silence_without_name_is_lost_host() {
        let timers = Timers::default();
        let start = Instant::now();
        let (mut rig, _) = Rig::connected(timers, start);
        let quiet = start + timers.reconnecting_after;
        rig.tick(quiet);
        let view = rig.client.view(quiet, 0);
        assert_eq!(view.strip.state, LinkState::Reconnecting);
        assert_eq!(view.notice, None);
        assert!(rig.client.name_wanted().is_none());

        rig.tick(start + timers.lost_after);
        assert_eq!(rig.client.notice, Some(Notice::LostHost));
        rig.host.wire.packets();
        let later = start + timers.lost_after * 3;
        rig.tick(later);
        assert_eq!(rig.client.notice, Some(Notice::LostHost));
        let (kind, _, _) = rig
            .host
            .answer_all(later)
            .pop()
            .expect("the ladder goes on");
        assert_eq!(kind, InitKind::Known);
    }

    // The name still points where the host was, so as far as anyone can
    // tell the host did not move: the lost host notice again.
    #[test]
    fn same_address_from_name_is_lost_host() {
        let timers = Timers::default();
        let start = Instant::now();
        let (mut rig, _) = named(timers, start);
        let quiet = start + timers.reconnecting_after;
        rig.tick(quiet);
        assert!(
            rig.client.name_wanted().is_some(),
            "the name is looked up at once"
        );
        let host_ip = rig.host.wire.addr().ip().to_string();
        assert!(rig.client.name_found(name_gave(&[&host_ip]), quiet));
        assert!(rig.client.from_name.is_empty());
        rig.tick(quiet + NAME_EVERY);
        assert!(
            rig.client.name_wanted().is_some(),
            "looked up again two seconds later"
        );
        rig.tick(start + timers.lost_after * 2);
        assert_eq!(rig.client.notice, Some(Notice::LostHost));
    }

    // The name gave a new address and nothing answered there within
    // lost_after of the first try.
    #[test]
    fn silent_new_address_is_host_moved() {
        let timers = Timers::default();
        let start = Instant::now();
        let (mut rig, _) = named(timers, start);
        let quiet = start + timers.reconnecting_after;
        rig.tick(quiet);
        assert!(rig.client.name_wanted().is_some());
        let tried = quiet + Duration::from_millis(50);
        assert!(rig.client.name_found(name_gave(&["127.0.0.2"]), tried));
        let port = rig.host.wire.addr().port();
        assert_eq!(
            rig.client.from_name,
            [SocketAddr::from(([127, 0, 0, 2], port))]
        );
        // A ping on the session goes there on this timer pass.
        assert_eq!(rig.client.link.next_ping, tried);

        rig.tick(start + timers.lost_after);
        assert_eq!(rig.client.notice, Some(Notice::LostHost));
        let moved = tried + timers.lost_after;
        assert!(rig.client.next_deadline().is_some_and(|at| at <= moved));
        rig.tick(moved - Duration::from_millis(1));
        assert_eq!(rig.client.notice, Some(Notice::LostHost));
        rig.tick(moved);
        assert_eq!(rig.client.view(moved, 0).notice, Some(Notice::HostMoved));

        // The host answers after all: back, with the time it took.
        rig.host.wire.packets();
        let back = moved + timers.handshake_slow_retry;
        rig.tick(back);
        let (kind, mut session, response) = rig.host.answer_all(back).pop().expect("a try");
        assert_eq!(kind, InitKind::Known);
        rig.deliver(&response, back);
        assert!(rig.host.open_all(&mut session) > 0);
        rig.deliver(&ping(&mut session, 0), back);
        let view = rig.client.view(back, 0);
        assert_eq!(view.strip.state, LinkState::Live);
        assert_eq!(view.notice, None);
        let took = view.numbers.reconnect_ms.expect("a reconnect time");
        let silence = numbers::millis(back - start);
        assert!((took - silence).abs() < 1.0, "{took} ms, not {silence} ms");
        assert_eq!(rig.client.next_name, None, "no more lookups");
    }

    // The name's nameserver times out on one of the lookups every 2 s. The
    // new address it gave before is still where the host may be.
    #[test]
    fn failed_lookup_keeps_old_addresses() {
        let timers = Timers::default();
        let start = Instant::now();
        let (mut rig, _) = named(timers, start);
        let quiet = start + timers.reconnecting_after;
        rig.tick(quiet);
        assert!(rig.client.name_found(name_gave(&["127.0.0.2"]), quiet));
        let port = rig.host.wire.addr().port();
        let moved = SocketAddr::from(([127, 0, 0, 2], port));
        assert_eq!(rig.client.from_name, [moved]);

        for err in [
            net::dns::DnsError::Unanswered(NAME.to_owned()),
            net::dns::DnsError::NoSuchName(NAME.to_owned()),
            net::dns::DnsError::NoAddress(NAME.to_owned()),
        ] {
            let failed = Outcome {
                servers: None,
                result: Err(err),
            };
            let later = quiet + NAME_EVERY;
            rig.client.link.next_ping = later + timers.ping_idle;
            assert!(rig.client.name_found(failed, later));
            assert_eq!(rig.client.from_name, [moved]);
            assert!(rig.client.everywhere().contains(&moved));
            assert_eq!(
                rig.client.link.next_ping,
                later + timers.ping_idle,
                "not tried again as if it were new"
            );
        }
    }

    // A STUN server on a wire, with the first round answered.
    fn stun_on(rig: &mut Rig, server: &Wire, seen: SocketAddrV4, now: Instant) {
        rig.client.stun_found(vec![server.addr()], &rig.socket);
        assert!(!rig.client.stun_resolved(now));
        answer_stun(rig, server, seen, now);
        assert!(rig.client.stun.is_settled());
    }

    fn answer_stun(rig: &mut Rig, server: &Wire, seen: SocketAddrV4, now: Instant) {
        let request = server.packets().pop().expect("a stun request");
        let txid: [u8; 12] = request[8..20].try_into().expect("a transaction id");
        let answer = crate::testing::stun_answer(&txid, seen);
        rig.client
            .on_packet(&answer, server.addr(), now, &rig.socket);
    }

    const MOVED: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(203, 0, 113, 9), 52001);

    // Silent for reconnecting_after, and STUN says this PC's outside port
    // changed.
    fn moved_away(rig: &mut Rig, start: Instant) -> Instant {
        let server = Wire::new();
        stun_on(rig, &server, OUTSIDE, start);
        let quiet = start + rig.client.timers.reconnecting_after;
        rig.tick(quiet);
        answer_stun(rig, &server, MOVED, quiet);
        quiet
    }

    // The code block above the people list, gone on the first packet back.
    #[test]
    fn moved_shows_code_in_room() {
        let start = Instant::now();
        let (mut rig, mut session) = Rig::connected(Timers::default(), start);
        let quiet = moved_away(&mut rig, start);
        let view = rig.client.view(quiet, 0);
        assert_eq!(view.strip.state, LinkState::Reconnecting);
        let reply = view.reply.expect("the code");
        assert_eq!(
            reply.state,
            ReplyState::Code {
                second_router: false
            }
        );
        let code = ReplyCode::decode(&reply.code).expect("the code decodes");
        assert_eq!(code.answers, Answers::Rejoin);
        assert_eq!(code.client_key, *rig.client.identity.public());
        assert_eq!(code.outside_v4, Some(MOVED));
        let change = view.numbers.address_change.expect("the change");
        assert!(change.this_pc);
        assert_eq!(
            (change.from, change.to),
            (SocketAddr::V4(OUTSIDE), SocketAddr::V4(MOVED))
        );

        let back = quiet + Duration::from_secs(1);
        rig.deliver(&ping(&mut session, 1), back);
        let view = rig.client.view(back, 0);
        assert_eq!(view.strip.state, LinkState::Live);
        assert_eq!(view.reply, None);
    }

    // The router is still coming back when the silence starts, so the round
    // it asks gets nothing. The client asks again every stun_retry, and the
    // answer that comes once the router is back shows the move.
    #[test]
    fn stun_retried_until_it_answers() {
        let timers = Timers::default();
        let start = Instant::now();
        let (mut rig, _) = Rig::connected(timers, start);
        let server = Wire::new();
        stun_on(&mut rig, &server, OUTSIDE, start);
        let quiet = start + timers.reconnecting_after;
        rig.tick(quiet);
        assert_eq!(server.packets().len(), 1, "the silence asks");
        // Windows saw the router go. The round out answers that too.
        let noticed = quiet + Duration::from_millis(10);
        rig.client.address_changed(noticed, &rig.socket);
        assert!(
            server.packets().is_empty(),
            "a second round while one is out"
        );

        let unanswered = quiet + timers.stun_wait;
        rig.tick(unanswered);
        let view = rig.client.view(unanswered, 0);
        assert_eq!(view.strip.state, LinkState::Reconnecting);
        assert_eq!(view.numbers.address_change, None);
        assert_eq!(view.reply, None);

        let again = quiet + timers.stun_retry;
        assert!(rig.client.next_deadline().is_some_and(|at| at <= again));
        rig.tick(again);
        answer_stun(&mut rig, &server, MOVED, again);
        let view = rig.client.view(again, 0);
        let change = view.numbers.address_change.expect("the change");
        assert_eq!(
            (change.from, change.to),
            (SocketAddr::V4(OUTSIDE), SocketAddr::V4(MOVED))
        );
        assert!(
            view.reply
                .is_some_and(|reply| matches!(reply.state, ReplyState::Code { .. })),
            "the code shows"
        );
        assert_eq!(rig.client.stun.next_deadline(), None, "nothing more to ask");
    }

    // The host is heard again while the round the silence asked is still
    // out, and STUN never answers it. The silence was the question, so
    // nothing more goes to STUN.
    #[test]
    fn stun_stops_once_host_heard() {
        let timers = Timers::default();
        let start = Instant::now();
        let (log, captured) = Log::capture(256);
        let (mut rig, mut session) = Rig::joined(Rig::with_log(timers, start, log), start);
        let server = Wire::new();
        stun_on(&mut rig, &server, OUTSIDE, start);
        let quiet = start + timers.reconnecting_after;
        rig.tick(quiet);
        assert_eq!(server.packets().len(), 1, "the silence asks");

        let back = quiet + Duration::from_millis(500);
        rig.deliver(&ping(&mut session, 1), back);
        assert_eq!(rig.client.state, LinkState::Live);
        rig.tick(quiet + timers.stun_wait);
        assert_eq!(rig.client.stun.next_deadline(), None, "a retry is waiting");
        rig.tick(quiet + timers.stun_retry);
        assert!(
            server.packets().is_empty(),
            "asked again after the host answered"
        );
        assert!(
            captured
                .lines()
                .iter()
                .any(|line| line == "stun: the host answered, the address check stops")
        );
    }

    #[test]
    fn code_in_room_expires_and_renews() {
        let start = Instant::now();
        let (mut rig, _) = Rig::connected(Timers::default(), start);
        let quiet = moved_away(&mut rig, start);
        let expiry = quiet + REPLY_LIFETIME;
        rig.tick(expiry);
        let view = rig.client.view(expiry, 0);
        assert_eq!(view.notice, Some(Notice::LostHost));
        assert_eq!(
            view.reply.map(|reply| reply.state),
            Some(ReplyState::Expired {
                second_router: false
            })
        );
        assert!(rig.client.new_code(expiry));
        let fresh = rig.client.view(expiry, 0).reply.expect("a new code");
        assert!(matches!(fresh.state, ReplyState::Code { .. }));
        assert!(!fresh.code.is_empty());
    }

    // The host takes this PC back through its mapped port, so the code
    // would only be noise.
    #[test]
    fn no_code_in_room_via_mapped_port() {
        for verified in [true, false] {
            let start = Instant::now();
            let rig = Rig::editing(Timers::default(), start, Log::off(), |invite| {
                invite.candidates[0].kind = CandidateKind::Public;
                invite.mapped = true;
                invite.mapped_verified = verified;
            });
            let (mut rig, _) = Rig::joined(rig, start);
            assert!(rig.client.reached_mapped);
            let quiet = moved_away(&mut rig, start);
            let view = rig.client.view(quiet, 0);
            assert!(view.numbers.address_change.is_some());
            assert_eq!(view.reply, None, "verified {verified}");
            assert!(rig.client.own_moved.is_some());
        }
    }

    // The host's answer to the last ping the client sent it.
    fn pong_to_last_ping(rig: &Rig, session: &mut Session) -> Vec<u8> {
        let mut plain = Vec::new();
        let mut last = None;
        for packet in rig.host.wire.packets() {
            if session.decrypt(&packet, &mut plain).is_ok()
                && let Some(Plain::Ping(PingMessage::Ping { seq, t1 })) = peer::read_plain(&plain)
            {
                last = Some((seq, t1));
            }
        }
        let (seq, t1) = last.expect("a ping from the client");
        let mut payload = Vec::new();
        PingMessage::Pong {
            seq,
            t1,
            t2: t1,
            t3: t1,
        }
        .encode(&mut payload);
        seal(session, Channel::Ping, &payload)
    }

    // A second network plugged in while the first stays up: the host's
    // pings still come in on the old one, while what this PC sends leaves
    // by the new one and a strict router in front of the host drops it.
    // Only a pong to a ping sent after the move shows the host hears it.
    #[test]
    fn host_packets_prove_nothing() {
        let timers = Timers::default();
        let start = Instant::now();
        let (mut rig, mut session) = Rig::connected(timers, start);
        let server = Wire::new();
        stun_on(&mut rig, &server, OUTSIDE, start);

        // A ping out before the move, answered after it.
        let before = start + timers.ping_idle;
        rig.host.wire.packets();
        rig.tick(before);
        let early_pong = pong_to_last_ping(&rig, &mut session);

        let plugged = before + Duration::from_millis(10);
        rig.client.address_changed(plugged, &rig.socket);
        answer_stun(&mut rig, &server, MOVED, plugged);
        assert!(rig.client.own_moved.is_some());
        assert_eq!(rig.client.view(plugged, 0).reply, None, "still live");

        let mut now = plugged;
        for seq in 1..5 {
            now += timers.ping_idle;
            rig.deliver(&ping(&mut session, seq), now);
            rig.tick(now);
        }
        rig.deliver(&early_pong, now);
        assert!(
            rig.client.own_moved.is_some(),
            "nothing yet shows the host hears the new address"
        );

        // The host lets this PC go, and STUN now sees what it saw last time.
        let quiet = now + timers.reconnecting_after;
        rig.tick(quiet);
        answer_stun(&mut rig, &server, MOVED, quiet);
        let view = rig.client.view(quiet, 0);
        assert_eq!(view.strip.state, LinkState::Reconnecting);
        assert!(
            view.reply
                .is_some_and(|reply| matches!(reply.state, ReplyState::Code { .. })),
            "the code shows"
        );
    }

    #[test]
    fn pong_after_move_proves_it() {
        let timers = Timers::default();
        let start = Instant::now();
        let (mut rig, mut session) = Rig::connected(timers, start);
        let server = Wire::new();
        stun_on(&mut rig, &server, OUTSIDE, start);

        let plugged = start + Duration::from_millis(10);
        rig.client.address_changed(plugged, &rig.socket);
        answer_stun(&mut rig, &server, MOVED, plugged);
        assert!(rig.client.own_moved.is_some());

        let pinged = plugged + timers.ping_idle;
        rig.host.wire.packets();
        rig.tick(pinged);
        let pong = pong_to_last_ping(&rig, &mut session);
        rig.deliver(&pong, pinged);
        assert_eq!(rig.client.own_moved, None);

        let quiet = pinged + timers.reconnecting_after;
        rig.tick(quiet);
        answer_stun(&mut rig, &server, MOVED, quiet);
        let view = rig.client.view(quiet, 0);
        assert_eq!(view.strip.state, LinkState::Reconnecting);
        assert_eq!(view.reply, None);
    }

    // Over the LAN or a tunnel, this PC's outside address is not where the
    // host sends, so a code with it would only punch somewhere useless.
    #[test]
    fn no_code_in_room_over_lan_or_tunnel() {
        for (host, code) in [
            ("192.168.1.20:41000", false),
            ("100.101.102.103:41000", false),
            ("[fd7a:115c:a1e0::1]:41000", false),
            ("203.0.113.20:41000", true),
        ] {
            let start = Instant::now();
            let (mut rig, _) = Rig::connected(Timers::default(), start);
            let server = Wire::new();
            stun_on(&mut rig, &server, OUTSIDE, start);
            // Set by hand: nothing is sent to it here.
            rig.client.host_addr = Some(host.parse().unwrap());
            rig.client.state = LinkState::Reconnecting;
            let quiet = start + rig.client.timers.reconnecting_after;
            rig.client.own_address_moved(
                Moved {
                    from: SocketAddr::V4(OUTSIDE),
                    to: SocketAddr::V4(MOVED),
                    from_confirmed: Some(start),
                },
                quiet,
            );
            let shown = rig.client.view(quiet, 0).reply.is_some();
            assert_eq!(shown, code, "host at {host}");
        }
    }

    fn addr(text: &str) -> SocketAddr {
        text.parse().unwrap()
    }

    fn lan(text: &str) -> Candidate {
        Candidate {
            kind: CandidateKind::Lan,
            addr: addr(text),
        }
    }

    // A known host as a rejoin reads it from hosts.bin.
    fn known_host(host: &FakeHost, reached: Option<SocketAddr>) -> KnownHost {
        KnownHost {
            host_key: *host.identity.public(),
            room_name: String::from("Mara's room"),
            host_name: String::from("Mara"),
            secret: Zeroizing::new(host.peer_secret),
            candidates: vec![lan("192.0.2.10:41000")],
            address_name: Some(String::from("myroom.duckdns.org")),
            last_reached: reached,
            manual: None,
            last_seen: 1_790_000_000,
        }
    }

    fn rejoining(known: &KnownHost, start: Instant) -> (Client, Socket) {
        let socket = Socket::bind(0, Log::off()).expect("bind the client socket");
        let client = Client::new(ClientSetup {
            identity: Arc::new(Identity::generate()),
            name: "Ana".to_owned(),
            ticket: Ticket::Known(known),
            timers: Timers::default(),
            port: socket.local_port(),
            has_ipv6: socket.has_ipv6(),
            lookup: Lookup::default(),
            joined: start,
            hosts: HostBook::new(vec![known.clone()], true),
            list_problem: None,
            voice: crate::testing::quiet_voice(false),
            speaker: crate::testing::no_speaker(),
            screen: crate::testing::quiet_screen(),
            log: Log::off(),
        });
        (client, socket)
    }

    fn saved(client: &mut Client) -> Vec<KnownHost> {
        let save = client.take_last_save().expect("the list changed");
        known::parse_hosts(&save.bytes).expect("what was saved reads back")
    }

    #[test]
    fn rejoin_address_order() {
        let host = FakeHost::new();
        let mut known = known_host(&host, Some(addr("198.51.100.4:41000")));
        known.manual = Manual::parse("203.0.113.5:41000").unwrap();
        let (mut client, _socket) = rejoining(&known, Instant::now());
        assert_eq!(
            client.candidates,
            [
                addr("203.0.113.5:41000"),
                addr("198.51.100.4:41000"),
                addr("192.0.2.10:41000"),
            ]
        );
        assert!(client.invite.is_none());
        assert_eq!(client.secret.as_deref(), Some(&host.peer_secret));
        // The room has its name on the screen while the host is looked for.
        assert_eq!(client.view(Instant::now(), 0).room_name, "Mara's room");
        // The stored name waits for the fast round, as an invite's does.
        assert!(client.name_wanted().is_none());

        // A name typed in is looked up at once, in place of the stored one.
        known.manual = Manual::parse("home.example.net").unwrap();
        let (mut client, _socket) = rejoining(&known, Instant::now());
        let asked = client.name_wanted().expect("looked up at once");
        assert_eq!(asked.name, "home.example.net");
        assert_eq!(
            client.candidates,
            [addr("198.51.100.4:41000"), addr("192.0.2.10:41000")]
        );
    }

    #[test]
    fn rejoin_keeps_record_current() {
        let start = Instant::now();
        let host = FakeHost::new();
        let mut known = known_host(&host, Some(host.wire.addr()));
        // Only the host's own address, so nothing leaves loopback.
        known.candidates.clear();
        let (client, socket) = rejoining(&known, start);
        let mut rig = Rig {
            client,
            socket,
            host,
        };
        rig.tick(start);
        let (kind, mut session, response) = rig.host.answer_all(start).pop().expect("a try");
        assert_eq!(kind, InitKind::Known);
        rig.deliver(&response, start);
        assert!(rig.host.open_all(&mut session) > 0);
        let hello = rig.host.hello(&mut session, start);
        rig.deliver(&hello, start);
        let secret = rig.host.send_secret(&mut session, start);
        rig.deliver(&secret, start);
        rig.deliver(&ping(&mut session, 0), start);
        assert_eq!(rig.client.state, LinkState::Live);
        saved(&mut rig.client);

        let told = Message::HostAddresses {
            candidates: vec![
                lan("192.0.2.10:41000"),
                Candidate {
                    kind: CandidateKind::Public,
                    addr: addr("203.0.113.9:41000"),
                },
            ],
            address_name: None,
        };
        let packet = rig.host.say(&mut session, &told, start);
        rig.deliver(&packet, start);
        let roster = Message::Roster(Roster {
            room: String::from("Tuesday night"),
            entries: vec![control::Entry {
                key: *rig.host.identity.public(),
                slot: 0,
                name: String::from("Mara"),
                rtt_ms: None,
                is_host: true,
                joined_by_invite: false,
                reconnecting: false,
                share: None,
                controlling: false,
            }],
        });
        let packet = rig.host.say(&mut session, &roster, start);
        rig.deliver(&packet, start);
        let hosts = saved(&mut rig.client);
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].room_name, "Tuesday night");
        assert_eq!(hosts[0].host_name, "Mara");
        assert_eq!(
            hosts[0].candidates,
            [
                lan("192.0.2.10:41000"),
                Candidate {
                    kind: CandidateKind::Public,
                    addr: addr("203.0.113.9:41000"),
                },
            ]
        );
        assert_eq!(hosts[0].address_name, None);
        assert_eq!(hosts[0].last_reached, Some(rig.host.wire.addr()));

        // The same roster again changes nothing worth a write.
        let packet = rig.host.say(&mut session, &roster, start);
        rig.deliver(&packet, start);
        assert!(rig.client.take_last_save().is_none());
    }

    // A host can say it moved in every packet. The list keeps the newest,
    // and the log gets a few lines, not one per packet.
    #[test]
    fn moving_host_costs_few_log_lines() {
        let start = Instant::now();
        let (mut rig, mut session) = Rig::connected(Timers::default(), start);
        let (log, captured) = Log::capture(256);
        rig.client.log = log;
        for n in 0..50u16 {
            let told = Message::HostAddresses {
                candidates: vec![lan(&format!("192.0.2.10:{}", 41000 + n))],
                address_name: None,
            };
            let packet = rig.host.say(&mut session, &told, start);
            rig.deliver(&packet, start);
        }
        let lines = captured
            .lines()
            .iter()
            .filter(|line| line.starts_with("the host's addresses, kept for rejoining"))
            .count();
        assert!((1..=20).contains(&lines), "{lines} lines");
        let hosts = saved(&mut rig.client);
        assert_eq!(hosts[0].candidates, [lan("192.0.2.10:41049")]);
    }

    #[test]
    fn a_first_join_puts_the_host_on_the_list() {
        let start = Instant::now();
        let (mut rig, _) = Rig::connected(Timers::default(), start);
        let hosts = saved(&mut rig.client);
        assert_eq!(hosts.len(), 1);
        let kept = &hosts[0];
        assert_eq!(kept.host_key, *rig.host.identity.public());
        assert_eq!(*kept.secret, rig.host.peer_secret);
        // The invite's one address is on loopback, which no list keeps, and
        // is where the host was reached.
        assert!(kept.candidates.is_empty());
        assert_eq!(kept.last_reached, Some(rig.host.wire.addr()));
        assert_eq!(kept.room_name, control::ROOM_FALLBACK);
    }

    #[test]
    fn the_code_on_a_rejoin_names_this_pc() {
        let host = FakeHost::new();
        let never = Wire::new();
        let mut known = known_host(&host, Some(never.addr()));
        known.candidates.clear();
        known.address_name = None;
        let start = Instant::now();
        let (mut client, socket) = rejoining(&known, start);
        client
            .stun
            .set_settled(net::stun::Mapping::Easy, Some(OUTSIDE));
        client.on_timer(start + DUE, &socket);
        let view = client.view(start + DUE, 0);
        assert_eq!(view.notice, Some(Notice::StillTrying));
        let reply = view.reply.expect("a reply code");
        let code = ReplyCode::decode(&reply.code).expect("the code decodes");
        assert_eq!(code.answers, Answers::Rejoin);
        assert_eq!(code.client_key, *client.identity.public());
    }

    // Someone on the path who can see and spoof packets sends a copy of the
    // host's packet from elsewhere: it moves the pings and control, never the
    // voice. A real move takes the voice once a ping there is answered from
    // there.
    #[test]
    fn voice_moves_on_answered_ping() {
        let (mut rig, mut session) = Rig::connected(Timers::default(), Instant::now());
        let home = rig.host.wire.addr();
        let media = |rig: &Rig| rig.client.media.map(|media| media.to());
        assert_eq!(media(&rig), Some(home));

        let copier = Wire::new();
        let fresh = ping(&mut session, 1);
        let now = Instant::now();
        rig.client
            .on_packet(&fresh, copier.addr(), now, &rig.socket);
        assert_eq!(rig.client.host_addr, Some(copier.addr()));
        assert_eq!(media(&rig), Some(home));
        assert_eq!(
            rig.client.link.next_ping, now,
            "the new address is pinged at once"
        );
        rig.deliver(&ping(&mut session, 2), Instant::now());
        assert_eq!(
            (rig.client.host_addr, media(&rig)),
            (Some(home), Some(home))
        );

        let moved = Wire::new();
        let from_there = ping(&mut session, 3);
        rig.client
            .on_packet(&from_there, moved.addr(), Instant::now(), &rig.socket);
        rig.tick(Instant::now());
        assert_eq!(media(&rig), Some(home));
        let mut plain = Vec::new();
        let (seq, t1) = moved
            .packets()
            .iter()
            .find_map(|packet| {
                session.decrypt(packet, &mut plain).ok()?;
                match peer::read_plain(&plain) {
                    Some(Plain::Ping(PingMessage::Ping { seq, t1 })) => Some((seq, t1)),
                    _ => None,
                }
            })
            .expect("a ping to the new address");
        let mut pong = Vec::new();
        PingMessage::Pong {
            seq,
            t1,
            t2: t1,
            t3: t1,
        }
        .encode(&mut pong);
        let pong = seal(&mut session, Channel::Ping, &pong);
        rig.client
            .on_packet(&pong, moved.addr(), Instant::now(), &rig.socket);
        assert_eq!(media(&rig), Some(moved.addr()));
    }

    // Stepped down at 60, the person asks for 90. The share's thread still
    // runs at 60, which the host has, so it hears nothing, whatever the roster
    // copy here says; the ask itself is no rate the share runs at. The host
    // hears each rate the thread runs at once, the one it was granted with
    // never.
    #[test]
    fn host_hears_each_running_rate_once() {
        use crate::control::ShareAnswer;
        use crate::screen::{OwnShare, ShareNews};

        let start = Instant::now();
        let (mut rig, mut session) = Rig::connected(Timers::default(), start);
        let (log, captured) = Log::capture(64);
        rig.client.log = log;
        assert!(rig.client.share(120, start, &rig.socket));
        let granted = Message::ShareAnswer(ShareAnswer::Granted { share: 3 });
        let packet = rig.host.say(&mut session, &granted, start);
        rig.deliver(&packet, start);
        assert_eq!(
            rig.client.screen.sharing.state(),
            OwnShare::Sharing {
                number: 3,
                fps: 120
            }
        );
        let runs_at = |rig: &mut Rig, fps: u8| {
            rig.client
                .screen
                .sharing
                .tell(ShareNews::Fps { share: 3, fps });
            rig.client.screen_work(start, &rig.socket);
        };
        runs_at(&mut rig, 120);
        runs_at(&mut rig, 60);
        assert!(rig.client.share(90, start, &rig.socket));
        runs_at(&mut rig, 60);
        runs_at(&mut rig, 90);
        let told: Vec<String> = captured
            .lines()
            .into_iter()
            .filter(|line| line.contains("runs at"))
            .collect();
        assert_eq!(told, ["share 3: runs at 60 fps", "share 3: runs at 90 fps"]);
    }

    // While the roster shows someone else sharing, Share is refused here at
    // once with the line the host's refusal brings, and no ask goes to the
    // host. Once the roster shows nobody sharing, the ask goes.
    #[test]
    fn share_refused_while_other_shares() {
        use crate::control::EntryShare;
        use crate::screen::{OwnShare, Refusal};
        use crate::view::LineKind;

        let start = Instant::now();
        let (mut rig, mut session) = Rig::connected(Timers::default(), start);
        let (mara, ana, bo) = (
            *rig.host.identity.public(),
            *rig.client.identity.public(),
            *Identity::generate().public(),
        );
        let roster = |bo_shares: Option<EntryShare>| {
            let entry = |key: [u8; 32], slot: u8, name: &str, share| control::Entry {
                key,
                slot,
                name: String::from(name),
                rtt_ms: None,
                is_host: slot == 0,
                joined_by_invite: slot != 0,
                reconnecting: false,
                share,
                controlling: false,
            };
            Message::Roster(Roster {
                room: String::from("Tuesday night"),
                entries: vec![
                    entry(mara, 0, "Mara", None),
                    entry(ana, 1, "Ana", None),
                    entry(bo, 2, "Bo", bo_shares),
                ],
            })
        };
        let asks = |rig: &Rig| {
            rig.client
                .link
                .reliable
                .unacked()
                .filter(|message| {
                    matches!(Message::decode(message), Some(Message::ShareStart { .. }))
                })
                .count()
        };
        let sharing = roster(Some(EntryShare { number: 4, fps: 60 }));
        let packet = rig.host.say(&mut session, &sharing, start);
        rig.deliver(&packet, start);

        let busy = OwnShare::Refused(Refusal::Busy {
            name: String::from("Bo"),
        });
        // The second press finds this PC refused already, and is refused the
        // same way.
        for press in 1..=2 {
            assert!(rig.client.share(120, start, &rig.socket));
            let view = rig.client.view(start, 0);
            assert_eq!(view.share.own, busy);
            let lines: Vec<(&[u8; 32], &str, &str)> = view
                .chat
                .iter()
                .filter(|line| line.kind == LineKind::System)
                .map(|line| (&line.author, line.name.as_str(), line.text.as_str()))
                .collect();
            assert_eq!(
                lines,
                vec![(&bo, "Bo", "Bo is sharing. One share at a time."); press]
            );
            assert_eq!(asks(&rig), 0, "the host heard an ask");
        }

        let nobody = roster(None);
        let packet = rig.host.say(&mut session, &nobody, start);
        rig.deliver(&packet, start);
        assert!(rig.client.share(120, start, &rig.socket));
        assert_eq!(
            rig.client.screen.sharing.state(),
            OwnShare::Asking { fps: 120 }
        );
        assert_eq!(asks(&rig), 1);
    }
}
