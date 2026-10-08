// The host: answers handshakes, keeps one link per client, tells every
// client who is in the room, and punches toward friends whose reply code was
// pasted.

use std::collections::HashMap;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use channels::{Channel, PingMessage};
use crossbeam_channel::{Sender, TrySendError};
use invite::{Answers, Candidate, Mapping, ReplyCode, Version};
use keys::Identity;
use net::addrs::{Gateway, LocalAddr, NoRouter};
use session::{InitKind, PacketType, Session, SessionError, Tai64N};
use stats::{LinkSnapshot, Thresholds};
use zeroize::Zeroizing;

use crate::Soonest;
use crate::chat::{self, ChatMessage, ChatRefused, Delivery, History};
use crate::config::{Lookup, Timers};
use crate::control::{
    self, Entry, EntryShare, LossPermille, MAX_CLIENTS, Message, Periods, Roster,
};
use crate::cookies::Gate;
use crate::invites::{Invites, Local, Recipe};
use crate::known::{self, DeviceBook, Joined, ListProblem, Save};
use crate::limit::{Bucket, RateLimit, Refused};
use crate::log::{
    self, Log, PerSource, counted, init_word, kind_word, list, log, mapping_word, path_text, secs,
    yes_no,
};
use crate::mapper::Report;
use crate::names::{AddressName, Outcome, Request};
use crate::numbers::{self, LinkNumbers};
use crate::peer::{self, Clock, Drops, Link, MediaPath, Opened, Plain, Sessions, Traffic, Which};
use crate::reply::{self, Punches, ReplyAccepted, ReplyRefused};
use crate::router::{self, PortMap};
use crate::screen::{Screen, ScreenSetup, Sharing, Watching};
use crate::socket::Socket;
use crate::stun::{Answer, Moved, Stun};
use crate::talk::{self, Outlet, Sent, Talk, ToSpeaker};
use crate::view::{
    AddressChange, AddressChanged, ChatLine, InviteView, LineKind, LinkState, MappingWord, Notice,
    Numbers, PasteState, PathWord, Person, Role, RouterState, Strip, View, VoiceLoss,
};

mod remote;
mod sharing;

use remote::PeerControl;
use sharing::{Kept, Live, PeerShare, Watcher};

// The client keeps its tries for 3 s. A little more covers the way back.
const PENDING_LIFETIME: Duration = Duration::from_secs(5);
const PENDING_PER_KEY: usize = 16;
const PENDING_TOTAL: usize = 64;
// Keys that passed a handshake this run. Only a leaked multi-use invite can
// grow this quickly, and that is capped per invite; when it is full anyway,
// the key seen longest ago that is not in the room makes way.
const MAX_KEYS: usize = 1024;
const ROSTER_EVERY: Duration = Duration::from_secs(1);
// A join, leave or rename sends a roster to everyone at once, but not more
// often than this, so a client repeating Hello cannot turn each small packet
// of its own into a full roster to every other client.
const ROSTER_GAP: Duration = Duration::from_millis(250);
// The ladder sends one initiation to every address in the invite. When two of
// them reach this PC the same packet arrives twice, one path's delay apart,
// and the second copy is not a replay worth showing.
const COPY_WINDOW: Duration = Duration::from_secs(1);
// Stamps are not kept between runs. A known device's initiation stamped
// more than this before the room opened is an old one sent again and is not
// answered. A day covers a client clock set some hours wrong; one saved
// within the day is still answered once.
const STAMP_FLOOR: Duration = Duration::from_secs(24 * 60 * 60);
// How long the sessions of someone who said Bye are still recognised. The
// second copy of the Bye leaves right behind the first; this also covers a
// ping or two that was already on a slow path.
const LEFT_GRACE: Duration = Duration::from_secs(5);
const JUST_LEFT_KEPT: usize = 64;
// After this PC's public address changed, the paste field says for this
// long that a code from a friend will not help, while friends are still
// reconnecting or lost...
const CODES_USELESS_FOR: Duration = Duration::from_secs(120);
// ...and the panel says friends could not follow for this long at most.
// Friends let go without a Bye are remembered for as long, for the reconnect
// time when they come back.
const CHANGE_SHOWN_FOR: Duration = Duration::from_secs(300);
const LOST_KEPT: usize = 64;
// Friends whose silence began within this of each other went quiet together,
// which looks like this PC's own network and not theirs.
const TOGETHER: Duration = Duration::from_secs(1);
// A friend reports what it lost once a second. Each report can queue a
// WorstLoss for every talker, so past this rate they are left out.
const VOICE_REPORTS_PER_SECOND: f64 = 4.0;
const VOICE_REPORT_BURST: f64 = 4.0;
// A friend sends 200 voice frames a second at most, 5 ms each. Past this
// rate the host drops what comes, since each frame goes on to everyone; the
// burst covers a friend's capture thread catching up after a stall.
const VOICE_PER_SECOND: f64 = 250.0;
const VOICE_BURST: f64 = 100.0;
// What limit.rs gives any one address, for initiations from the address and
// port a friend in the room is at.
const REKEYS_PER_SECOND: f64 = 10.0;
const REKEY_BURST: f64 = 20.0;

struct Peer {
    key: [u8; 32],
    name: String,
    // Where control and pings go: where this friend was last heard from.
    // Voice goes where `media` says.
    addr: SocketAddr,
    media: MediaPath,
    path: PathWord,
    sessions: Sessions,
    link: Link,
    traffic: Traffic,
    last_heard: Instant,
    reconnecting: bool,
    joined_by_invite: bool,
    rekeys: u32,
    // The Hello on this link has come, so what this friend says has a name
    // to go with. Until then their chat waits in its stream.
    hello: bool,
    // How much more this friend may say (chat::SAYS_PER_SECOND).
    says: Bucket,
    // What the host's relayed voice calls this friend, 1 to 7; the host is 0.
    slot: u8,
    // How many more voice frames this friend may send (VOICE_PER_SECOND).
    voice: Bucket,
    // How many more initiations may come from `addr` (REKEYS_PER_SECOND).
    handshakes: Bucket,
    // What the capture thread sent on this link.
    voice_out: Arc<Sent>,
    // What this friend said of their audio, and what they were last told
    // of the host's.
    periods: Option<Periods>,
    told_periods: Option<Periods>,
    // How many more loss reports this friend may send
    // (VOICE_REPORTS_PER_SECOND).
    voice_reports: Bucket,
    // The link's jitter is past 5 ms, so a time through its clock offset is
    // only about right. Worked out once a timer pass, not per voice packet.
    jittery: bool,
    share: PeerShare,
    control: PeerControl,
}

impl Peer {
    fn new(
        key: [u8; 32],
        addr: SocketAddr,
        session: Session,
        invited: bool,
        slot: u8,
        now: Instant,
    ) -> Peer {
        Peer {
            key,
            name: control::PERSON_FALLBACK.to_owned(),
            addr,
            media: MediaPath::new(addr),
            path: numbers::path_word(net::addrs::path_of(addr)),
            sessions: Sessions {
                current: Some(session),
                previous: None,
            },
            link: Link::new(now),
            traffic: Traffic::default(),
            last_heard: now,
            reconnecting: false,
            joined_by_invite: invited,
            rekeys: 0,
            hello: false,
            says: Bucket::full(now, chat::SAY_BURST),
            slot,
            voice: Bucket::full(now, VOICE_BURST),
            handshakes: Bucket::full(now, REKEY_BURST),
            voice_out: Arc::default(),
            periods: None,
            told_periods: None,
            voice_reports: Bucket::full(now, VOICE_REPORT_BURST),
            jittery: false,
            share: PeerShare::new(now),
            control: PeerControl::new(now),
        }
    }

    fn move_to(&mut self, addr: SocketAddr, now: Instant, clock: Clock) {
        self.addr = addr;
        self.path = numbers::path_word(net::addrs::path_of(addr));
        self.link.path_changed(now);
        // A ping goes there at once, so voice follows a real move within a
        // round trip. The time is taken now, under the state lock, so no
        // ping built before the move can count.
        if self.media.moved(addr, clock.micros(Instant::now())) {
            self.link.next_ping = now;
        }
    }

    // Returns how long the silence was, when it had gone on long enough to
    // count as reconnecting.
    fn heard(
        &mut self,
        from: SocketAddr,
        roamed: bool,
        now: Instant,
        quiet_after: Duration,
        clock: Clock,
    ) -> Option<Duration> {
        let silence = now.saturating_duration_since(self.last_heard);
        let quiet = self.reconnecting || silence >= quiet_after;
        if roamed {
            self.move_to(from, now, clock);
        } else if quiet {
            self.link.path_recovered(now);
        }
        let back = self.reconnecting.then_some(silence);
        self.last_heard = now;
        self.reconnecting = false;
        back
    }

    fn restart(&mut self, now: Instant) {
        self.traffic.earlier_retransmits += self.link.retransmissions();
        self.link = Link::new(now);
        self.hello = false;
        self.told_periods = None;
        self.share.restart();
    }

    fn flush(&mut self, socket: &Socket, now: Instant) {
        if let Some(session) = self.sessions.current.as_mut() {
            self.link
                .flush(socket, session, &[self.addr], now, &mut self.traffic);
        }
    }

    fn send(&mut self, socket: &Socket, channel: Channel, payload: &[u8]) {
        if let Some(session) = self.sessions.current.as_mut() {
            peer::send_on(
                socket,
                session,
                channel,
                payload,
                &[self.addr],
                &mut self.traffic,
            );
        }
    }

    // While media checks the address control goes to, the ping is a probe,
    // the one kind whose answer moves media there.
    fn send_ping(&mut self, socket: &Socket, clock: Clock) {
        let ping = if self.media.checks(&[self.addr]) {
            let (ping, seq) = self.link.probe(clock);
            self.media.probed(seq);
            ping
        } else {
            self.link.ping(clock)
        };
        self.send(socket, Channel::Ping, &ping);
    }

    fn has_live_session(&self, now: Instant) -> bool {
        self.sessions
            .current
            .as_ref()
            .is_some_and(|session| !session.is_expired(now))
    }
}

// Accepted and answered, waiting for the first data packet that proves the
// client finished the handshake.
struct Pending {
    session: Session,
    key: [u8; 32],
    kind: InitKind,
    created: Instant,
    // Where the response went: the one address the handshake made a round
    // trip to.
    answered_at: SocketAddr,
}

struct KeyRecord {
    stamp: Option<Tai64N>,
    // The initiation that set stamp and when, to tell a second copy of it
    // from a replay.
    accepted: Option<(Vec<u8>, Instant)>,
    secret: Option<Zeroizing<[u8; 32]>>,
    invited: bool,
    last_seen: Instant,
}

// A friend let go after lost_after with no Bye.
struct Lost {
    key: [u8; 32],
    last_heard: Instant,
    at: Instant,
}

// A listener's word on how much of a talker it lost over the last 2 s.
struct LossReport {
    talker: [u8; 32],
    from: [u8; 32],
    loss: VoiceLoss,
    at: Instant,
}

// This PC's public address changed, as STUN saw it.
struct Change {
    at: Instant,
    // Friends who went with it: everyone in the room at the time, and
    // anyone let go since STUN last saw the old address, which is when the
    // change can have begun.
    watched: Vec<[u8; 32]>,
    // Past CODES_USELESS_FOR, written down so the view changes once.
    codes_over: bool,
}

impl KeyRecord {
    fn new(now: Instant) -> KeyRecord {
        KeyRecord {
            stamp: None,
            accepted: None,
            secret: None,
            invited: false,
            last_seen: now,
        }
    }
}

// This PC's addresses read again after Windows said one changed, and the
// router the default route leads to now.
pub(crate) struct Listing {
    pub addrs: Vec<LocalAddr>,
    pub router: Result<Gateway, NoRouter>,
}

pub(crate) struct HostSetup {
    pub identity: Arc<Identity>,
    pub name: String,
    pub room_name: String,
    pub timers: Timers,
    pub local: Local,
    pub port: u16,
    pub has_ipv6: bool,
    // The router a mapper thread asks for the port, which reports here. It
    // is the one toward the internet, so its side of this PC is the address
    // the carrier-grade NAT check looks at.
    pub router: Option<Gateway>,
    pub punch_loopback: bool,
    // Already checked with invite::check_hostname.
    pub address_name: Option<String>,
    pub lookup: Lookup,
    // Read from devices.bin when the room opened.
    pub devices: DeviceBook,
    pub list_problem: Option<ListProblem>,
    // Counts every initiation the host did key math for. Only tests read it.
    pub initiations_read: Arc<AtomicU64>,
    // Voice: what the audio threads share with the room, and the way to the
    // render thread's mixer.
    pub voice: Arc<talk::Shared>,
    pub speaker: Sender<ToSpeaker>,
    pub screen: ScreenSetup,
    pub now: Instant,
    pub log: Log,
}

pub(crate) struct Host {
    identity: Arc<Identity>,
    name: String,
    room_name: String,
    timers: Timers,
    session_timers: session::Timers,
    thresholds: Thresholds,
    clock: Clock,
    port: u16,
    has_ipv6: bool,
    local: Local,
    stun: Stun,
    portmap: PortMap,
    // Asks the mapper thread to renew now.
    renew_mapping: Option<Sender<()>>,
    // Asks it to delete the mapping and make it again, at the router it
    // names.
    remap_mapping: Option<Sender<Gateway>>,
    // The router the mapper asks, and this PC's address toward it. A router
    // that restarts can hand this PC a new address, so it is picked again
    // when the address list changes.
    router: Option<Gateway>,
    change: Option<Change>,
    // For the stats panel, kept after the change stops mattering.
    last_change: Option<AddressChange>,
    lost: Vec<Lost>,
    reconnect_ms: Option<f32>,
    opened: Instant,
    // When the first invite stops waiting for STUN and the port mapping.
    first_invite_by: Instant,
    invites: Invites,
    limit: RateLimit,
    cookies: Gate,
    initiations_read: Arc<AtomicU64>,
    keys: HashMap<[u8; 32], KeyRecord>,
    peers: Vec<Peer>,
    pending: Vec<Pending>,
    next_roster: Instant,
    // A changed roster waiting out ROSTER_GAP.
    roster_due: Option<Instant>,
    roster_gap_until: Instant,
    drops: Drops,
    scratch: Vec<u8>,
    closed: bool,
    socket_failed: bool,
    log: Log,
    // Lines about packets that belong to no live session, kept to a few a
    // minute per source. Booth packets and everything else are counted
    // apart, so a friend's join that keeps failing cannot use up the lines
    // their port test from outside needs.
    booth_lines: PerSource,
    other_lines: PerSource,
    // Session indexes of people who just said Bye, and until when. The Bye
    // goes out twice and its second copy lands after they are gone.
    just_left: Vec<(u32, Instant)>,
    // Friends this room turned away for their version, so the chat says it
    // once each, however often they try again.
    other_versions: Vec<[u8; 32]>,
    // Reply codes pasted back: where the punches go and for which key.
    punches: Punches,
    punch_loopback: bool,
    // What came of the last paste, and the key it named.
    last_paste: Option<([u8; 32], PasteState)>,
    // The dynamic DNS name every invite carries, looked up once when the
    // room opens to show whether it points here.
    address_name: Option<AddressName>,
    // The last line written about that, so each change is written once.
    name_said: Option<String>,
    // Every device that joined, with its secret, and the keys refused.
    devices: DeviceBook,
    list_problem: Option<ListProblem>,
    // The addresses every friend in the room was last told, to tell them
    // again when they change.
    told: Option<(Vec<Candidate>, Option<String>)>,
    // The chat in the order this host took it, which is the room's order.
    history: History,
    // How long what clients say takes to get here.
    delivery: Delivery,
    talk: Talk,
    // What each listener last said it lost of each talker.
    reports: Vec<LossReport>,
    // Buffers for voice handed on, kept so a relayed frame allocates nothing.
    voice_payload: Vec<u8>,
    voice_plain: Vec<u8>,
    voice_sealed: Vec<u8>,
    // The slot given last, so a slot someone just left is the last one
    // given again and a client whose roster is a moment old does not play
    // the newcomer under the old name.
    last_slot: u8,
    screen: Screen,
    // The one share in the room, whoever's it is, and the number the next
    // one gets.
    live: Option<Live>,
    next_share: u32,
    // This host as a watcher of a friend's share.
    own_watcher: Watcher,
    // The friend whose share ended last, until when what they sent before
    // it ended is let go quietly.
    share_ended: Option<([u8; 32], Instant)>,
    // The sharing limits of friends who left lately.
    left_limits: Vec<Kept>,
    // What a video packet is sealed into for each watcher, kept so passing
    // one on allocates nothing.
    video_sealed: Vec<u8>,
    // This host's upload setting, which the bitrate rule divides.
    upload_kbps: u32,
    // What a friend's share passed on over internet paths takes of that
    // upload. The cap in the facts is only advice to the sharer; this holds
    // the host to RELAY_SHARE times the setting whatever the sharer sends.
    relay_upload: Bucket,
    // The number the next control session gets, and the controller whose
    // session ended last, until when what they sent before the end is let
    // go quietly.
    next_control: u32,
    control_ended: Option<([u8; 32], Instant)>,
    // An input packet's events on their way to this host's injector, and
    // what one is sealed into for a friend who shares: kept, so passing
    // input on allocates nothing.
    input_events: Vec<crate::remote::InputEvent>,
    input_sealed: Vec<u8>,
}

impl Host {
    pub(crate) fn new(setup: HostSetup) -> Host {
        let name = control::clean(&setup.name, control::PERSON_FALLBACK);
        let carrier_nat = setup
            .router
            .and_then(|gateway| router::carrier_nat(gateway.local));
        // A known device rejoins with the secret it was given, whenever this
        // room opened. Sized once, as the known lists are: a map that grows
        // frees its old table, secrets and all, without wiping it.
        let mut keys = HashMap::with_capacity(MAX_KEYS);
        for device in setup.devices.devices() {
            let mut record = KeyRecord::new(setup.now);
            record.secret = Some(device.secret.clone());
            record.stamp = stamp_floor();
            keys.insert(device.key, record);
        }
        let cookies = Gate::new(
            setup.identity.public(),
            setup.timers.load_initiations,
            setup.timers.load_calm,
            setup.now,
        );
        let clock = Clock::new(setup.now);
        let talk = Talk::new(
            setup.voice,
            setup.speaker,
            clock,
            setup.timers,
            setup.log.clone(),
            setup.now,
        );
        Host {
            portmap: PortMap::new(setup.router.is_some(), carrier_nat, setup.log.clone()),
            renew_mapping: None,
            remap_mapping: None,
            router: setup.router,
            change: None,
            last_change: None,
            lost: Vec::new(),
            reconnect_ms: None,
            opened: setup.now,
            first_invite_by: setup.now + setup.timers.first_invite_wait,
            room_name: control::clean(&setup.room_name, &name),
            name,
            session_timers: setup.timers.session(),
            stun: Stun::new(
                Some(setup.timers.stun_every),
                setup.timers.stun_wait,
                setup.timers.stun_retry,
                setup.now,
                setup.log.clone(),
            ),
            identity: setup.identity,
            timers: setup.timers,
            thresholds: Thresholds::default(),
            clock,
            port: setup.port,
            has_ipv6: setup.has_ipv6,
            local: setup.local,
            invites: Invites::new(),
            limit: RateLimit::new(
                setup.now,
                setup.timers.cookie_replies_per_second,
                setup.timers.cookie_reply_burst,
            ),
            cookies,
            initiations_read: setup.initiations_read,
            keys,
            peers: Vec::new(),
            pending: Vec::new(),
            next_roster: setup.now,
            roster_due: None,
            roster_gap_until: setup.now,
            drops: Drops::default(),
            scratch: Vec::new(),
            closed: false,
            socket_failed: false,
            log: setup.log,
            booth_lines: PerSource::new(setup.now),
            other_lines: PerSource::new(setup.now),
            just_left: Vec::new(),
            other_versions: Vec::new(),
            punches: Punches::default(),
            punch_loopback: setup.punch_loopback,
            last_paste: None,
            address_name: setup.address_name.map(|name| {
                let mut name = AddressName::new(name, setup.lookup);
                name.ask();
                name
            }),
            name_said: None,
            devices: setup.devices,
            list_problem: setup.list_problem,
            told: None,
            history: History::default(),
            delivery: Delivery::default(),
            talk,
            reports: Vec::new(),
            voice_payload: Vec::with_capacity(talk::MAX_VOICE),
            voice_plain: Vec::with_capacity(talk::MAX_VOICE + 1),
            voice_sealed: Vec::with_capacity(talk::MAX_VOICE + 1 + session::DATA_OVERHEAD),
            last_slot: 0,
            upload_kbps: setup.screen.upload_kbps,
            relay_upload: Bucket::full(
                setup.now,
                sharing::relay_per_second(setup.screen.upload_kbps),
            ),
            screen: Screen::new(clock, true, setup.screen, setup.now),
            live: None,
            next_share: 1,
            own_watcher: Watcher::default(),
            share_ended: None,
            left_limits: Vec::new(),
            video_sealed: Vec::with_capacity(crate::screen::LAN_DATAGRAM),
            next_control: 1,
            control_ended: None,
            input_events: Vec::with_capacity(crate::remote::MAX_EVENTS),
            input_sealed: Vec::with_capacity(crate::screen::LAN_DATAGRAM),
        }
    }

    pub(crate) fn take_save(&mut self, now: Instant) -> Option<Save> {
        self.devices.take_save(now)
    }

    pub(crate) fn take_last_save(&mut self) -> Option<Save> {
        self.devices.take_last_save()
    }

    pub(crate) fn save_due(&self) -> Option<Instant> {
        self.devices.save_due()
    }

    pub(crate) fn stun_found(&mut self, servers: Vec<SocketAddr>, socket: &Socket) {
        if !self.closed {
            self.stun.found(servers, socket);
        }
    }

    // Returns true when that settled STUN, which can make the first invite.
    pub(crate) fn stun_resolved(&mut self, now: Instant) -> bool {
        if !self.closed && self.stun.resolved(now) {
            self.stun_settled(now);
            return true;
        }
        false
    }

    pub(crate) fn renew_mapping_with(&mut self, renew: Sender<()>) {
        self.renew_mapping = Some(renew);
    }

    pub(crate) fn remap_with(&mut self, remap: Sender<Gateway>) {
        self.remap_mapping = Some(remap);
    }

    pub(crate) fn lists_addresses(&self) -> bool {
        matches!(self.local, Local::Discover(_))
    }

    // Windows says an address on this PC changed. `listing` is the list read
    // again, when this host reads its own.
    pub(crate) fn address_changed(
        &mut self,
        now: Instant,
        socket: &Socket,
        listing: Option<io::Result<Listing>>,
    ) -> bool {
        if self.closed {
            return false;
        }
        if let Some(listing) = listing {
            self.relisted(listing, socket);
        }
        if self.stun.check(now, socket) {
            log!(
                self.log,
                "windows reported an address change on this pc, asking stun whether the public address changed too"
            );
        }
        false
    }

    // IPv6 goes out from the stable address, not the temporary one Windows
    // prefers: that one changes daily, which to a strict router looks like
    // the host moving. The stable one is picked again when the PC's own
    // addresses change, and so is the router the mapping is asked of the
    // next time it is made again.
    fn relisted(&mut self, listing: io::Result<Listing>, socket: &Socket) {
        let Listing { addrs, router } = match listing {
            Ok(listing) => listing,
            Err(err) => {
                log!(
                    self.log,
                    "could not list this pc's addresses again after the change: {err}"
                );
                return;
            }
        };
        if matches!(&self.local, Local::Discover(old) if *old == addrs) {
            return;
        }
        if self.log.is_on() {
            let ips: Vec<_> = addrs.iter().map(|addr| addr.ip).collect();
            log!(self.log, "this pc's addresses are now {}", list(&ips));
        }
        self.local = Local::Discover(addrs);
        if let Some(was) = self.router {
            match router {
                Ok(now) if now != was => {
                    log!(
                        self.log,
                        "port mapping: the router to ask is now {}, this pc is {} to it; the next mapping is asked of it",
                        now.ip,
                        now.local
                    );
                    self.router = Some(now);
                    // The mapping forwards to the old address, which the
                    // router can give to another device. No outside change
                    // may follow to have it made again.
                    self.remap();
                }
                Ok(_) => {}
                Err(_) => log!(
                    self.log,
                    "port mapping: no router to ask in the new address list, {} is asked again if the mapping is made again",
                    was.ip
                ),
            }
        }
        let source = self.local.ipv6_source(self.has_ipv6);
        if source == socket.ipv6_source() {
            return;
        }
        socket.set_ipv6_source(source);
        match source {
            Some(ip) => log!(
                self.log,
                "ipv6 replies to global addresses leave from {ip} now"
            ),
            None => log!(
                self.log,
                "ipv6 source not pinned any more, this pc has no stable global ipv6 address"
            ),
        }
    }

    pub(crate) fn new_invite(&mut self, multi_use: bool, now: Instant) -> bool {
        if self.closed {
            return false;
        }
        if self.invite_ready(now) {
            self.make_invite(multi_use, now);
        } else {
            self.invites.want(multi_use);
        }
        true
    }

    // What the mapper thread found. A later answer changes the router
    // sentence at once and the invite only when the user asks for a new one.
    pub(crate) fn port_mapping(&mut self, report: Report, now: Instant) -> bool {
        if self.closed {
            return false;
        }
        self.portmap.report(report, self.stun.public_v4_ip());
        self.portmap.note_verdict();
        self.note_name();
        self.first_invite(now);
        true
    }

    // Once, when the room opens: the timer thread starts the lookup.
    pub(crate) fn name_wanted(&mut self) -> Option<Request> {
        if self.closed {
            return None;
        }
        let request = self.address_name.as_mut()?.request()?;
        log!(
            self.log,
            "address name: looking up {} to see whether it points to this pc",
            request.name
        );
        Some(request)
    }

    pub(crate) fn name_found(&mut self, outcome: Outcome) -> bool {
        if self.closed {
            return false;
        }
        let Some(name) = self.address_name.as_mut() else {
            return false;
        };
        name.found(outcome);
        self.note_name();
        true
    }

    // A reply code the user pasted, already decoded by the panel. The code
    // came from outside, so everything is checked here against this host's
    // own clock and tables, the address rules decode applies included.
    // Accepted, it starts the punch rounds, which the timer thread sends.
    pub(crate) fn accept_reply(
        &mut self,
        code: &ReplyCode,
        now: Instant,
    ) -> Result<ReplyAccepted, ReplyRefused> {
        let key = code.client_key;
        let mut in_code = Vec::with_capacity(2);
        in_code.extend(code.outside_v4.map(SocketAddr::V4));
        in_code.extend(code.outside_v6.map(SocketAddr::V6));
        if self.log.is_on() {
            let answers = match code.answers {
                Answers::Invite(_) => "an invite",
                Answers::Rejoin => "a rejoin",
            };
            let outside = if in_code.is_empty() {
                String::from("no address")
            } else {
                list(&in_code)
            };
            log!(
                self.log,
                "reply code pasted for {}: answers {answers}, outside {outside}, mapping {}, expires {}",
                keys::fingerprint(&key),
                mapping_word(code.mapping),
                log::utc(code.expires_at)
            );
        }
        let to = match self.check_reply(code, in_code, now) {
            Ok(to) => to,
            Err(why) => {
                log!(self.log, "reply code refused: {why}");
                if !self.closed {
                    self.last_paste = Some((key, PasteState::Refused(why.clone())));
                }
                return Err(why);
            }
        };
        log!(
            self.log,
            "reply code checks passed: not expired, {} open for this key, no live session for it or one silent for {} and more, no other paste for it in the last {} s, {}, mapping not hard",
            match code.answers {
                Answers::Invite(_) => "its invite",
                Answers::Rejoin => "a rejoin",
            },
            secs(self.timers.reconnecting_after),
            crate::reply::PASTE_GAP.as_secs(),
            counted(to.len() as u64, "address", "addresses")
        );
        let left = Duration::from_secs(code.expires_at.saturating_sub(crate::unix_now()));
        self.punches.start(key, to.clone(), left, now);
        self.last_paste = Some((key, PasteState::Sent));
        Ok(ReplyAccepted { to })
    }

    // Returns the addresses to punch.
    fn check_reply(
        &self,
        code: &ReplyCode,
        in_code: Vec<SocketAddr>,
        now: Instant,
    ) -> Result<Vec<SocketAddr>, ReplyRefused> {
        let key = &code.client_key;
        if self.closed {
            return Err(ReplyRefused::Closed);
        }
        // The panel hides the field then; this is for anything else that
        // calls in.
        if self.stun.mapping_word() == Some(MappingWord::Hard) {
            return Err(ReplyRefused::HostHard);
        }
        // A code that did not come through ReplyCode::decode still gets its
        // rules, so no caller can aim punches into this PC's own networks.
        let mut checked = *code;
        if self.punch_loopback {
            checked.outside_v4 = checked.outside_v4.filter(|addr| !addr.ip().is_loopback());
            checked.outside_v6 = checked.outside_v6.filter(|addr| !addr.ip().is_loopback());
        }
        checked.check().map_err(ReplyRefused::BadCode)?;
        if code.is_expired(crate::unix_now()) {
            return Err(ReplyRefused::Expired);
        }
        // Whatever it answers, even a live invite the blocked person saw:
        // every handshake from that key is dropped, so punches toward it
        // would only open this router to it and say Sent for nothing.
        if self.devices.is_blocked(key) {
            return Err(ReplyRefused::Blocked);
        }
        match code.answers {
            Answers::Invite(id) => {
                if self.invites.secret_for(&id, key, now).is_none() {
                    return Err(ReplyRefused::InviteNotLive);
                }
            }
            // A key with a per-peer secret is a known device, or one that
            // joined this run: in the room now with a silent session
            // (AlreadyHere below lets that by), or with no session at all.
            Answers::Rejoin => {
                let known = self
                    .keys
                    .get(key)
                    .is_some_and(|record| record.secret.is_some());
                if !known {
                    return Err(ReplyRefused::NotKnown);
                }
            }
        }
        // A friend who went quiet may come back through a fresh punch.
        if let Some(peer) = self
            .peers
            .iter()
            .find(|p| p.key == *key && !p.reconnecting && p.has_live_session(now))
        {
            return Err(ReplyRefused::AlreadyHere {
                name: peer.name.clone(),
            });
        }
        if self.punches.too_soon(key, now) {
            return Err(ReplyRefused::TooSoon);
        }
        // Without an IPv6 address of its own this PC cannot send to the
        // friend's, and the panel would say Sent for nothing. The invite
        // leaves IPv6 out by the same rule.
        let sends_ipv6 = self.local.ipv6_source(self.has_ipv6).is_some();
        let mut to = in_code;
        to.retain(|addr| {
            let usable = addr.is_ipv4() || sends_ipv6;
            if !usable {
                log!(
                    self.log,
                    "reply code: {addr} left out, this pc has no ipv6 address to send from"
                );
            }
            usable
        });
        if to.is_empty() {
            return Err(ReplyRefused::NoAddress);
        }
        if code.mapping == Mapping::Hard {
            return Err(ReplyRefused::FriendHard);
        }
        // The punches let only this key in from its address. Whoever else
        // is in the room there, or had it punched open, would be locked out.
        let taken = |addr: &&SocketAddr| {
            self.punches.held_for_other(**addr, key, now).is_some()
                || self
                    .peers
                    .iter()
                    .any(|p| p.key != *key && reply::same_address(p.addr, **addr))
        };
        if let Some(&addr) = to.iter().find(taken) {
            return Err(ReplyRefused::AddressTaken { addr });
        }
        Ok(to)
    }

    pub(crate) fn on_packet(
        &mut self,
        packet: &[u8],
        from: SocketAddr,
        now: Instant,
        socket: &Socket,
    ) -> bool {
        if self.closed {
            return false;
        }
        if net::stun::is_stun(packet) {
            return match self.stun.on_answer(packet, from, now) {
                Answer::NotOurs(why) => {
                    if self.log_other(from, now) {
                        self.log
                            .line(format!("{}: {why}, dropped", heard(packet, from)));
                    }
                    self.drops.bad(now)
                }
                Answer::Recorded => {
                    self.stun_answered();
                    self.tell_addresses(now, socket, None);
                    true
                }
                Answer::Settled => {
                    self.stun_settled(now);
                    self.tell_addresses(now, socket, None);
                    true
                }
                Answer::Moved(moved) => {
                    self.public_moved(moved, now, socket);
                    self.tell_addresses(now, socket, None);
                    true
                }
            };
        }
        let booth = match session::packet_type(packet) {
            Some(PacketType::Initiation) => return self.on_initiation(packet, from, now, socket),
            Some(PacketType::Data) => {
                let mut plain = std::mem::take(&mut self.scratch);
                let changed = self.on_data(packet, from, now, socket, &mut plain);
                self.scratch = plain;
                return changed;
            }
            Some(PacketType::Response) => Some("a handshake response, which only a client takes"),
            Some(PacketType::CookieReply) => Some("a cookie reply, which only a client takes"),
            Some(PacketType::Punch) => Some("a punch, which only a client takes"),
            None => None,
        };
        let allowed = match booth {
            Some(_) => self.log_stray(from, now),
            None => self.log_other(from, now),
        };
        if allowed {
            let what = match (booth, packet.first()) {
                (Some(what), _) => what.to_owned(),
                (None, Some(first)) => format!("not a booth packet (first byte 0x{first:02x})"),
                (None, None) => String::from("an empty datagram"),
            };
            self.log
                .line(format!("{}: {what}, dropped", heard(packet, from)));
        }
        self.drops.bad(now)
    }

    pub(crate) fn on_timer(&mut self, now: Instant, socket: &Socket) -> bool {
        if self.closed {
            return false;
        }
        let mut changed = false;
        if self.stun.tick(now, socket) {
            self.stun_settled(now);
            changed = true;
        }
        changed |= self.first_invite(now);
        self.note_load(now);
        if let Some(count) = self.cookies.capped_over(now) {
            self.say_capped(count);
        }
        self.punches.send_due(now, socket, &self.log);
        for key in self.punches.expire(now, &self.log) {
            if self
                .last_paste
                .as_ref()
                .is_some_and(|(pasted, state)| *pasted == key && *state == PasteState::Sent)
            {
                self.last_paste = None;
                changed = true;
            }
        }
        if self.invites.tick(now) {
            log!(self.log, "the invite on show expired");
            changed = true;
        }
        if self.log.is_on() {
            self.booth_lines.tick(now, &self.log);
            self.other_lines.tick(now, &self.log);
        }
        self.pending.retain(|p| {
            now.saturating_duration_since(p.created) < PENDING_LIFETIME
                && !p.session.is_expired(now)
        });

        let lost_after = self.timers.lost_after;
        let gone: Vec<Peer> = self
            .peers
            .extract_if(.., |peer| {
                now.saturating_duration_since(peer.last_heard) >= lost_after
            })
            .collect();
        let someone_left = !gone.is_empty();
        let now_unix = crate::unix_now();
        for peer in gone {
            log!(
                self.log,
                "{}: nothing heard for {}, lost, out of the room",
                who(&peer),
                secs(lost_after)
            );
            self.devices.seen(&peer.key, now_unix);
            self.talk.forget(&peer.key, now);
            self.note_lost(peer.key, peer.last_heard, now);
            self.left_share(&peer.key, now, socket);
            self.keep_limits(peer.key, peer.share, peer.control, now);
        }
        changed |= self.change_step(now);

        let clock = self.clock;
        let mut went_quiet = false;
        let sending = self.sent_lately(now);
        for peer in &mut self.peers {
            if !peer.reconnecting
                && now.saturating_duration_since(peer.last_heard) >= self.timers.reconnecting_after
            {
                log!(
                    self.log,
                    "{}: nothing heard for {}, reconnecting",
                    who(peer),
                    secs(self.timers.reconnecting_after)
                );
                peer.reconnecting = true;
                changed = true;
                went_quiet = true;
            }
            if peer.sessions.expire(now) {
                log!(
                    self.log,
                    "{}: the session ran out without a rekey",
                    who(peer)
                );
                changed = true;
            }
            if peer.sessions.current.is_some() {
                let every = peer.link.ping_every(sending, now, &self.timers);
                // This PC may have started talking since the ping was planned.
                peer.link.next_ping = peer.link.next_ping.min(now + every);
                if now >= peer.link.next_ping {
                    peer.send_ping(socket, clock);
                    peer.link.next_ping = now + every;
                    changed = true;
                }
            }
            peer.flush(socket, now);
            peer.link.stats.tick(now);
            peer.jittery = chat::is_about(peer.link.stats.snapshot().jitter_ms);
            if self.log.is_on()
                && let Some(line) = peer.link.minute_line(now)
            {
                self.log.line(format!(
                    "reliable to {}: {line}",
                    keys::fingerprint(&peer.key)
                ));
            }
        }

        if went_quiet && self.all_went_quiet_together() {
            let asking = if self.stun.check(now, socket) {
                ", asking stun whether the public address changed"
            } else {
                ""
            };
            log!(
                self.log,
                "every friend went quiet within {} of each other{asking}",
                secs(TOGETHER)
            );
        }

        // STUN, the mapper and a new address list all reach the room through
        // here or on_packet, so this is where a change in them is noticed.
        self.tell_addresses(now, socket, None);

        changed |= self.talk.tick(now);
        if self.talk.report_due(now) {
            self.voice_reports(now, socket);
            self.share_reports(now, socket);
        }
        self.share_step(now, socket);
        self.control_step(now);

        changed |= someone_left;
        if someone_left || self.roster_due.is_some_and(|at| now >= at) {
            self.roster_changed(now, socket);
        } else if !self.peers.is_empty() && now >= self.next_roster {
            // A client that has not acked the last one gets nothing new, so a
            // dead path does not pile up a queue of stale rosters.
            let roster = Message::Roster(self.roster());
            for peer in self.peers.iter_mut().filter(|p| !p.link.has_unacked()) {
                peer.link.queue(&roster);
                peer.flush(socket, now);
            }
            self.next_roster = now + ROSTER_EVERY;
        }
        changed
    }

    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        if self.closed {
            return None;
        }
        let reject_after = self.session_timers.reject_after;
        let mut soonest = Soonest::default();
        soonest.add(self.stun.next_deadline());
        soonest.add(self.invites.next_deadline());
        if self.invites.is_wanted() {
            soonest.add(Some(self.first_invite_by));
        }
        for p in &self.pending {
            soonest.add(Some(
                (p.created + PENDING_LIFETIME).min(p.session.created() + reject_after),
            ));
        }
        for peer in &self.peers {
            if !peer.reconnecting {
                soonest.add(Some(peer.last_heard + self.timers.reconnecting_after));
            }
            soonest.add(Some(peer.last_heard + self.timers.lost_after));
            peer.sessions
                .deadlines(reject_after)
                .for_each(|at| soonest.add(Some(at)));
            if peer.sessions.current.is_some() {
                soonest.add(Some(peer.link.next_ping));
                soonest.add(peer.link.next_timeout());
            }
            if self.log.is_on() {
                soonest.add(Some(peer.link.next_minute_line()));
            }
        }
        if !self.peers.is_empty() {
            soonest.add(Some(self.next_roster));
        }
        soonest.add(self.roster_due);
        soonest.add(self.punches.next_deadline());
        soonest.add(self.cookies.next_deadline());
        soonest.add(self.booth_lines.next_deadline());
        soonest.add(self.other_lines.next_deadline());
        soonest.add(self.talk.next_deadline(!self.peers.is_empty()));
        soonest.add(self.share_deadline());
        soonest.add(self.control_deadline());
        if let Some(change) = &self.change {
            if !change.codes_over {
                soonest.add(Some(change.at + CODES_USELESS_FOR));
            }
            soonest.add(Some(change.at + CHANGE_SHOWN_FOR));
        }
        soonest.0
    }

    pub(crate) fn leave(&mut self, now: Instant, socket: &Socket) {
        if self.closed {
            return;
        }
        if self.log.is_on() {
            self.booth_lines.flush(now, &self.log);
            self.other_lines.flush(now, &self.log);
        }
        log!(
            self.log,
            "room closed, bye sent to {} people",
            self.peers.len()
        );
        self.closed = true;
        self.close_share();
        let now_unix = crate::unix_now();
        for peer in &mut self.peers {
            self.devices.seen(&peer.key, now_unix);
            peer.link.queue(&Message::Bye);
            if let Some(session) = peer.sessions.current.as_mut() {
                peer.link
                    .flush_twice(socket, session, &[peer.addr], now, &mut peer.traffic);
            }
        }
    }

    // The host is in the room too. Its line shows here at once and goes to
    // everyone else in the same order as what clients say. `text` has been
    // through chat::clean_text.
    pub(crate) fn say(
        &mut self,
        text: String,
        now: Instant,
        socket: &Socket,
    ) -> Result<(), ChatRefused> {
        if self.closed {
            return Err(ChatRefused::NotLive);
        }
        let author = *self.identity.public();
        let said = ChatMessage::Said {
            author,
            name: self.name.clone(),
            text: text.clone(),
            sent_at_host: Some(self.clock.micros(now)),
            about: false,
        };
        let len = text.len();
        self.history.push(ChatLine {
            author,
            name: self.name.clone(),
            text,
            at_unix_ms: crate::unix_now_ms(),
            mine: true,
            kind: LineKind::Said,
        });
        let to = self.hand_on(None, &said.encode(), now, socket);
        log!(
            self.log,
            "chat: {} wrote {len} bytes, this pc's own so no delivery time, handed to {}",
            keys::fingerprint(&author),
            counted(to, "friend", "friends")
        );
        Ok(())
    }

    // The socket stopped receiving. Nobody can reach this room any more, so
    // it stops rather than keep pinging clients it can no longer hear.
    pub(crate) fn socket_failed(&mut self) {
        self.closed = true;
        self.socket_failed = true;
        self.peers.clear();
        self.talk.keep_only(&[], Instant::now());
        self.talk.shared().room_over();
        self.pending.clear();
        self.close_share();
    }

    // `oversized` is what net dropped for not fitting the receive buffer,
    // before any of it reached this room. It is junk all the same.
    pub(crate) fn view(&self, now: Instant, oversized: u64) -> View {
        let snapshots: Vec<LinkSnapshot> = self
            .peers
            .iter()
            .map(|peer| peer.link.stats.snapshot())
            .collect();
        let mut numbers = Numbers {
            local_port: self.port,
            ping_interval: self.timers.ping_idle,
            dropped_bad: self.drops.bad + oversized,
            dropped_replay: self.drops.replayed,
            cookie_replies: self.cookies.replies_sent,
            dropped_no_cookie: self.drops.no_cookie,
            mapping: self.stun.mapping_word(),
            public_addr: self.stun.public(),
            mapping_protocol: self.portmap.protocol().map(|p| p.name().to_owned()),
            mapped_addr: self.portmap.mapped().map(SocketAddr::V4),
            address_name: self
                .address_name
                .as_ref()
                .map(|name| name.view(name.matched(self.outside()))),
            address_change: self.last_change,
            reconnect_ms: self.reconnect_ms,
            ..Numbers::default()
        };
        self.delivery.fill(&mut numbers, now);
        let sending = self.sent_lately(now);
        let strip = match worst(&self.peers, &snapshots) {
            Some(i) => {
                let peer = &self.peers[i];
                let voice = self.talk.on_link(&peer.key, now);
                let video = self.screen.on_link(&peer.key, now);
                LinkNumbers {
                    name: Some(peer.name.clone()),
                    reconnecting: peer.reconnecting,
                    snapshot: &snapshots[i],
                    voice,
                    video,
                    link: &peer.link,
                    sessions: &peer.sessions,
                    traffic: &peer.traffic,
                    path: Some(peer.path),
                    peer_addr: Some(peer.addr),
                    rekeys: peer.rekeys,
                    ping_interval: peer.link.ping_every(sending, now, &self.timers),
                }
                .fill(&mut numbers, now);
                peer.voice_out.add_to(&mut numbers);
                peer.share.video_out.add_to(&mut numbers);
                let state = if peer.reconnecting {
                    LinkState::Reconnecting
                } else {
                    LinkState::Live
                };
                let path = Some(peer.path);
                numbers::strip(state, &snapshots[i], voice, video, path, &self.thresholds)
            }
            None => Strip::default(),
        };
        let own = *self.identity.public();
        let far = worst(&self.peers, &snapshots).and_then(|i| self.peers[i].periods);
        let heard = self.talk.losses(now);
        let name = |key: &[u8; 32]| {
            if *key == own {
                return Some(self.name.clone());
            }
            let peer = self.peers.iter().find(|peer| peer.key == *key)?;
            Some(peer.name.clone())
        };
        // On the host each talker's loss is the worst any listener had.
        let loss =
            |key: &[u8; 32], own_ears: VoiceLoss| self.worst_loss(key, &heard).unwrap_or(own_ears);
        self.talk.fill(&mut numbers, name, loss, far, now);
        self.screen.fill(&mut numbers);

        let sharer = self.live.as_ref().map(|live| live.sharer);
        let mut people = vec![Person {
            key: own,
            name: self.name.clone(),
            fingerprint: keys::fingerprint(&own),
            rtt_ms: None,
            rtt_level: Default::default(),
            is_you: true,
            is_host: true,
            joined_by_invite: false,
            reconnecting: false,
            talking: self.talk.shared().sending(),
            sharing: sharer == Some(own),
        }];
        people.extend(self.peers.iter().zip(&snapshots).map(|(peer, snapshot)| {
            let rtt_ms = numbers::shown_rtt(peer.reconnecting, snapshot);
            Person {
                key: peer.key,
                name: peer.name.clone(),
                fingerprint: keys::fingerprint(&peer.key),
                rtt_ms,
                rtt_level: numbers::rtt_level(rtt_ms, &self.thresholds),
                is_you: false,
                is_host: false,
                joined_by_invite: peer.joined_by_invite,
                reconnecting: peer.reconnecting,
                talking: self.talk.talking(&peer.key, now),
                sharing: sharer == Some(peer.key),
            }
        }));

        View {
            role: Role::Host,
            room_name: self.room_name.clone(),
            strip,
            people,
            // Nobody can join through a socket that stopped working.
            invite: if self.socket_failed {
                None
            } else {
                let offered = self.portmap.offer().mapped.map(SocketAddr::V4);
                self.invites
                    .view(now, self.router_state())
                    .map(|shown| InviteView {
                        mapped_since: self.invites.shown_lacks(offered),
                        ..shown
                    })
            },
            numbers,
            chat: self.history.shared(),
            notice: self.socket_failed.then_some(Notice::SocketFailed),
            reply: None,
            paste: self.last_paste.as_ref().map(|(_, state)| state.clone()),
            address_changed: self.address_changed_view(),
            list_problem: self.list_problem.clone(),
            share: self.share_view(),
            voice: self.talk.shared().view(),
        }
    }

    // The public address changed mid-session: the router is asked for the
    // port again, the invite on show is marked, and every friend gets a ping
    // from the new address at once. Friends whose router lets that in follow
    // by the roaming rule.
    fn public_moved(&mut self, moved: Moved, now: Instant, socket: &Socket) {
        log!(
            self.log,
            "this pc's public address changed from {} to {}",
            moved.from,
            moved.to
        );
        // Only for its own records and the log: asking the router again is
        // part of mapping again.
        let _ = self.portmap.stun_seen(self.stun.public_v4_ip());
        self.portmap.note_verdict();
        self.note_name();
        if moved.to.is_ipv4() {
            self.remap();
        }
        let marked = self.invites.shown_before_change();

        // Anyone let go since STUN last saw the old address in this family
        // can have gone with the change, and friends who went with an
        // earlier one still count until they are back.
        let since = moved.from_confirmed.unwrap_or(self.opened);
        let mut watched: Vec<[u8; 32]> = self.peers.iter().map(|peer| peer.key).collect();
        let lost_since = self.lost.iter().filter(|lost| lost.at >= since);
        let earlier = self.change.take();
        let went_before = earlier.iter().flat_map(|change| change.watched.iter());
        for key in lost_since.map(|lost| &lost.key).chain(went_before) {
            if !watched.contains(key) {
                watched.push(*key);
            }
        }
        let pair = AddressChange {
            at_unix: crate::unix_now(),
            this_pc: true,
            from: moved.from,
            to: moved.to,
        };
        // A router that comes back shows a new address in both families,
        // one answer after the other in the same round. That is one change:
        // it counts from the first, friends were pinged then, and the stats
        // panel shows the IPv4 pair.
        if let Some(earlier) = earlier
            .filter(|change| now.saturating_duration_since(change.at) <= self.timers.stun_wait)
        {
            log!(
                self.log,
                "the same change as the one {} ago in the other family, friends were pinged then",
                secs(now.saturating_duration_since(earlier.at))
            );
            self.change = Some(Change { watched, ..earlier });
            if moved.to.is_ipv4() {
                self.last_change = Some(pair);
            }
            return;
        }
        if marked {
            log!(
                self.log,
                "the invite on show was made before the change, its outside addresses lead nowhere now"
            );
        }
        self.change = Some(Change {
            at: now,
            watched,
            codes_over: false,
        });
        self.last_change = Some(pair);

        let clock = self.clock;
        let sending = self.sent_lately(now);
        let mut pinged = Vec::new();
        for peer in &mut self.peers {
            if peer.sessions.current.is_none() {
                continue;
            }
            peer.send_ping(socket, clock);
            peer.link.next_ping = now + peer.link.ping_every(sending, now, &self.timers);
            pinged.push(peer.addr);
        }
        if pinged.is_empty() {
            log!(self.log, "nobody in the room to ping from the new address");
        } else {
            log!(
                self.log,
                "pinged {} at their last addresses from the new one: {}",
                counted(pinged.len() as u64, "friend", "friends"),
                list(&pinged)
            );
        }
    }

    // Some routers keep listing a mapping that stopped working after they
    // came back, so it is deleted and made again rather than renewed.
    fn remap(&mut self) {
        let (Some(remap), Some(router)) = (&self.remap_mapping, self.router) else {
            return;
        };
        match remap.try_send(router) {
            Ok(()) | Err(TrySendError::Full(_)) => log!(
                self.log,
                "port mapping: the router at {} is asked to delete the mapping and make it again",
                router.ip
            ),
            Err(TrySendError::Disconnected(_)) => log!(
                self.log,
                "port mapping: not made again, the mapper stopped after the router opened no port at the start"
            ),
        }
    }

    fn note_lost(&mut self, key: [u8; 32], last_heard: Instant, now: Instant) {
        let watched = |key: &[u8; 32]| {
            self.change
                .as_ref()
                .is_some_and(|change| change.watched.contains(key))
        };
        let lost = &mut self.lost;
        lost.retain(|l| {
            l.key != key
                && (now.saturating_duration_since(l.at) < CHANGE_SHOWN_FOR || watched(&l.key))
        });
        if lost.len() >= LOST_KEPT {
            lost.remove(0);
        }
        lost.push(Lost {
            key,
            last_heard,
            at: now,
        });
    }

    // The two sentences the panel shows after a change, while they apply.
    fn change_step(&mut self, now: Instant) -> bool {
        let Some(change) = &mut self.change else {
            return false;
        };
        if now >= change.at + CHANGE_SHOWN_FOR {
            self.change = None;
            return true;
        }
        if !change.codes_over && now >= change.at + CODES_USELESS_FOR {
            change.codes_over = true;
            return true;
        }
        false
    }

    fn address_changed_view(&self) -> Option<AddressChanged> {
        let change = self.change.as_ref()?;
        let friends_lost = change
            .watched
            .iter()
            .any(|key| !self.is_peer(key) && self.lost.iter().any(|lost| lost.key == *key));
        let reconnecting = self.peers.iter().any(|peer| peer.reconnecting);
        let codes_cannot_help = !change.codes_over && (reconnecting || friends_lost);
        (codes_cannot_help || friends_lost).then_some(AddressChanged {
            codes_cannot_help,
            friends_lost,
            name_set: self.address_name.is_some(),
        })
    }

    fn all_went_quiet_together(&self) -> bool {
        let heard = self.peers.iter().map(|peer| peer.last_heard);
        let (Some(first), Some(last)) = (heard.clone().min(), heard.max()) else {
            return false;
        };
        self.peers.iter().all(|peer| peer.reconnecting)
            && last.saturating_duration_since(first) <= TOGETHER
    }

    fn stun_settled(&mut self, now: Instant) {
        self.stun_answered();
        self.first_invite(now);
    }

    fn stun_answered(&mut self) {
        if self.portmap.stun_seen(self.stun.public_v4_ip())
            && let Some(renew) = &self.renew_mapping
        {
            let _ = renew.try_send(());
        }
        self.portmap.note_verdict();
        self.note_name();
    }

    // What STUN saw, or what the router says when STUN has not answered and
    // the router's address can be the one the internet sees.
    fn outside(&self) -> Option<Ipv4Addr> {
        self.stun.public_v4_ip().or_else(|| self.portmap.outside())
    }

    // Whether the name points here, written when the name has answered and
    // again whenever this PC's outside address changes the verdict.
    fn note_name(&mut self) {
        let Some(name) = self.address_name.as_ref().filter(|_| self.log.is_on()) else {
            return;
        };
        let line = match (name.v4().next(), name.matched(self.outside())) {
            (_, Some(found)) if found.is_this_pc() => format!(
                "address name {} points to {}, this pc's outside address",
                name.name(),
                found.points_to
            ),
            (_, Some(found)) => format!(
                "address name {} points to {}, not this pc's outside address {}; friends who look it up cannot reach this pc until the dynamic dns client updates it",
                name.name(),
                found.points_to,
                found.outside
            ),
            (Some(v4), None) => format!(
                "address name {} points to {v4}; stun and the router have not said what this pc's outside address is",
                name.name()
            ),
            (None, None) if name.has_answered() => format!(
                "address name {} has no ipv4 address to compare with this pc's outside address",
                name.name()
            ),
            (None, None) => return,
        };
        if self.name_said.as_ref() != Some(&line) {
            log!(self.log, "{line}");
            self.name_said = Some(line);
        }
    }

    // STUN and the router have both answered, or had until first_invite_by
    // to do so. A slow name lookup holds up STUN as much as a slow router
    // holds up the mapping.
    fn invite_ready(&self, now: Instant) -> bool {
        (self.stun.is_settled() && self.portmap.is_settled()) || now >= self.first_invite_by
    }

    // Returns true when it made the invite.
    fn first_invite(&mut self, now: Instant) -> bool {
        if !self.invites.is_wanted() || !self.invite_ready(now) {
            return false;
        }
        let waited_for = match (self.stun.is_settled(), self.portmap.is_settled()) {
            (true, true) => None,
            (false, true) => Some("stun"),
            (true, false) => Some("the port mapping"),
            (false, false) => Some("stun and the port mapping"),
        };
        if let Some(what) = waited_for {
            log!(
                self.log,
                "the first invite waits no longer for {what}, {} after the room opened",
                secs(now.saturating_duration_since(self.opened))
            );
        }
        match self.invites.take_wanted() {
            Some(multi_use) => {
                self.make_invite(multi_use, now);
                true
            }
            None => false,
        }
    }

    fn router_state(&self) -> RouterState {
        self.portmap.state(self.stun.router())
    }

    // What an invite made now would carry.
    fn candidates_now(&self) -> Vec<Candidate> {
        // The mapped address first. The guess that a port was forwarded by
        // hand stays only when it is not the same address.
        let mut public: Vec<_> = self.portmap.offer().mapped.into_iter().collect();
        public.extend(self.stun.public_for_invite(self.port));
        self.local.candidates(self.port, self.has_ipv6, &public)
    }

    // A friend keeps the host's addresses to rejoin with later, so each one
    // is told them on joining and everyone again whenever they change.
    // `joined` is a friend who just came in.
    fn tell_addresses(&mut self, now: Instant, socket: &Socket, joined: Option<usize>) {
        if self.peers.is_empty() {
            return;
        }
        let current = (
            known::clean_candidates(&self.candidates_now()),
            self.address_name
                .as_ref()
                .map(|name| name.name().to_owned()),
        );
        let changed = self.told.as_ref() != Some(&current);
        if !changed && joined.is_none() {
            return;
        }
        let message = Message::HostAddresses {
            candidates: current.0.clone(),
            address_name: current.1.clone(),
        };
        for (i, peer) in self.peers.iter_mut().enumerate() {
            if changed || joined == Some(i) {
                peer.link.queue(&message);
                peer.flush(socket, now);
            }
        }
        if changed && self.told.is_some() {
            log!(
                self.log,
                "this host's addresses changed, everyone in the room is told: {}",
                log::candidates(&current.0)
            );
        }
        self.told = Some(current);
    }

    fn make_invite(&mut self, multi_use: bool, now: Instant) {
        let mapping = self.stun.invite_mapping();
        let offer = self.portmap.offer();
        let recipe = Recipe {
            host_key: *self.identity.public(),
            candidates: self.candidates_now(),
            mapping,
            mapped: offer.mapped.map(SocketAddr::V4),
            mapped_verified: offer.verified,
            second_router: offer.second_router,
            address_name: self
                .address_name
                .as_ref()
                .map(|name| name.name().to_owned()),
            single_use_lifetime: self.timers.invite_single_use_lifetime,
            multi_use_lifetime: self.timers.invite_multi_use_lifetime,
            now,
            now_unix: crate::unix_now(),
        };
        // Fails only with a clock past 2106, and then the panel shows no
        // code: there is nothing a friend could paste that would work.
        match self.invites.make(recipe, multi_use) {
            Ok(made) if self.log.is_on() => {
                let uses = if multi_use { "multi use" } else { "single use" };
                log!(
                    self.log,
                    "invite made, {uses}, expires {}, mapping {}, mapped {}, verified {}, second router {}, {} candidates",
                    log::utc(made.expires_at_unix),
                    mapping_word(mapping),
                    yes_no(made.mapped),
                    yes_no(made.mapped_verified),
                    yes_no(made.second_router),
                    made.candidates.len()
                );
                for candidate in &made.candidates {
                    log!(
                        self.log,
                        "invite candidate {} {}",
                        kind_word(candidate.kind),
                        candidate.addr
                    );
                }
                if let Some(name) = &self.address_name {
                    log!(self.log, "invite address name {}", name.name());
                }
            }
            Ok(_) => {}
            Err(err) => log!(self.log, "could not make an invite: {err}"),
        }
    }

    fn on_initiation(
        &mut self,
        packet: &[u8],
        from: SocketAddr,
        now: Instant,
        socket: &Socket,
    ) -> bool {
        // Only initiations made for this host's key count toward load or get
        // a cookie. The rest go on to read_initiation, which drops them
        // before any key math and says whether the length or the key was
        // wrong.
        let has_mac1 = self.cookies.has_valid_mac1(packet);
        // The address and port a friend in the room is at has a bucket of its
        // own. The one for the IP alone is shared with everyone behind the
        // same NAT and with anyone spoofing it, and the table it lives in
        // fills up with strangers.
        let friend = if has_mac1 {
            self.peers
                .iter_mut()
                .find(|peer| peer.addr == from)
                .map(|peer| peer.handshakes.take(now, REKEYS_PER_SECOND, REKEY_BURST))
        } else {
            None
        };
        if friend == Some(false) {
            self.stray(packet, from, now, "initiation, dropped: rate limited");
            return self.drops.bad(now);
        }
        if has_mac1 {
            let under_load = self.cookies.count(now);
            self.note_load(now);
            if under_load && !self.cookies.has_valid_mac2(packet, from, now) {
                return self.send_cookie(packet, from, friend.is_some(), now, socket);
            }
        }
        // Without mac1 read_initiation drops it before any key math, so it
        // spends no tokens: junk from many or spoofed sources would use up
        // the budget every join and rejoin needs.
        let in_room = self.peers.iter().any(|peer| peer.addr.ip() == from.ip());
        if has_mac1 && friend.is_none() && !self.limit.allow(from.ip(), in_room, now) {
            self.stray(packet, from, now, "initiation, dropped: rate limited");
            return self.drops.bad(now);
        }
        if has_mac1 {
            self.initiations_read.fetch_add(1, Ordering::Relaxed);
        }
        let own = *self.identity.public();
        let incoming = match session::read_initiation(&self.identity.private_bytes(), &own, packet)
        {
            Ok(incoming) => incoming,
            Err(err) => {
                if self.log_stray(from, now) {
                    self.log.line(format!(
                        "{}: initiation, dropped: {}",
                        heard(packet, from),
                        unreadable(err)
                    ));
                }
                return self.drops.bad(now);
            }
        };
        let key = incoming.remote_public;
        let kind = incoming.kind;
        let stamp = incoming.timestamp;
        // Message 1 is where the key first shows. No answer: to that device
        // this host has gone quiet, and it learns nothing more.
        if self.devices.is_blocked(&key) {
            if self.log_stray(from, now) {
                self.log.line(format!(
                    "{}: initiation ({}) from {}, dropped: the key is blocked",
                    heard(packet, from),
                    init_word(kind),
                    keys::fingerprint(&key)
                ));
            }
            return self.drops.bad(now);
        }
        if let Some(record) = self.keys.get(&key)
            && record.stamp.is_some_and(|last| stamp <= last)
        {
            let copy = record.accepted.as_ref().is_some_and(|(bytes, at)| {
                bytes.as_slice() == packet && now.saturating_duration_since(*at) < COPY_WINDOW
            });
            let what = if copy {
                "a second copy of one already answered, over another path; ignored"
            } else {
                "dropped: old timestamp or replay"
            };
            if self.log_stray(from, now) {
                self.log.line(format!(
                    "{}: initiation ({}) from {}, {what}",
                    heard(packet, from),
                    init_word(kind),
                    keys::fingerprint(&key)
                ));
            }
            // Neither is answered. Only the replay is counted.
            return if copy {
                false
            } else {
                self.drops.replayed(now)
            };
        }
        // The punches opened this host's router for that one friend. Anyone
        // else who can send from their address keeps out of it with an
        // invite; a key with a secret of its own takes nothing from them.
        if matches!(kind, InitKind::Invite(_))
            && let Some(owner) = self.punches.held_for_other(from, &key, now)
        {
            if self.log_stray(from, now) {
                self.log.line(format!(
                    "{}: initiation ({}) from {}, dropped: {from} was punched open for {} only",
                    heard(packet, from),
                    init_word(kind),
                    keys::fingerprint(&key),
                    keys::fingerprint(&owner)
                ));
            }
            return self.drops.bad(now);
        }
        let room_full = !self.is_peer(&key) && self.seats_taken(&key) >= MAX_CLIENTS;
        let psk = match self.psk_for(&key, kind, now) {
            Err(why) => Err(why),
            Ok(_) if key == own => Err("it carries this host's own key"),
            Ok(_) if room_full => Err("room full"),
            Ok(_) if !self.make_room_for(&key) => {
                Err("the key table is full of people in the room and handshakes in flight")
            }
            Ok(psk) => Ok(psk),
        };
        let psk = match psk {
            Ok(psk) => psk,
            Err(why) => {
                if self.log_stray(from, now) {
                    self.log.line(format!(
                        "{}: initiation ({}) from {}, dropped: {why}",
                        heard(packet, from),
                        init_word(kind),
                        keys::fingerprint(&key)
                    ));
                }
                return self.drops.bad(now);
            }
        };
        let index = self.fresh_index();
        let (session, response) = match incoming.accept(&psk, index, now) {
            Ok(accepted) => accepted,
            Err(err) => {
                if self.log_stray(from, now) {
                    self.log.line(format!(
                        "{}: initiation ({}) from {}, dropped: noise failure, {err}",
                        heard(packet, from),
                        init_word(kind),
                        keys::fingerprint(&key)
                    ));
                }
                return self.drops.bad(now);
            }
        };
        match self.peers.iter_mut().find(|peer| peer.key == key) {
            Some(peer) => peer.traffic.send(socket, &response, from),
            None => {
                let _ = socket.send_to(&response, from);
            }
        }

        let record = self.keys.entry(key).or_insert_with(|| KeyRecord::new(now));
        record.stamp = Some(stamp);
        record.accepted = Some((packet.to_vec(), now));
        record.last_seen = now;
        // Message 1 shows the key but not the invite secret, which IKpsk2
        // mixes in at the end of message 2. The invite is spent in confirm,
        // once a packet under the new keys shows the client had it.
        let by_invite = matches!(kind, InitKind::Invite(_));
        self.pending.push(Pending {
            session: session.with_timers(self.session_timers),
            key,
            kind,
            created: now,
            answered_at: from,
        });
        let trimmed = self.trim_pending(&key);
        if self.log_stray(from, now) {
            let trimmed = trimmed.map_or(String::new(), |what| format!("; {what}"));
            self.log.line(format!(
                "{}: initiation ({}) from {}, answered{trimmed}",
                heard(packet, from),
                init_word(kind),
                keys::fingerprint(&key)
            ));
        }
        by_invite
    }

    // Under load an initiation without a valid mac2 gets a cookie reply,
    // smaller than itself, and nothing else. A source that sends the cookie
    // back has shown it receives at its address.
    fn send_cookie(
        &mut self,
        packet: &[u8],
        from: SocketAddr,
        friend: bool,
        now: Instant,
        socket: &Socket,
    ) -> bool {
        let changed = self.drops.no_cookie(now);
        // A flood is what brings these, so one line a minute per source, or
        // they would push the rest out of the log. The stats panel counts
        // every one.
        let line =
            self.log.is_on() && self.cookies.line_due(from.ip(), now) && self.log_stray(from, now);
        // A friend's own bucket was spent on the way in. The cap is for
        // everyone else, so a flood that keeps it used up cannot stop a
        // rekey.
        let allowed = if friend {
            Ok(())
        } else {
            self.limit.allow_cookie(from.ip(), now)
        };
        let outcome = match allowed {
            Ok(()) => match self.cookies.reply(packet, from, now) {
                Ok(reply) => match socket.send_to(&reply, from) {
                    Ok(_) => {
                        self.cookies.replies_sent += 1;
                        Ok("cookie reply sent")
                    }
                    Err(err) => Err(format!("could not send the cookie reply: {err}")),
                },
                // mac1 was checked on the way in, so only a bug fails here.
                Err(err) => Err(format!("could not make a cookie reply: {err}")),
            },
            Err(Refused::Source) => Ok("rate limited, no cookie reply"),
            Err(Refused::Cap) => {
                if let Some(count) = self.cookies.capped(now) {
                    self.say_capped(count);
                }
                Ok("cookie replies at their cap, no cookie reply")
            }
        };
        if line {
            let what = match outcome {
                Ok(what) => what.to_owned(),
                Err(what) => what,
            };
            self.log.line(format!(
                "{}: initiation without a valid mac2 under load, dropped: {what}; one line a minute per source",
                heard(packet, from)
            ));
        }
        changed
    }

    fn note_load(&mut self, now: Instant) {
        match self.cookies.changed(now) {
            Some(true) => log!(
                self.log,
                "under load: {} initiations within a second; one without a valid mac2 now gets a cookie reply and no key math",
                self.timers.load_initiations
            ),
            Some(false) => log!(
                self.log,
                "no longer under load: under {} initiations a second for {}; {} cookie replies sent and {} initiations dropped for want of mac2 so far",
                self.timers.load_initiations,
                secs(self.timers.load_calm),
                self.cookies.replies_sent,
                self.drops.no_cookie
            ),
            None => {}
        }
    }

    fn say_capped(&self, count: u64) {
        log!(
            self.log,
            "cookie replies at their cap, {count} dropped in the last second"
        );
    }

    // The error is why not, for the log.
    fn psk_for(
        &self,
        key: &[u8; 32],
        kind: InitKind,
        now: Instant,
    ) -> Result<Zeroizing<[u8; 32]>, &'static str> {
        const NO_SECRET: &str = "no per-peer secret for this key";
        match kind {
            InitKind::Invite(id) => {
                let secret = self
                    .invites
                    .secret_for(&id, key, now)
                    .ok_or_else(|| self.invites.refusal(&id, key, now))?;
                Ok(session::invite_psk(&secret, self.identity.public(), key))
            }
            InitKind::Known => self
                .keys
                .get(key)
                .and_then(|record| record.secret.clone())
                .ok_or(NO_SECRET),
            InitKind::Rekey => {
                let live = self
                    .peers
                    .iter()
                    .any(|peer| peer.key == *key && peer.has_live_session(now));
                if !live {
                    return Err("rekey without a live link");
                }
                self.keys
                    .get(key)
                    .and_then(|record| record.secret.clone())
                    .ok_or(NO_SECRET)
            }
        }
    }

    // Every key that completes a join stays on the list, secret and all,
    // until the host removes it in settings, so a friend can come back
    // without a new invite.
    fn remember(&mut self, key: [u8; 32], secret: &Zeroizing<[u8; 32]>) {
        let peers = &self.peers;
        let in_room = |k: &[u8; 32]| peers.iter().any(|peer| peer.key == *k);
        let fingerprint = keys::fingerprint(&key);
        match self.devices.joined(key, secret, crate::unix_now(), in_room) {
            Joined::Added => log!(self.log, "{fingerprint}: kept as a known device"),
            Joined::AddedInPlaceOf(gone) => log!(
                self.log,
                "{fingerprint}: kept as a known device in place of {}, the one seen longest ago, since the list holds {}",
                keys::fingerprint(&gone),
                known::MAX_DEVICES
            ),
            Joined::Again => {}
            Joined::NotKept => log!(
                self.log,
                "{fingerprint}: not kept as a known device, the list could not be read when the room opened and is not written to"
            ),
        }
    }

    fn is_peer(&self, key: &[u8; 32]) -> bool {
        self.peers.iter().any(|peer| peer.key == *key)
    }

    // A key that was answered and has not confirmed yet holds a seat, so two
    // joining at the same moment cannot both be answered for the last one.
    // Not with an invite: anyone with its id gets that answer, secret or
    // not, and confirm turns away whoever finds the room full.
    fn seats_taken(&self, key: &[u8; 32]) -> usize {
        let mut waiting: Vec<&[u8; 32]> = Vec::new();
        for p in &self.pending {
            if p.key != *key
                && !matches!(p.kind, InitKind::Invite(_))
                && !self.is_peer(&p.key)
                && !waiting.contains(&&p.key)
            {
                waiting.push(&p.key);
            }
        }
        self.peers.len() + waiting.len()
    }

    // A known device keeps its secret in the list and never makes way here.
    // Any other key with a secret is one whose join the list could not keep,
    // and dropping it costs that device a new invite.
    fn make_room_for(&mut self, key: &[u8; 32]) -> bool {
        if self.keys.len() < MAX_KEYS || self.keys.contains_key(key) {
            return true;
        }
        let oldest = self
            .keys
            .iter()
            .filter(|(k, _)| {
                !self.is_peer(k)
                    && !self.pending.iter().any(|p| p.key == **k)
                    && !self.devices.is_known(k)
            })
            .min_by_key(|(_, record)| record.last_seen)
            .map(|(k, _)| *k);
        match oldest {
            Some(k) => {
                self.keys.remove(&k);
                true
            }
            None => false,
        }
    }

    fn fresh_index(&self) -> u32 {
        peer::random_index(|index| {
            self.peers.iter().any(|peer| peer.sessions.holds(index))
                || self
                    .pending
                    .iter()
                    .any(|p| p.session.local_index() == index)
        })
    }

    // Says what was let go, for the log.
    fn trim_pending(&mut self, key: &[u8; 32]) -> Option<&'static str> {
        let mut trimmed = None;
        let mine = self.pending.iter().filter(|p| p.key == *key).count();
        if mine > PENDING_PER_KEY
            && let Some(oldest) = self.pending.iter().position(|p| p.key == *key)
        {
            self.pending.remove(oldest);
            trimmed = Some("too many pending handshakes for this key, its oldest was let go");
        }
        if self.pending.len() > PENDING_TOTAL {
            // The key holding the most tries lets its oldest go, a
            // stranger's before a friend's, so a flood of new keys cannot
            // push out the rekey a friend in the room is about to confirm,
            // and friends holding many tries cannot push out a newcomer's
            // one. Backwards, so a tie goes to the oldest.
            let tries = |k: &[u8; 32]| self.pending.iter().filter(|p| p.key == *k).count();
            let at = (0..self.pending.len())
                .rev()
                .max_by_key(|&i| {
                    let k = &self.pending[i].key;
                    (tries(k), !self.is_peer(k))
                })
                .unwrap_or(0);
            self.pending.remove(at);
            trimmed = Some("too many pending handshakes, one was let go");
        }
        trimmed
    }

    fn on_data(
        &mut self,
        packet: &[u8],
        from: SocketAddr,
        now: Instant,
        socket: &Socket,
        plain: &mut Vec<u8>,
    ) -> bool {
        let Some(index) = session::data_receiver_index(packet) else {
            self.stray(packet, from, now, "data, dropped: bad length");
            return self.drops.bad(now);
        };
        let found = self
            .peers
            .iter()
            .enumerate()
            .find_map(|(i, peer)| peer.sessions.which(index).map(|which| (i, which)));
        if let Some((i, which)) = found {
            let peer = &mut self.peers[i];
            let Some(session) = peer.sessions.get_mut(which) else {
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
            peer.traffic.received(packet.len());
            // Only the current session moves the address: a late packet on the
            // old one would drag it back to where the client used to be.
            let roamed = which == Which::Current && received.newest && from != peer.addr;
            let was_at = peer.addr;
            let back = peer.heard(from, roamed, now, self.timers.quiet_after(), self.clock);
            // Within the per-source limit: a friend can move with every packet.
            if roamed && self.log.is_on() && self.booth_lines.allow(from.ip(), now, &self.log) {
                self.log
                    .line(format!("{}: moved from {was_at} to {from}", who(peer)));
            }
            if let Some(silence) = back {
                log!(
                    self.log,
                    "{}: heard again after {}",
                    who(peer),
                    secs(silence)
                );
                self.reconnect_ms = Some(numbers::millis(silence));
            }
            return self.on_plain(i, plain, from, now, socket) || roamed;
        }

        let Some(p) = self
            .pending
            .iter()
            .position(|p| p.session.local_index() == index)
        else {
            // The second copy of a Bye, or a packet that was on its way when
            // they left. Not junk, and nothing to write down.
            if self.left_just_now(index, now) {
                return false;
            }
            self.stray(packet, from, now, "data for an unknown session, dropped");
            return self.drops.bad(now);
        };
        match peer::open(&mut self.pending[p].session, packet, plain) {
            Opened::Data(_) => {}
            Opened::Replayed => {
                self.stray(
                    packet,
                    from,
                    now,
                    "data for a pending handshake, replayed, dropped",
                );
                return self.drops.replayed(now);
            }
            Opened::Bad => {
                self.stray(
                    packet,
                    from,
                    now,
                    "data for a pending handshake that did not decrypt, dropped",
                );
                return self.drops.bad(now);
            }
        }
        let Some(i) = self.confirm(p, from, now, socket) else {
            return false;
        };
        self.peers[i].traffic.received(packet.len());
        self.on_plain(i, plain, from, now, socket);
        true
    }

    fn confirm(
        &mut self,
        p: usize,
        from: SocketAddr,
        now: Instant,
        socket: &Socket,
    ) -> Option<usize> {
        let Pending {
            session,
            key,
            kind,
            answered_at,
            ..
        } = self.pending.remove(p);
        // The client keeps the first answer it gets and forgets its other tries.
        self.pending.retain(|other| other.key != key);
        // Two keys can be answered on one single-use invite. The first to
        // confirm takes it.
        if let InitKind::Invite(id) = kind {
            if !self.invites.admit(&id, key) {
                log!(
                    self.log,
                    "{from}: handshake from {} finished, dropped: its invite let in another key first",
                    keys::fingerprint(&key)
                );
                self.drops.bad(now);
                return None;
            }
            if let Some(record) = self.keys.get_mut(&key) {
                record.invited = true;
            }
        }
        let invited = match self.keys.get_mut(&key) {
            Some(record) => {
                record.last_seen = now;
                record.invited
            }
            None => false,
        };
        let quiet_after = self.timers.quiet_after();
        let clock = self.clock;

        let i = match self.peers.iter().position(|peer| peer.key == key) {
            Some(i) => {
                let peer = &mut self.peers[i];
                if kind == InitKind::Rekey {
                    peer.sessions.previous = peer.sessions.current.replace(session);
                    peer.rekeys += 1;
                } else {
                    // Its sequence numbers belong to the reliable stream both
                    // sides are about to start over, so it cannot stay around.
                    peer.sessions.previous = None;
                    peer.sessions.current = Some(session);
                    peer.restart(now);
                }
                peer.joined_by_invite |= invited;
                let was_at = peer.addr;
                let moved = from != peer.addr;
                if let Some(silence) = peer.heard(from, moved, now, quiet_after, clock) {
                    self.reconnect_ms = Some(numbers::millis(silence));
                }
                let peer = &self.peers[i];
                if kind == InitKind::Rekey {
                    log!(self.log, "{}: rekey confirmed", who(peer));
                } else {
                    log!(
                        self.log,
                        "{}: session confirmed again ({}) at {from}, path {}",
                        who(peer),
                        init_word(kind),
                        path_text(peer.path)
                    );
                }
                if moved {
                    log!(self.log, "{}: moved from {was_at} to {from}", who(peer));
                }
                i
            }
            None => {
                let refused = if kind == InitKind::Rekey {
                    Some("a rekey from a key not in the room")
                } else if self.peers.len() >= MAX_CLIENTS {
                    Some("room full")
                } else {
                    None
                };
                if let Some(why) = refused {
                    log!(
                        self.log,
                        "{from}: handshake from {} finished, dropped: {why}",
                        keys::fingerprint(&key)
                    );
                    self.drops.bad(now);
                    return None;
                }
                let slot = self.free_slot();
                // The packet that confirmed can come from anywhere. Media
                // starts where the response went and follows it to `from`
                // once a ping there is answered, as for any move.
                let mut peer = Peer::new(key, answered_at, session, invited, slot, now);
                if from != answered_at {
                    peer.move_to(from, now, clock);
                }
                self.peers.push(peer);
                self.limits_back(self.peers.len() - 1, now);
                let peer = &self.peers[self.peers.len() - 1];
                log!(
                    self.log,
                    "{}: session confirmed ({}) at {from}, path {}",
                    keys::fingerprint(&key),
                    init_word(kind),
                    path_text(peer.path)
                );
                if let Some(at) = self.lost.iter().position(|lost| lost.key == key) {
                    let silence = now.saturating_duration_since(self.lost[at].last_heard);
                    log!(
                        self.log,
                        "{}: back after {} of silence, through a new handshake",
                        keys::fingerprint(&key),
                        secs(silence)
                    );
                    self.reconnect_ms = Some(numbers::millis(silence));
                    self.lost.remove(at);
                }
                self.peers.len() - 1
            }
        };

        if kind != InitKind::Rekey {
            // First on the new stream, before anything else can be queued on
            // it: a client takes anything else first for a build from before
            // version numbers.
            self.peers[i].link.queue(&Message::Hello {
                version: invite::VERSION,
                name: self.name.clone(),
                reached: None,
            });
            let secret = self
                .keys
                .entry(key)
                .or_insert_with(|| KeyRecord::new(now))
                .secret
                .get_or_insert_with(new_secret)
                .clone();
            self.remember(key, &secret);
            // A link that started over starts its share and its watching
            // over too.
            self.left_share(&key, now, socket);
            self.peers[i].link.queue(&Message::PeerSecret { secret });
            self.tell_addresses(now, socket, Some(i));
            self.peers[i].flush(socket, now);
            self.roster_changed(now, socket);
            if let Some(rounds) = self.punches.joined(&key) {
                log!(
                    self.log,
                    "{} joined after {}, no more are sent",
                    keys::fingerprint(&key),
                    counted(u64::from(rounds), "punch round", "punch rounds")
                );
            }
            if let Some((pasted, state)) = &mut self.last_paste
                && *pasted == key
            {
                *state = PasteState::Joined;
            }
        }
        Some(i)
    }

    fn on_plain(
        &mut self,
        i: usize,
        plain: &mut [u8],
        from: SocketAddr,
        now: Instant,
        socket: &Socket,
    ) -> bool {
        // Video, pointers and input are passed on with their prefix turned
        // around in place, so they are taken before the read that borrows it.
        match plain.first().map(|&byte| Channel::try_from(byte)) {
            Some(Ok(Channel::Video)) => return self.on_video(i, plain, now, socket),
            Some(Ok(Channel::Cursor)) => return self.on_cursor(i, plain, now, socket),
            Some(Ok(Channel::Input)) => return self.on_input(i, plain, now, socket),
            _ => {}
        }
        let clock = self.clock;
        let peer = &mut self.peers[i];
        match peer::read_plain(plain) {
            Some(Plain::Ping(PingMessage::Ping { seq, t1 })) => {
                let pong = peer.link.answer(seq, t1, now, clock);
                peer.send(socket, Channel::Ping, &pong);
                true
            }
            Some(Plain::Ping(pong)) => {
                if !peer.link.pong(pong, now, clock) {
                    self.drops.bad(now);
                } else if let PingMessage::Pong { seq, t1, .. } = pong
                    && peer.media.answered(from, seq, t1)
                {
                    log!(
                        self.log,
                        "{}: answered a ping at {from}, voice and video go there now",
                        who(peer)
                    );
                }
                true
            }
            Some(Plain::Control(frame)) => self.on_control(i, frame, now, socket),
            Some(Plain::Chat(frame)) => self.on_chat(i, frame, now, socket),
            Some(Plain::Voice(payload)) => self.on_voice(i, payload, now, socket),
            // Taken above.
            Some(Plain::Video(_) | Plain::Cursor(_) | Plain::Input(_)) | None => {
                self.drops.bad(now)
            }
        }
    }

    // A friend's voice is hostile input until it parses. It goes on to
    // everyone else under the slot this host gave its sender, whatever it
    // says, with its capture time moved to this host's clock, and is never
    // decoded to do so. The host is a listener too.
    fn on_voice(&mut self, i: usize, payload: &[u8], now: Instant, socket: &Socket) -> bool {
        let peer = &mut self.peers[i];
        // Over the rate is not malformed, so it is counted with voice only.
        if !peer.voice.take(now, VOICE_PER_SECOND, VOICE_BURST) {
            self.talk.dropped();
            return false;
        }
        let frame = match talk::read_spoken(payload) {
            Ok(frame) => frame,
            Err(why) => {
                self.talk.dropped();
                let (key, ip) = (peer.key, peer.addr.ip());
                if self.log.is_on() && self.booth_lines.allow(ip, now, &self.log) {
                    self.log.line(format!(
                        "voice from {}, {} bytes, dropped: {why}",
                        keys::fingerprint(&key),
                        payload.len()
                    ));
                }
                return self.drops.bad(now);
            }
        };
        let timers = self.timers;
        peer.link.media_passed(now, &timers);
        let offset = peer.link.offset.best().map(|sample| sample.offset_us);
        let (captured, about) = talk::our_time(frame.captured, offset, peer.jittery);
        let (key, slot) = (peer.key, peer.slot);
        frame.write_relayed(slot, captured, about, &mut self.voice_payload);
        self.voice_plain.clear();
        channels::frame(Channel::Voice, &self.voice_payload, &mut self.voice_plain);
        for (j, other) in self.peers.iter_mut().enumerate() {
            if j == i {
                continue;
            }
            if let Some(session) = other.sessions.current.as_mut()
                && session
                    .encrypt(&self.voice_plain, &mut self.voice_sealed)
                    .is_ok()
            {
                other
                    .traffic
                    .send(socket, &self.voice_sealed, other.media.to());
                other.link.media_passed(now, &timers);
            }
        }
        self.talk.hear(key, &frame, captured, about, now)
    }

    // What a listener lost of each talker. A report that is the first
    // scattered loss anyone has heard of a talker goes to them at once, so
    // their redundancy is on within 2 s of the loss; the rest wait for the
    // report once a second. Loss in an outage alone switches nothing, so it
    // waits too.
    fn took_losses(
        &mut self,
        i: usize,
        heard: &[(u8, LossPermille)],
        now: Instant,
        socket: &Socket,
    ) {
        let from = self.peers[i].key;
        let own = *self.identity.public();
        for &(slot, lost) in heard {
            let talker = if slot == talk::HOST_SLOT {
                own
            } else {
                match self.peers.iter().find(|peer| peer.slot == slot) {
                    Some(peer) => peer.key,
                    None => continue,
                }
            };
            if talker == from {
                continue;
            }
            let loss = talk::from_wire(lost);
            let was_clean = self
                .worst_loss(&talker, &[])
                .is_none_or(|worst| worst.scattered_pct <= 0.0);
            self.reports
                .retain(|report| report.talker != talker || report.from != from);
            self.reports.push(LossReport {
                talker,
                from,
                loss,
                at: now,
            });
            if loss.scattered_pct > 0.0 && was_clean {
                let Some(worst) = self.worst_loss(&talker, &[]) else {
                    continue;
                };
                if talker == own {
                    self.talk.listeners_lost(worst, now);
                } else if let Some(peer) = self.peers.iter_mut().find(|peer| peer.key == talker) {
                    peer.link.queue(&Message::WorstLoss(talk::to_wire(worst)));
                    peer.flush(socket, now);
                }
            }
        }
    }

    // The worst loss any listener had of `talker` over the last 2 s, each
    // number on its own: the reports, and this host's own ears in `heard`.
    fn worst_loss(&self, talker: &[u8; 32], heard: &[([u8; 32], VoiceLoss)]) -> Option<VoiceLoss> {
        let own = heard
            .iter()
            .filter(|(key, _)| key == talker)
            .map(|(_, loss)| *loss);
        let reported = self
            .reports
            .iter()
            .filter(|report| report.talker == *talker)
            .map(|report| report.loss);
        own.chain(reported).reduce(talk::worse)
    }

    // Once a second: each talker hears the worst of what its listeners lost,
    // this host acts on what its own listeners lost, and anyone whose view
    // of this host's audio periods is out of date gets them.
    fn voice_reports(&mut self, now: Instant, socket: &Socket) {
        self.reports
            .retain(|report| now.saturating_duration_since(report.at) < talk::HEARD_LATELY);
        let heard = self.talk.losses(now);
        let worst: Vec<Option<VoiceLoss>> = self
            .peers
            .iter()
            .map(|peer| self.worst_loss(&peer.key, &heard))
            .collect();
        let own = *self.identity.public();
        if let Some(loss) = self.worst_loss(&own, &heard) {
            self.talk.listeners_lost(loss, now);
        }
        let periods = self.talk.periods();
        for (peer, worst) in self.peers.iter_mut().zip(worst) {
            if let Some(loss) = worst {
                peer.link.queue(&Message::WorstLoss(talk::to_wire(loss)));
            }
            if peer.hello && peer.told_periods != Some(periods) {
                peer.link.queue(&Message::Periods(periods));
                peer.told_periods = Some(periods);
            }
            peer.flush(socket, now);
        }
    }

    // The links the capture thread sends this host's voice on: every friend
    // in the room, at the address media goes to, until it closes.
    pub(crate) fn publish_voice(&mut self) {
        let live = !self.closed;
        let peers = &self.peers;
        let links = peers.iter().filter(|_| live).filter_map(|peer| {
            let session = peer.sessions.current.as_ref()?;
            session
                .is_confirmed()
                .then(|| (session.remote_index(), peer.media.to()))
        });
        self.talk.publish(links, || {
            peers
                .iter()
                .filter(|_| live)
                .filter_map(|peer| {
                    Some(Outlet {
                        sealer: peer.sessions.current.as_ref()?.sealer()?,
                        to: peer.media.to(),
                        sent: Arc::clone(&peer.voice_out),
                    })
                })
                .collect()
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

    // This host's own voice or video went out lately, to every link.
    fn sent_lately(&self, now: Instant) -> bool {
        self.talk.sent_lately(now) || self.screen.sharing.sent_lately(now)
    }

    // The next slot after the one given last that nobody holds. The room is
    // never fuller than MAX_CLIENTS here, so there is one.
    fn free_slot(&mut self) -> u8 {
        let slots = MAX_CLIENTS as u8;
        let slot = (1..=slots)
            .map(|step| (self.last_slot + step - 1) % slots + 1)
            .find(|slot| !self.peers.iter().any(|peer| peer.slot == *slot))
            .unwrap_or(1);
        self.last_slot = slot;
        slot
    }

    fn on_chat(&mut self, i: usize, frame: &[u8], now: Instant, socket: &Socket) -> bool {
        let peer = &mut self.peers[i];
        if peer.link.receive_chat(frame, now).is_err() {
            return self.drops.bad(now);
        }
        self.take_says(i, now, socket);
        true
    }

    // Only once the Hello on this link has come. A friend whose link started
    // over sends the lines the old one never got right behind their Hello,
    // and if the Hello were lost on the way they would go out under the
    // fallback name. Until then they wait in the stream.
    fn take_says(&mut self, i: usize, now: Instant, socket: &Socket) {
        let peer = &mut self.peers[i];
        let mut delivered = Vec::new();
        if peer.hello {
            while let Some(message) = peer.link.chat.next_delivered() {
                delivered.push(message);
            }
        }
        peer.flush(socket, now);
        for bytes in delivered {
            self.took_say(i, &bytes, now, socket);
        }
    }

    // What a friend's PC sends is hostile input.
    // The text is cleaned again here, and a message that breaks the rules
    // even then goes no further than this line of the log. So does one past
    // what a friend may say: a flood would fill the others' streams, and
    // they would miss lines.
    fn took_say(&mut self, i: usize, bytes: &[u8], now: Instant, socket: &Socket) {
        let peer = &mut self.peers[i];
        if !peer.says.take(now, chat::SAYS_PER_SECOND, chat::SAY_BURST) {
            let why = format!("over {} lines a second", chat::SAYS_PER_SECOND);
            self.refused_say(i, bytes.len(), &why, now);
            return;
        }
        let refused = match ChatMessage::decode(bytes) {
            Some(ChatMessage::Say { text, sent_at }) => match chat::clean_text(&text) {
                Ok(text) => Ok((text, sent_at)),
                Err(why) => Err(why.to_string()),
            },
            Some(ChatMessage::Said { .. }) => Err(String::from("only a host hands messages on")),
            None => Err(String::from("it does not parse")),
        };
        let (text, sent_at) = match refused {
            Ok(taken) => taken,
            Err(why) => {
                self.refused_say(i, bytes.len(), &why, now);
                return;
            }
        };
        let peer = &self.peers[i];
        let (author, name, ip) = (peer.key, peer.name.clone(), peer.addr.ip());
        let offset = peer.link.offset.best().map(|sample| sample.offset_us);
        let delivery = offset
            .and_then(|offset| chat::delivery_ms(sent_at, offset, self.clock.micros(now)))
            .map(|ms| (ms, chat::is_about(peer.link.stats.snapshot().jitter_ms)));
        // Without a time this host believes, the others get none either.
        let sent_at_host = offset
            .filter(|_| delivery.is_some())
            .map(|offset| chat::to_our_clock(sent_at, offset));
        if let Some((ms, about)) = delivery {
            self.delivery.record(now, ms, about);
        }
        let len = text.len();
        let said = ChatMessage::Said {
            author,
            name: name.clone(),
            text: text.clone(),
            sent_at_host,
            about: delivery.is_some_and(|(_, about)| about),
        };
        self.history.push(ChatLine {
            author,
            name,
            text,
            at_unix_ms: crate::unix_now_ms(),
            mine: false,
            kind: LineKind::Said,
        });
        let to = self.hand_on(Some(i), &said.encode(), now, socket);
        // Within the per-source limit: a friend's program can say something
        // with every packet.
        if self.log.is_on() && self.booth_lines.allow(ip, now, &self.log) {
            self.log.line(format!(
                "chat: {} wrote {len} bytes, {}, handed to {}",
                keys::fingerprint(&author),
                log::delivery(delivery),
                counted(to, "friend", "friends")
            ));
        }
    }

    fn refused_say(&mut self, i: usize, len: usize, why: &str, now: Instant) {
        self.drops.bad(now);
        let peer = &self.peers[i];
        let (key, ip) = (peer.key, peer.addr.ip());
        if self.log.is_on() && self.booth_lines.allow(ip, now, &self.log) {
            self.log.line(format!(
                "chat from {}, {len} bytes, refused and not handed on: {why}",
                keys::fingerprint(&key)
            ));
        }
    }

    // To everyone in the room but its author, in the order this host took
    // it. Returns how many it went to.
    fn hand_on(
        &mut self,
        author: Option<usize>,
        said: &[u8],
        now: Instant,
        socket: &Socket,
    ) -> u64 {
        let mut to = 0;
        for (i, peer) in self.peers.iter_mut().enumerate() {
            if Some(i) == author {
                continue;
            }
            // With what each friend may say limited, full only after a long
            // silence, and the silence timers let that friend go soon after.
            if peer.link.queue_chat(said).is_ok() {
                to += 1;
            }
            peer.flush(socket, now);
        }
        to
    }

    fn on_control(&mut self, i: usize, frame: &[u8], now: Instant, socket: &Socket) -> bool {
        let peer = &mut self.peers[i];
        if peer.link.receive(frame, now).is_err() {
            return self.drops.bad(now);
        }
        let mut delivered = Vec::new();
        while let Some(message) = peer.link.reliable.next_delivered() {
            delivered.push(message);
        }
        peer.flush(socket, now);

        let mut renamed = false;
        let mut greeted = false;
        for bytes in delivered {
            match Message::decode(&bytes) {
                Some(Message::Hello {
                    version,
                    name,
                    reached,
                }) => {
                    let proved = reached.and_then(|addr| self.portmap.reached(addr));
                    let first = !self.peers[i].hello;
                    greeted |= first;
                    self.peers[i].hello = true;
                    let peer = &self.peers[i];
                    if first
                        && version != invite::VERSION
                        && self.log.is_on()
                        && self.booth_lines.allow(peer.addr.ip(), now, &self.log)
                    {
                        self.log.line(format!(
                            "{} runs booth {version}, this pc {}, the same protocol",
                            keys::fingerprint(&peer.key),
                            invite::VERSION
                        ));
                    }
                    self.devices.named(&self.peers[i].key, &name);
                    if self.peers[i].name != name {
                        self.peers[i].name = name;
                        renamed = true;
                        // Within the per-source limit: a friend's program can
                        // send a new name with every packet.
                        let peer = &self.peers[i];
                        if self.log.is_on()
                            && self.booth_lines.allow(peer.addr.ip(), now, &self.log)
                        {
                            self.log.line(format!(
                                "{} is called {}",
                                keys::fingerprint(&peer.key),
                                log::quoted(&peer.name)
                            ));
                        }
                    }
                    // Once per mapped address, so it needs no line limit.
                    if let Some((protocol, mapped)) = proved {
                        log!(
                            self.log,
                            "{} reached this host at {mapped}: the {} mapping works",
                            who(&self.peers[i]),
                            protocol.name()
                        );
                    }
                }
                Some(Message::VoiceLoss(heard)) => {
                    // Over the limit a report is left out: the next one says
                    // the same a second later.
                    if self.peers[i].voice_reports.take(
                        now,
                        VOICE_REPORTS_PER_SECOND,
                        VOICE_REPORT_BURST,
                    ) {
                        self.took_losses(i, &heard, now, socket);
                    }
                }
                Some(Message::Periods(periods)) => self.peers[i].periods = Some(periods),
                Some(Message::OtherHello {
                    protocol,
                    version,
                    name,
                }) => {
                    self.other_version(i, Some((protocol, version)), name, now, socket);
                    return true;
                }
                Some(Message::UnversionedHello { name }) => {
                    self.other_version(i, None, name, now, socket);
                    return true;
                }
                Some(Message::Bye) => {
                    log!(self.log, "{}: left, said bye", who(&self.peers[i]));
                    self.let_go(i, now, socket);
                    return true;
                }
                Some(
                    message @ (Message::ShareStart { .. }
                    | Message::ShareStop
                    | Message::Watch { .. }
                    | Message::Recover { .. }
                    | Message::Idr { .. }
                    | Message::VideoLoss { .. }
                    | Message::Shape(_)),
                ) => {
                    self.on_share_message(i, message, now, socket);
                }
                Some(
                    message @ (Message::ControlAsk { .. }
                    | Message::ControlAsked { .. }
                    | Message::ControlAnswer { .. }
                    | Message::ControlEnd { .. }
                    | Message::ControlPaused { .. }),
                ) => {
                    self.on_control_message(i, message, now, socket);
                }
                _ => {
                    self.drops.bad(now);
                }
            }
        }
        if renamed {
            self.roster_changed(now, socket);
        }
        if greeted {
            self.take_says(i, now, socket);
        }
        true
    }

    fn let_go(&mut self, i: usize, now: Instant, socket: &Socket) {
        let gone = self.peers.remove(i);
        self.talk.forget(&gone.key, now);
        self.keep_left(&gone.sessions, now);
        self.pending.retain(|p| p.key != gone.key);
        if let Some(record) = self.keys.get_mut(&gone.key) {
            record.last_seen = now;
        }
        self.devices.seen(&gone.key, crate::unix_now());
        self.roster_changed(now, socket);
        self.left_share(&gone.key, now, socket);
        self.keep_limits(gone.key, gone.share, gone.control, now);
    }

    // A friend of another protocol cannot stay: nothing after the Hello
    // means the same to both sides. `theirs` is None for a test build from
    // before version numbers. The Bye ends it on their side at once. Such a
    // test build reads nothing of this host's Hello and shows the Bye as the
    // host closing the room, so one line in its own chat, under this host's
    // name, goes ahead of the Bye and says why. A numbered build has already
    // left over this host's Hello, with its own sentence.
    fn other_version(
        &mut self,
        i: usize,
        theirs: Option<(u16, Version)>,
        name: String,
        now: Instant,
        socket: &Socket,
    ) {
        let why = theirs.is_none().then(|| ChatMessage::Said {
            author: *self.identity.public(),
            name: self.name.clone(),
            text: unversioned_friend_line(),
            sent_at_host: None,
            about: false,
        });
        let peer = &mut self.peers[i];
        if let Some(session) = peer.sessions.current.as_mut() {
            if let Some(said) = why {
                let _ = peer.link.queue_chat(&said.encode());
                peer.link
                    .flush_all_twice(socket, session, &[peer.addr], now, &mut peer.traffic);
            }
            peer.link.queue(&Message::Bye);
            peer.link
                .flush_twice(socket, session, &[peer.addr], now, &mut peer.traffic);
        }
        let (key, ip) = (peer.key, peer.addr.ip());
        if self.log.is_on() && self.booth_lines.allow(ip, now, &self.log) {
            let runs = match theirs {
                Some((protocol, version)) => format!("booth {version}, protocol {protocol}"),
                None => {
                    String::from("a test build from before version numbers, told why in its chat")
                }
            };
            self.log.line(format!(
                "{} {}: runs {runs}, this pc booth {}, protocol {}; bye sent",
                keys::fingerprint(&key),
                log::quoted(&name),
                invite::VERSION,
                invite::PROTOCOL
            ));
        }
        if !self.other_versions.contains(&key) {
            self.other_versions.push(key);
            self.history.push(ChatLine {
                author: key,
                text: other_version_line(&name, theirs),
                name,
                at_unix_ms: crate::unix_now_ms(),
                mine: false,
                kind: LineKind::Problem,
            });
        }
        self.let_go(i, now, socket);
    }

    fn roster_changed(&mut self, now: Instant, socket: &Socket) {
        if now >= self.roster_gap_until {
            self.send_roster(now, socket);
        } else {
            self.roster_due = Some(self.roster_gap_until);
        }
    }

    fn send_roster(&mut self, now: Instant, socket: &Socket) {
        let roster = Message::Roster(self.roster());
        for peer in &mut self.peers {
            peer.link.queue(&roster);
            peer.flush(socket, now);
        }
        self.next_roster = now + ROSTER_EVERY;
        self.roster_gap_until = now + ROSTER_GAP;
        self.roster_due = None;
    }

    fn roster(&self) -> Roster {
        let share = |key: &[u8; 32]| {
            let live = self.live.as_ref().filter(|live| live.sharer == *key)?;
            Some(EntryShare {
                number: live.number,
                fps: live.fps,
            })
        };
        let own = Entry {
            key: *self.identity.public(),
            slot: talk::HOST_SLOT,
            name: self.name.clone(),
            rtt_ms: None,
            is_host: true,
            joined_by_invite: false,
            reconnecting: false,
            share: share(self.identity.public()),
            controlling: self.controls(self.identity.public()),
        };
        let clients = self.peers.iter().map(|peer| Entry {
            key: peer.key,
            slot: peer.slot,
            name: peer.name.clone(),
            rtt_ms: numbers::shown_rtt(peer.reconnecting, &peer.link.stats.snapshot())
                .map(|ms| ms.round().clamp(0.0, f32::from(u16::MAX)) as u16),
            is_host: false,
            joined_by_invite: peer.joined_by_invite,
            reconnecting: peer.reconnecting,
            share: share(&peer.key),
            controlling: self.controls(&peer.key),
        });
        Roster {
            room: self.room_name.clone(),
            entries: std::iter::once(own).chain(clients).collect(),
        }
    }

    // True when a line about a booth packet that belongs to no live session
    // may be written: the log is on and the source is within its lines a
    // minute.
    fn log_stray(&mut self, from: SocketAddr, now: Instant) -> bool {
        self.log.is_on() && self.booth_lines.allow(from.ip(), now, &self.log)
    }

    // The same for anything else: a port test, STUN nobody here asked for.
    fn log_other(&mut self, from: SocketAddr, now: Instant) -> bool {
        self.log.is_on() && self.other_lines.allow(from.ip(), now, &self.log)
    }

    fn keep_left(&mut self, sessions: &Sessions, now: Instant) {
        self.just_left.retain(|(_, until)| now < *until);
        // Only a full handshake per entry can add one, so this stays small;
        // the cap is for a program that joins and leaves in a loop.
        if self.just_left.len() >= JUST_LEFT_KEPT {
            self.just_left.remove(0);
        }
        for session in [&sessions.current, &sessions.previous]
            .into_iter()
            .flatten()
        {
            self.just_left
                .push((session.local_index(), now + LEFT_GRACE));
        }
    }

    fn left_just_now(&self, index: u32, now: Instant) -> bool {
        self.just_left
            .iter()
            .any(|(left, until)| *left == index && now < *until)
    }

    fn stray(&mut self, packet: &[u8], from: SocketAddr, now: Instant, what: &str) {
        if self.log_stray(from, now) {
            self.log.line(format!("{}: {what}", heard(packet, from)));
        }
    }
}

fn heard(packet: &[u8], from: SocketAddr) -> String {
    format!("from {from}, {} bytes", packet.len())
}

fn who(peer: &Peer) -> String {
    format!(
        "{} {}",
        keys::fingerprint(&peer.key),
        log::quoted(&peer.name)
    )
}

// Shown under this host's name in the chat of a friend on a test build from
// before version numbers.
fn unversioned_friend_line() -> String {
    let this = invite::VERSION;
    format!(
        "This room runs Booth {this} and you have a test build of Booth made before the first release, so you could not join. Get Booth {this} from {}.",
        invite::RELEASES_PAGE
    )
}

// Whoever has the older build is the one to get the newer.
fn other_version_line(name: &str, theirs: Option<(u16, Version)>) -> String {
    let page = invite::RELEASES_PAGE;
    let this = invite::VERSION;
    let Some((protocol, version)) = theirs else {
        return format!(
            "{name} has a test build of Booth made before the first release, so they could not join. Ask them to get Booth {this} from {page}."
        );
    };
    let (them, us) = invite::two_versions(version, protocol);
    let what = if (version, protocol) > (this, invite::PROTOCOL) {
        format!("Get the same version as {name} from {page}.")
    } else {
        format!("Ask them to get Booth {this} from {page}.")
    };
    format!("{name} has Booth {them} and this room runs {us}, so they could not join. {what}")
}

fn unreadable(err: SessionError) -> String {
    match err {
        SessionError::Malformed => String::from("bad length"),
        SessionError::BadMac1 => String::from("bad mac1, made for another host key"),
        other => format!("noise failure, {other}"),
    }
}

// The strip follows the worst client: one that went quiet, then one with no
// current round trip, then the highest round trip.
fn worst(peers: &[Peer], snapshots: &[LinkSnapshot]) -> Option<usize> {
    let badness = |i: usize| (peers[i].reconnecting, snapshots[i].rtt_ms.is_none());
    let rtt = |i: usize| snapshots[i].rtt_ms.unwrap_or(f32::INFINITY);
    (0..peers.len()).max_by(|&a, &b| {
        badness(a)
            .cmp(&badness(b))
            .then_with(|| rtt(a).total_cmp(&rtt(b)))
    })
}

// The oldest stamp a known device's first initiation this run may carry.
fn stamp_floor() -> Option<Tai64N> {
    SystemTime::now()
        .checked_sub(STAMP_FLOOR)
        .map(Tai64N::from_system_time)
}

fn new_secret() -> Zeroizing<[u8; 32]> {
    let mut secret = Zeroizing::new([0u8; 32]);
    getrandom::fill(secret.as_mut_slice()).expect("the Windows random number generator failed");
    secret
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::known::KnownDevices;
    use crate::mapper::Protocol;
    use crate::testing::{Wire, ping, seal};
    use crate::view::InviteView;
    use channels::{ClockSample, Reliable};
    use invite::{CandidateKind, Invite};
    use session::{COOKIE_REPLY_LEN, Initiation, TimestampSource};
    use std::net::{Ipv4Addr, SocketAddrV4, SocketAddrV6};

    struct Rig {
        host: Host,
        socket: Socket,
        invite: Invite,
    }

    impl Rig {
        // A host named Mara with no room name, holding a multi-use invite.
        fn new(now: Instant) -> Rig {
            Rig::with_log(now, Log::off())
        }

        fn with_log(now: Instant, log: Log) -> Rig {
            Rig::with_timers(now, log, Timers::default())
        }

        fn with_timers(now: Instant, log: Log, timers: Timers) -> Rig {
            let identity = Arc::new(Identity::generate());
            let devices = DeviceBook::new(KnownDevices::default(), true);
            let (mut host, socket) =
                Rig::build(now, log, timers, false, identity, devices, Vec::new());
            host.stun_resolved(now);
            host.new_invite(true, now);
            Rig::holding(host, socket, now)
        }

        // No STUN servers, so STUN settles as soon as it is told so.
        fn host(now: Instant, log: Log, asking_router: bool) -> (Host, Socket) {
            let identity = Arc::new(Identity::generate());
            let devices = DeviceBook::new(KnownDevices::default(), true);
            Rig::build(
                now,
                log,
                Timers::default(),
                asking_router,
                identity,
                devices,
                Vec::new(),
            )
        }

        // A host opened again, with the list an earlier one saved.
        fn reopened(now: Instant, identity: Arc<Identity>, devices: KnownDevices, log: Log) -> Rig {
            let devices = DeviceBook::new(devices, true);
            let timers = Timers::default();
            let (mut host, socket) =
                Rig::build(now, log, timers, false, identity, devices, Vec::new());
            host.stun_resolved(now);
            host.new_invite(true, now);
            Rig::holding(host, socket, now)
        }

        fn build(
            now: Instant,
            log: Log,
            timers: Timers,
            asking_router: bool,
            identity: Arc<Identity>,
            devices: DeviceBook,
            local: Vec<Candidate>,
        ) -> (Host, Socket) {
            let socket = Socket::bind(0, Log::off()).expect("bind the host socket");
            let host = Host::new(HostSetup {
                identity,
                name: "Mara".to_owned(),
                room_name: String::new(),
                timers,
                local: Local::Fixed(local),
                port: socket.local_port(),
                has_ipv6: socket.has_ipv6(),
                router: asking_router.then_some(Gateway {
                    ip: Ipv4Addr::LOCALHOST,
                    local: Ipv4Addr::LOCALHOST,
                }),
                punch_loopback: true,
                address_name: None,
                lookup: Lookup::default(),
                devices,
                list_problem: None,
                initiations_read: Arc::default(),
                voice: crate::testing::quiet_voice(true),
                speaker: crate::testing::no_speaker(),
                screen: crate::testing::quiet_screen(),
                now,
                log,
            });
            (host, socket)
        }

        // The rig around a host that already shows an invite.
        fn holding(host: Host, socket: Socket, now: Instant) -> Rig {
            let code = host.view(now, 0).invite.expect("an invite").code;
            let invite = Invite::decode(&code).expect("the code decodes");
            Rig {
                host,
                socket,
                invite,
            }
        }

        fn deliver(&mut self, packet: &[u8], from: SocketAddr, now: Instant) {
            self.host.on_packet(packet, from, now, &self.socket);
        }

        fn tick(&mut self, now: Instant) {
            self.host.on_timer(now, &self.socket);
        }
    }

    #[derive(Debug)]
    enum Took {
        Control(Message),
        Chat(ChatMessage),
    }

    // A client played by hand.
    struct Guest {
        identity: Identity,
        wire: Wire,
        stamps: TimestampSource,
        session: Option<Session>,
        reliable: Reliable,
        chat: Reliable,
    }

    impl Guest {
        fn new() -> Guest {
            Guest {
                identity: Identity::generate(),
                wire: Wire::new(),
                stamps: TimestampSource::new(),
                session: None,
                reliable: Reliable::new(),
                chat: Reliable::new(),
            }
        }

        fn invite_psk(&self, invite: &Invite) -> Zeroizing<[u8; 32]> {
            session::invite_psk(&invite.secret, &invite.host_key, self.identity.public())
        }

        fn initiate(
            &mut self,
            rig: &mut Rig,
            kind: InitKind,
            psk: &[u8; 32],
            from: SocketAddr,
            now: Instant,
        ) -> Initiation {
            self.initiate_with(rig, kind, psk, from, None, now)
        }

        fn initiate_with(
            &mut self,
            rig: &mut Rig,
            kind: InitKind,
            psk: &[u8; 32],
            from: SocketAddr,
            cookie: Option<&[u8; 16]>,
            now: Instant,
        ) -> Initiation {
            let (initiation, packet) = Initiation::start_with_cookie(
                &self.identity.private_bytes(),
                self.identity.public(),
                &rig.invite.host_key,
                psk,
                kind,
                self.stamps.next_stamp(),
                peer::random_index(|_| false),
                cookie,
            )
            .expect("start an initiation");
            rig.deliver(&packet, from, now);
            initiation
        }

        fn knock(&mut self, rig: &mut Rig, now: Instant) -> Initiation {
            self.knock_with(rig, None, now)
        }

        fn knock_with(
            &mut self,
            rig: &mut Rig,
            cookie: Option<&[u8; 16]>,
            now: Instant,
        ) -> Initiation {
            let psk = self.invite_psk(&rig.invite);
            let kind = InitKind::Invite(rig.invite.invite_id);
            self.initiate_with(rig, kind, &psk, self.wire.addr(), cookie, now)
        }

        // The session from the host's answer, if it answered.
        fn answer(&mut self, mut initiation: Initiation, now: Instant) -> Option<Session> {
            self.wire
                .packets()
                .iter()
                .find_map(|packet| initiation.finish(packet, now).ok())
        }

        fn join(&mut self, rig: &mut Rig, name: &str, now: Instant) {
            self.join_saying(rig, name, None, now);
        }

        fn join_saying(
            &mut self,
            rig: &mut Rig,
            name: &str,
            reached: Option<SocketAddr>,
            now: Instant,
        ) {
            let initiation = self.knock(rig, now);
            self.session = Some(self.answer(initiation, now).expect("the host answers"));
            let hello = Message::Hello {
                version: invite::VERSION,
                name: name.to_owned(),
                reached,
            };
            self.say(rig, &hello, now);
        }

        fn say(&mut self, rig: &mut Rig, message: &Message, now: Instant) {
            self.reliable.send(&message.encode()).expect("queued");
            while let Some(frame) = self.reliable.poll_transmit(now, None) {
                let session = self.session.as_mut().expect("joined");
                let packet = seal(session, Channel::Control, &frame);
                rig.deliver(&packet, self.wire.addr(), now);
            }
        }

        // Any bytes on the chat stream, as a friend's program could send.
        fn chat(&mut self, rig: &mut Rig, bytes: &[u8], now: Instant) {
            self.chat.send(bytes).expect("queued");
            self.send_chat(rig, now);
        }

        // What the chat stream has for the host: lines the window lets out,
        // and acks for what was read.
        fn send_chat(&mut self, rig: &mut Rig, now: Instant) {
            while let Some(frame) = self.chat.poll_transmit(now, None) {
                let session = self.session.as_mut().expect("joined");
                let packet = seal(session, Channel::Chat, &frame);
                rig.deliver(&packet, self.wire.addr(), now);
            }
        }

        // Everything the host hands on, however much: what it holds back
        // for want of window follows each ack.
        fn said_all(&mut self, rig: &mut Rig, now: Instant) -> Vec<ChatMessage> {
            let mut out = Vec::new();
            loop {
                let got = self.said(now);
                if got.is_empty() {
                    return out;
                }
                out.extend(got);
                self.send_chat(rig, now);
            }
        }

        // What arrived since the last look, into the two streams.
        fn read(&mut self, now: Instant) {
            for packet in self.wire.packets() {
                self.take(&packet, now);
            }
        }

        fn take(&mut self, packet: &[u8], now: Instant) {
            let mut plain = Vec::new();
            let Some(session) = self.session.as_mut() else {
                return;
            };
            if session.decrypt(packet, &mut plain).is_err() {
                return;
            }
            match peer::read_plain(&plain) {
                Some(Plain::Control(frame)) => {
                    let _ = self.reliable.receive(frame, now);
                }
                Some(Plain::Chat(frame)) => {
                    let _ = self.chat.receive(frame, now);
                }
                _ => {}
            }
        }

        // The control messages the host sent since the last look.
        fn heard(&mut self, now: Instant) -> Vec<Message> {
            self.read(now);
            let mut out = Vec::new();
            while let Some(bytes) = self.reliable.next_delivered() {
                out.extend(Message::decode(&bytes));
            }
            out
        }

        // Both streams since the last look, in the order a build takes them.
        fn took(&mut self, now: Instant) -> Vec<Took> {
            let mut out = Vec::new();
            for packet in self.wire.packets() {
                self.take(&packet, now);
                while let Some(bytes) = self.reliable.next_delivered() {
                    out.extend(Message::decode(&bytes).map(Took::Control));
                }
                while let Some(bytes) = self.chat.next_delivered() {
                    out.extend(ChatMessage::decode(&bytes).map(Took::Chat));
                }
            }
            out
        }

        // The chat the host handed on since the last look.
        fn said(&mut self, now: Instant) -> Vec<ChatMessage> {
            self.read(now);
            let mut out = Vec::new();
            while let Some(bytes) = self.chat.next_delivered() {
                out.extend(ChatMessage::decode(&bytes));
            }
            out
        }

        // Back from a closed room: no session, and reliable streams that
        // start over with the next one.
        fn left(&mut self) {
            self.session = None;
            self.reliable = Reliable::new();
            self.chat = Reliable::new();
            self.wire.packets();
        }
    }

    fn addresses_in(messages: &[Message]) -> Vec<(Vec<Candidate>, Option<String>)> {
        messages
            .iter()
            .filter_map(|message| match message {
                Message::HostAddresses {
                    candidates,
                    address_name,
                } => Some((candidates.clone(), address_name.clone())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn device_rejoins_after_restart() {
        let start = Instant::now();
        let mut rig = Rig::new(start);
        let mut ana = Guest::new();
        ana.join(&mut rig, "Ana", start);
        let save = rig.host.take_last_save().expect("the join is saved");
        let saved = known::parse_devices(&save.bytes).expect("what was saved reads back");
        assert_eq!(saved.devices.len(), 1);
        let kept = saved.devices[0].clone();
        assert_eq!(kept.key, *ana.identity.public());
        assert_eq!(kept.name, "Ana");
        let secret = rig.host.keys[ana.identity.public()]
            .secret
            .clone()
            .expect("a secret");
        assert_eq!(*kept.secret, *secret);
        assert!(rig.host.take_last_save().is_none(), "nothing changed since");

        // The host closes, and a new room opens from what it saved. Ana
        // comes back with the secret and no invite.
        let identity = Arc::clone(&rig.host.identity);
        let later = start + Duration::from_secs(60);
        rig.host.leave(later, &rig.socket);
        let mut again = Rig::reopened(later, identity, saved, Log::off());
        ana.left();
        let from = ana.wire.addr();
        let rejoin = ana.initiate(&mut again, InitKind::Known, &secret, from, later);
        let session = ana.answer(rejoin, later);
        ana.session = Some(session.expect("a known device is answered after a restart"));
        let hello = Message::Hello {
            version: invite::VERSION,
            name: "Ana".to_owned(),
            reached: None,
        };
        ana.say(&mut again, &hello, later);
        assert_eq!(again.host.peers.len(), 1);
        let view = again.host.view(later, 0);
        assert!(!view.people[1].joined_by_invite, "no fingerprint this time");
        let heard = ana.heard(later);
        assert!(
            heard
                .iter()
                .any(|m| matches!(m, Message::PeerSecret { secret: s } if **s == *secret)),
            "{heard:?}"
        );
        let seen = known::parse_devices(&again.host.take_last_save().expect("seen again").bytes)
            .expect("reads back");
        assert!(seen.devices[0].last_seen >= kept.last_seen);
    }

    #[test]
    fn a_blocked_key_gets_no_answer() {
        let start = Instant::now();
        let mut ana = Guest::new();
        let devices = KnownDevices {
            devices: Vec::new(),
            blocked: vec![known::BlockedKey {
                key: *ana.identity.public(),
                since: 1_790_000_000,
            }],
        };
        let (log, captured) = Log::capture(64);
        let mut rig = Rig::reopened(start, Arc::new(Identity::generate()), devices, log);
        let bad = rig.host.drops.bad;
        let knock = ana.knock(&mut rig, start);
        assert!(
            ana.answer(knock, start).is_none(),
            "a blocked key was answered"
        );
        assert_eq!(rig.host.drops.bad, bad + 1);
        assert!(rig.host.pending.is_empty());
        let lines = captured.lines();
        assert!(
            lines
                .iter()
                .any(|line| line.ends_with("dropped: the key is blocked")),
            "{lines:?}"
        );

        // Nor does a code sent back, even one answering a live invite the
        // blocked person saw, and no punch goes toward them.
        for answers in [Answers::Invite(rig.invite.invite_id), Answers::Rejoin] {
            let code = code_for(&ana, answers, Some(ana.wire.addr()));
            assert_eq!(
                rig.host.accept_reply(&code, start),
                Err(ReplyRefused::Blocked)
            );
        }
        rig.tick(start);
        assert_eq!(punches(&ana.wire), 0);
    }

    #[test]
    fn known_device_may_send_rejoin_code() {
        let start = Instant::now();
        let ana = Guest::new();
        let devices = KnownDevices {
            devices: vec![known::KnownDevice {
                key: *ana.identity.public(),
                name: String::from("Ana"),
                first_seen: 1_790_000_000,
                last_seen: 1_790_000_000,
                secret: Zeroizing::new([4; 32]),
            }],
            blocked: Vec::new(),
        };
        let mut rig = Rig::reopened(start, Arc::new(Identity::generate()), devices, Log::off());
        let code = code_for(&ana, Answers::Rejoin, Some(SocketAddr::V4(OUTSIDE)));
        let accepted = rig.host.accept_reply(&code, start).expect("accepted");
        assert_eq!(accepted.to, [SocketAddr::V4(OUTSIDE)]);
        // A stranger's key is not known, whatever the code says.
        let bo = Guest::new();
        let code = code_for(&bo, Answers::Rejoin, Some(SocketAddr::V4(OUTSIDE)));
        assert_eq!(
            rig.host.accept_reply(&code, start),
            Err(ReplyRefused::NotKnown)
        );
    }

    #[test]
    fn friends_told_host_addresses() {
        let start = Instant::now();
        let lan = Candidate {
            kind: CandidateKind::Lan,
            addr: "192.0.2.10:41000".parse().unwrap(),
        };
        let devices = DeviceBook::new(KnownDevices::default(), true);
        let identity = Arc::new(Identity::generate());
        let (mut host, socket) = Rig::build(
            start,
            Log::off(),
            Timers::default(),
            false,
            identity,
            devices,
            vec![lan],
        );
        let name = AddressName::new(String::from("myroom.duckdns.org"), Lookup::default());
        host.address_name = Some(name);
        let server = Wire::new();
        host.stun_found(vec![server.addr()], &socket);
        host.stun_resolved(start);
        answer_stun(&mut host, &socket, &server, OUTSIDE, start);
        let mut rig = Rig::holding(host, socket, start);
        let mut ana = Guest::new();
        ana.join(&mut rig, "Ana", start);
        let heard = ana.heard(start);
        assert!(
            matches!(
                heard[..],
                [
                    Message::Hello { reached: None, .. },
                    Message::PeerSecret { .. },
                    ..
                ]
            ),
            "the host's Hello, then the secret: {heard:?}"
        );
        let port = rig.host.port;
        let public = |addr: SocketAddrV4| Candidate {
            kind: CandidateKind::Public,
            addr: SocketAddr::V4(addr),
        };
        let bound = |addr: SocketAddrV4| SocketAddrV4::new(*addr.ip(), port);
        let name = Some(String::from("myroom.duckdns.org"));
        assert_eq!(
            addresses_in(&heard),
            [(
                vec![lan, public(OUTSIDE), public(bound(OUTSIDE))],
                name.clone()
            )]
        );

        // Nothing new, nothing sent.
        let later = start + Duration::from_millis(100);
        rig.tick(later);
        assert!(addresses_in(&ana.heard(later)).is_empty());

        rig.host.address_changed(later, &rig.socket, None);
        answer_stun(&mut rig.host, &rig.socket, &server, MOVED, later);
        assert_eq!(
            addresses_in(&ana.heard(later)),
            [(vec![lan, public(MOVED), public(bound(MOVED))], name)]
        );
    }

    #[test]
    fn repeated_hellos_do_not_multiply_rosters() {
        let start = Instant::now();
        let mut rig = Rig::new(start);
        let mut ana = Guest::new();
        let bo = {
            let mut bo = Guest::new();
            ana.join(&mut rig, "Ana", start);
            bo.join(&mut rig, "Bo", start);
            bo
        };
        let mut now = start + Duration::from_secs(1);
        rig.tick(now);
        bo.wire.packets();

        for i in 0..60 {
            now += Duration::from_millis(10);
            let name = if i % 2 == 0 { "Ana" } else { "Anna" };
            let hello = Message::Hello {
                version: invite::VERSION,
                name: name.to_owned(),
                reached: None,
            };
            ana.say(&mut rig, &hello, now);
            rig.tick(now);
        }
        now += ROSTER_GAP;
        rig.tick(now);

        // Sixty renames in 600 ms. What reaches Bo is a few rosters and
        // their retransmits, not one roster per packet Ana sent.
        let reached_bo = bo.wire.packets().len();
        assert!(reached_bo <= 15, "{reached_bo} packets reached Bo");
        assert_eq!(rig.host.peers[0].name, "Anna");
        assert_eq!(rig.host.roster().entries[1].name, "Anna");
    }

    #[test]
    fn the_last_seat_goes_to_the_first_to_confirm() {
        let start = Instant::now();
        let mut rig = Rig::new(start);
        for i in 0..MAX_CLIENTS - 1 {
            Guest::new().join(&mut rig, &format!("Friend {i}"), start);
        }
        assert_eq!(rig.host.peers.len(), MAX_CLIENTS - 1);

        // Answers to an invite hold no seat: one with a wrong secret would
        // hold it as well as one with the right one.
        let mut stranger = Guest::new();
        let kind = InitKind::Invite(rig.invite.invite_id);
        let from = stranger.wire.addr();
        stranger.initiate(&mut rig, kind, &[0x5a; 32], from, start);
        let mut first = Guest::new();
        let mut second = Guest::new();
        let a = first.knock(&mut rig, start);
        let b = second.knock(&mut rig, start);
        first.session = Some(first.answer(a, start).expect("first answered"));
        second.session = Some(second.answer(b, start).expect("second answered"));
        let hello = |name: &str| Message::Hello {
            version: invite::VERSION,
            name: name.to_owned(),
            reached: None,
        };
        first.say(&mut rig, &hello("First"), start);
        second.say(&mut rig, &hello("Second"), start);
        assert_eq!(rig.host.peers.len(), MAX_CLIENTS);
        assert!(rig.host.is_peer(first.identity.public()));
        assert!(!rig.host.is_peer(second.identity.public()));
    }

    #[test]
    fn key_flood_keeps_the_rekey() {
        let start = Instant::now();
        let mut rig = Rig::new(start);
        let mut ana = Guest::new();
        ana.join(&mut rig, "Ana", start);
        let secret = rig.host.keys[ana.identity.public()]
            .secret
            .clone()
            .expect("the host made Ana a secret");

        let mut now = start + Duration::from_secs(1);
        let from = ana.wire.addr();
        let rekey = ana.initiate(&mut rig, InitKind::Rekey, &secret, from, now);
        let rekeyed = ana.answer(rekey, now).expect("the rekey is answered");

        // Someone holding the invite fills every seat left, as many tries per
        // key as the host keeps, each from its own address so no one source
        // runs out of tokens. 25 a second keeps the host from going under
        // load, where every one of these would get a cookie reply instead.
        let mut strangers: Vec<Guest> = (0..MAX_CLIENTS - 1).map(|_| Guest::new()).collect();
        let mut source = 0u8;
        for _ in 0..PENDING_PER_KEY {
            for stranger in &mut strangers {
                now += Duration::from_millis(40);
                source += 1;
                let from = SocketAddr::from(([127, 0, 1, source], 9));
                let psk = stranger.invite_psk(&rig.invite);
                let kind = InitKind::Invite(rig.invite.invite_id);
                stranger.initiate(&mut rig, kind, &psk, from, now);
            }
        }
        assert_eq!(rig.host.pending.len(), PENDING_TOTAL);

        ana.session = Some(rekeyed);
        let first = ping(ana.session.as_mut().expect("rekeyed"), 0);
        rig.deliver(&first, from, now);
        assert_eq!(rig.host.peers[0].rekeys, 1, "the rekey was pushed out");
    }

    fn load_initiations() -> u32 {
        Timers::default().load_initiations
    }

    fn reads(rig: &Rig) -> u64 {
        rig.host.initiations_read.load(Ordering::Relaxed)
    }

    // An initiation from someone with the host key and no invite. Under load
    // the host looks no further than mac2, so one packet sent again and
    // again stands for a flood.
    fn stranger_packet(rig: &Rig) -> Vec<u8> {
        let stranger = Identity::generate();
        Initiation::start(
            &stranger.private_bytes(),
            stranger.public(),
            &rig.invite.host_key,
            &[0; 32],
            InitKind::Known,
            Tai64N::now(),
            1,
        )
        .expect("start an initiation")
        .1
    }

    // Enough initiations to put the host under load, a millisecond apart,
    // each from an address of its own. Returns when the last came in.
    fn flood(rig: &mut Rig, now: Instant) -> Instant {
        let packet = stranger_packet(rig);
        let mut at = now;
        for n in 0..load_initiations() {
            at = now + Duration::from_millis(u64::from(n));
            let from = SocketAddr::from(([127, 0, 3, 1 + n as u8], 9));
            rig.deliver(&packet, from, at);
        }
        at
    }

    fn cookie_replies(packets: &[Vec<u8>]) -> usize {
        packets
            .iter()
            .filter(|packet| session::packet_type(packet) == Some(PacketType::CookieReply))
            .count()
    }

    #[test]
    fn no_cookie_reply_without_load() {
        let start = Instant::now();
        let mut rig = Rig::new(start);
        let packet = stranger_packet(&rig);
        // One under the threshold every second for three seconds.
        let per_second = load_initiations() - 1;
        let mut sent = 0u8;
        for second in 0..3 {
            for k in 0..u64::from(per_second) {
                sent += 1;
                let at = start + Duration::from_secs(second) + Duration::from_millis(k * 32);
                rig.deliver(&packet, SocketAddr::from(([127, 0, 4, sent], 9)), at);
            }
        }
        // A friend joining makes one more within the last second, and that
        // is still one short.
        let now = start + Duration::from_secs(3);
        let mut ana = Guest::new();
        let mut tried = ana.knock(&mut rig, now);
        let packets = ana.wire.packets();
        assert_eq!(cookie_replies(&packets), 0);
        assert!(
            packets
                .iter()
                .any(|packet| tried.finish(packet, now).is_ok())
        );
        assert_eq!(reads(&rig), u64::from(sent) + 1);
        assert_eq!(rig.host.cookies.replies_sent, 0);
        assert_eq!(rig.host.drops.no_cookie, 0);
    }

    #[test]
    fn under_load_no_mac2_gets_cookie() {
        let start = Instant::now();
        let (log, captured) = Log::capture(1024);
        let mut rig = Rig::with_log(start, log);
        let now = flood(&mut rig, start);
        // Every one before the threshold was read. The one that reached it
        // was not.
        let read = reads(&rig);
        assert_eq!(read, u64::from(load_initiations() - 1));

        let mut ana = Guest::new();
        let first = ana.knock(&mut rig, now);
        assert_eq!(reads(&rig), read);
        let packets = ana.wire.packets();
        assert_eq!(packets.len(), 1, "a cookie reply and nothing else");
        assert_eq!(packets[0].len(), COOKIE_REPLY_LEN);
        let (index, cookie) =
            session::read_cookie_reply(&packets[0], &rig.invite.host_key, &first.mac1())
                .expect("the reply opens with the try's mac1");
        assert_eq!(index, first.sender_index());

        let second = ana.knock_with(&mut rig, Some(&cookie), now);
        assert_eq!(reads(&rig), read + 1);
        assert!(
            ana.answer(second, now).is_some(),
            "with the cookie it is answered"
        );

        // The initiation that reached the threshold got one too.
        let numbers = rig.host.view(now, 0).numbers;
        assert_eq!(numbers.cookie_replies, 2);
        assert_eq!(numbers.dropped_no_cookie, 2);
        let lines = captured.lines();
        assert!(
            lines
                .iter()
                .any(|line| line.starts_with("under load: 32 initiations within a second")),
            "{lines:?}"
        );
        let heard = format!("from {}, ", ana.wire.addr());
        assert!(
            lines
                .iter()
                .any(|line| line.starts_with(&heard) && line.contains("cookie reply sent")),
            "{lines:?}"
        );
    }

    // Windows sends nothing to 0.0.0.0, which stands in for a send that
    // fails when a flood has used up the socket's buffers.
    #[test]
    fn unsent_cookie_reply_not_logged_sent() {
        let start = Instant::now();
        let (log, captured) = Log::capture(1024);
        let mut rig = Rig::with_log(start, log);
        let now = flood(&mut rig, start);
        let sent = rig.host.cookies.replies_sent;
        let dropped = rig.host.drops.no_cookie;

        let from = SocketAddr::from((Ipv4Addr::UNSPECIFIED, 9));
        rig.deliver(&stranger_packet(&rig), from, now);
        assert_eq!(rig.host.cookies.replies_sent, sent);
        assert_eq!(rig.host.drops.no_cookie, dropped + 1);
        let heard = format!("from {from}, ");
        let lines: Vec<String> = captured
            .lines()
            .into_iter()
            .filter(|line| line.starts_with(&heard))
            .collect();
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(
            lines[0].contains("dropped: could not send the cookie reply: "),
            "{lines:?}"
        );
    }

    #[test]
    fn one_source_under_load() {
        let start = Instant::now();
        let (log, captured) = Log::capture(4096);
        let mut rig = Rig::with_log(start, log);
        let now = flood(&mut rig, start);
        let mut ana = Guest::new();
        let heard = format!("from {}, ", ana.wire.addr());
        let cookie_lines = |lines: Vec<String>| {
            lines
                .iter()
                .filter(|line| line.starts_with(&heard) && line.contains("cookie reply sent"))
                .count()
        };

        let tries: Vec<Initiation> = (0..30).map(|_| ana.knock(&mut rig, now)).collect();
        let replies = ana.wire.packets();
        assert_eq!(cookie_replies(&replies), 20, "one source's burst");
        assert_eq!(replies.len(), 20);
        assert_eq!(rig.host.drops.no_cookie, 31);
        assert_eq!(cookie_lines(captured.lines()), 1);

        // Sending the cookie back gets key math again, at one source's rate.
        let (_, cookie) =
            session::read_cookie_reply(&replies[0], &rig.invite.host_key, &tries[0].mac1())
                .expect("the first reply answers the first try");
        let later = now + Duration::from_secs(1);
        let read = reads(&rig);
        for _ in 0..30 {
            ana.knock_with(&mut rig, Some(&cookie), later);
        }
        assert_eq!(reads(&rig), read + 10);

        // A minute on, under load again, the next one has a line of its own.
        let again = flood(&mut rig, now + Duration::from_secs(60));
        ana.knock(&mut rig, again);
        assert_eq!(cookie_lines(captured.lines()), 1);
    }

    #[test]
    fn cookie_replies_spare_key_math() {
        let start = Instant::now();
        let mut rig = Rig::new(start);
        let now = flood(&mut rig, start);
        // More sources than the shared budget has tokens, all at once. Each
        // is a socket, since Windows stops saying port unreachable for a
        // while after a burst of them, and the mapper's tests running
        // alongside wait for one.
        let packet = stranger_packet(&rig);
        let sources: Vec<Wire> = (1..=60)
            .map(|n| Wire::at(Ipv4Addr::new(127, 0, 5, n)))
            .collect();
        for source in &sources {
            rig.deliver(&packet, source.addr(), now);
        }
        assert_eq!(rig.host.cookies.replies_sent, 61);

        let mut ana = Guest::new();
        let first = ana.knock(&mut rig, now);
        let packets = ana.wire.packets();
        let (_, cookie) =
            session::read_cookie_reply(&packets[0], &rig.invite.host_key, &first.mac1())
                .expect("a cookie reply");
        let second = ana.knock_with(&mut rig, Some(&cookie), now);
        assert!(ana.answer(second, now).is_some());
    }

    // The counts in the lines that sum up what the cap held back.
    fn capped_counts(lines: Vec<String>) -> Vec<u64> {
        lines
            .iter()
            .filter_map(|line| {
                line.strip_prefix("cookie replies at their cap, ")?
                    .strip_suffix(" dropped in the last second")?
                    .parse()
                    .ok()
            })
            .collect()
    }

    // A flood from many addresses, each within its own rate, together twice
    // what the cap lets out.
    #[test]
    fn cookie_replies_keep_to_cap() {
        let start = Instant::now();
        let (log, captured) = Log::capture(4096);
        let timers = Timers {
            cookie_replies_per_second: 100,
            cookie_reply_burst: 20,
            ..Timers::default()
        };
        let cap = u64::from(timers.cookie_replies_per_second);
        let mut rig = Rig::with_timers(start, log, timers);
        let now = flood(&mut rig, start);
        let read = reads(&rig);
        let packet = stranger_packet(&rig);
        // Each a socket, as in the test above.
        let sources: Vec<Wire> = (1..=40)
            .map(|n| Wire::at(Ipv4Addr::new(127, 0, 6, n)))
            .collect();
        let sent = |rig: &Rig| rig.host.cookies.replies_sent;

        // All at once, the burst goes out, less the one the last initiation
        // of the flood took.
        let before = sent(&rig);
        for source in &sources {
            rig.deliver(&packet, source.addr(), now);
        }
        let burst = u64::from(timers.cookie_reply_burst);
        assert_eq!(sent(&rig) - before, burst - 1);
        let lines = captured.lines();
        let heard = format!("from {}, ", sources[39].addr());
        assert!(
            lines.iter().any(|line| line.starts_with(&heard)
                && line.contains("dropped: cookie replies at their cap, no cookie reply")),
            "{lines:?}"
        );
        let mut counts = capped_counts(lines);

        // Then one every 5 ms, 5 a second from each source, well within its
        // own rate, for three seconds, with the timer thread's ticks between.
        let mut at = now;
        for second in 0..3 {
            let before = sent(&rig);
            for n in 0..200 {
                at += Duration::from_millis(5);
                rig.deliver(&packet, sources[n % sources.len()].addr(), at);
                rig.tick(at);
            }
            let replies = sent(&rig) - before;
            assert!(
                replies <= cap && replies + 1 >= cap,
                "{replies} cookie replies in second {second}, the cap is {cap} a second"
            );
            counts.extend(capped_counts(captured.lines()));
            assert!(counts.len() <= second + 1, "{counts:?} by second {second}");
        }
        let last = at + Duration::from_secs(1);
        rig.tick(last);
        counts.extend(capped_counts(captured.lines()));

        // Every one is counted with the rest dropped for want of mac2, and
        // every one the cap held back is in a summary line, one a second.
        let initiations = 1 + sources.len() as u64 + 600;
        let numbers = rig.host.view(last, 0).numbers;
        assert_eq!(numbers.dropped_no_cookie, initiations);
        assert_eq!(numbers.cookie_replies, sent(&rig));
        assert_eq!(counts.iter().sum::<u64>(), initiations - sent(&rig));
        assert!(counts.len() <= 4, "{counts:?}");

        // What was sent reached the sources, all but the flood's one.
        let reached: usize = sources
            .iter()
            .map(|source| cookie_replies(&source.packets()))
            .sum();
        assert_eq!(reached as u64, sent(&rig) - 1);
        assert_eq!(reads(&rig), read, "no key math for any of them");
    }

    #[test]
    fn load_ends_after_calm() {
        let start = Instant::now();
        let (log, captured) = Log::capture(1024);
        let mut rig = Rig::with_log(start, log);
        flood(&mut rig, start);
        // They all came in within 31 ms of the first, so the count drops
        // under the threshold a second after the first.
        let over = start + Duration::from_secs(1) + Timers::default().load_calm;
        assert_eq!(rig.host.cookies.next_deadline(), Some(over));
        assert!(rig.host.next_deadline().is_some_and(|at| at <= over));

        let mut ana = Guest::new();
        ana.knock(&mut rig, over - Duration::from_millis(1));
        assert_eq!(cookie_replies(&ana.wire.packets()), 1);

        rig.tick(over);
        let lines = captured.lines();
        assert!(
            lines.iter().any(|line| line
                == "no longer under load: under 32 initiations a second for 5.0 s; 2 cookie replies sent and 2 initiations dropped for want of mac2 so far"),
            "{lines:?}"
        );
        assert_eq!(rig.host.cookies.next_deadline(), None);
        let tried = ana.knock(&mut rig, over);
        assert!(
            ana.answer(tried, over).is_some(),
            "answered as before the load"
        );
    }

    #[test]
    fn full_key_table_makes_way() {
        let start = Instant::now();
        let mut rig = Rig::new(start);
        let ana = {
            let mut ana = Guest::new();
            ana.join(&mut rig, "Ana", start);
            ana
        };
        for n in 1..MAX_KEYS as u64 {
            let mut key = [7u8; 32];
            key[..8].copy_from_slice(&n.to_le_bytes());
            let mut record = KeyRecord::new(start + Duration::from_millis(n));
            record.secret = Some(new_secret());
            rig.host.keys.insert(key, record);
        }
        assert_eq!(rig.host.keys.len(), MAX_KEYS);

        // Every other record holds a secret, and Ana, seen longest ago, is
        // in the room. The oldest of the rest makes way.
        let later = start + Duration::from_secs(5);
        Guest::new().join(&mut rig, "Bo", later);
        assert_eq!(rig.host.peers.len(), 2);
        assert_eq!(rig.host.keys.len(), MAX_KEYS);
        assert!(rig.host.keys.contains_key(ana.identity.public()));
        let mut oldest = [7u8; 32];
        oldest[..8].copy_from_slice(&1u64.to_le_bytes());
        assert!(!rig.host.keys.contains_key(&oldest));
    }

    #[test]
    fn room_name_defaults_to_the_host_name() {
        let start = Instant::now();
        let rig = Rig::new(start);
        assert_eq!(rig.host.view(start, 0).room_name, "Mara");
        assert_eq!(rig.host.roster().room, "Mara");
    }

    #[test]
    fn a_failed_socket_stops_the_room() {
        let start = Instant::now();
        let mut rig = Rig::new(start);
        Guest::new().join(&mut rig, "Ana", start);
        rig.host.socket_failed();
        let view = rig.host.view(start, 0);
        assert_eq!(view.notice, Some(Notice::SocketFailed));
        assert!(view.invite.is_none());
        assert_eq!(view.people.len(), 1);
        assert_eq!(view.strip.state, LinkState::Alone);
        assert_eq!(rig.host.next_deadline(), None);
        assert!(
            !rig.host
                .on_timer(start + Duration::from_secs(1), &rig.socket)
        );
    }

    #[test]
    fn second_bye_copy_is_quiet() {
        let start = Instant::now();
        let (log, captured) = Log::capture(256);
        let mut rig = Rig::with_log(start, log);
        let mut ana = Guest::new();
        ana.join(&mut rig, "Ana", start);
        let from = ana.wire.addr();

        // What flush_twice sends on leaving: every frame twice.
        ana.reliable.send(&Message::Bye.encode()).expect("queued");
        let frame = ana.reliable.poll_transmit(start, None).expect("a frame");
        let session = ana.session.as_mut().expect("joined");
        let copies: Vec<Vec<u8>> = (0..3)
            .map(|_| seal(session, Channel::Control, &frame))
            .collect();
        let bad = rig.host.drops.bad;
        rig.deliver(&copies[0], from, start);
        assert!(rig.host.peers.is_empty());
        rig.deliver(&copies[1], from, start);
        assert_eq!(rig.host.drops.bad, bad);
        let lines = captured.lines();
        assert!(
            lines.iter().any(|line| line.ends_with(": left, said bye")),
            "{lines:?}"
        );
        assert!(
            !lines.iter().any(|line| line.contains("unknown session")),
            "{lines:?}"
        );

        // Long after, the same index is anyone's guess again.
        rig.deliver(&copies[2], from, start + LEFT_GRACE);
        assert_eq!(rig.host.drops.bad, bad + 1);
        let lines = captured.lines();
        assert!(
            lines
                .iter()
                .any(|line| line.ends_with("data for an unknown session, dropped")),
            "{lines:?}"
        );
    }

    fn texts(lines: &[Arc<ChatLine>]) -> Vec<(&str, &str, bool)> {
        lines
            .iter()
            .map(|line| (line.name.as_str(), line.text.as_str(), line.mine))
            .collect()
    }

    fn said_texts(messages: &[ChatMessage]) -> Vec<(String, String)> {
        messages
            .iter()
            .filter_map(|message| match message {
                ChatMessage::Said { name, text, .. } => Some((name.clone(), text.clone())),
                ChatMessage::Say { .. } => None,
            })
            .collect()
    }

    fn say(text: &str) -> Vec<u8> {
        say_at(text, 1_790_284_323_456_789)
    }

    fn say_at(text: &str, sent_at: u64) -> Vec<u8> {
        ChatMessage::Say {
            text: text.to_owned(),
            sent_at,
        }
        .encode()
    }

    #[test]
    fn chat_goes_out_in_host_order() {
        let start = Instant::now();
        let mut rig = Rig::new(start);
        let mut ana = Guest::new();
        ana.join(&mut rig, "Ana", start);
        let mut bo = Guest::new();
        bo.join(&mut rig, "Bo", start);

        rig.host
            .say(String::from("one"), start, &rig.socket)
            .unwrap();
        ana.chat(&mut rig, &say("two"), start);
        rig.host
            .say(String::from("three"), start, &rig.socket)
            .unwrap();

        let heard = |who: &str, text: &str| (who.to_owned(), text.to_owned());
        assert_eq!(
            said_texts(&bo.said(start)),
            [
                heard("Mara", "one"),
                heard("Ana", "two"),
                heard("Mara", "three")
            ]
        );
        // Nobody is handed their own line back.
        assert_eq!(
            said_texts(&ana.said(start)),
            [heard("Mara", "one"), heard("Mara", "three")]
        );
        let view = rig.host.view(start, 0);
        assert_eq!(
            texts(&view.chat),
            [
                ("Mara", "one", true),
                ("Ana", "two", false),
                ("Mara", "three", true)
            ]
        );
        assert_eq!(view.chat[1].author, *ana.identity.public());
    }

    // A friend's PC is hostile input: bytes that are not text, text over the
    // limit, text of nothing but control characters, too many lines, and a
    // message only a host sends are all dropped here, logged without their
    // text, and handed to nobody.
    #[test]
    fn bad_say_goes_no_further() {
        let start = Instant::now();
        let (log, captured) = Log::capture(256);
        let mut rig = Rig::with_log(start, log);
        let mut ana = Guest::new();
        ana.join(&mut rig, "Ana", start);
        let mut bo = Guest::new();
        bo.join(&mut rig, "Bo", start);
        captured.lines();

        let raw = |text: &[u8]| {
            let mut out = vec![1];
            out.extend_from_slice(&(text.len() as u16).to_le_bytes());
            out.extend_from_slice(text);
            out.extend_from_slice(&7u64.to_le_bytes());
            out
        };
        let lines = ["x"; 21].join("\n");
        let from_a_host = ChatMessage::Said {
            author: *ana.identity.public(),
            name: String::from("Ana"),
            text: String::from("I never said this"),
            sent_at_host: None,
            about: false,
        };
        let bad = rig.host.drops.bad;
        for bytes in [
            raw(&[b'o', b'k', 0xC3, 0x28]),
            raw("q".repeat(chat::MAX_TEXT_BYTES + 1).as_bytes()),
            raw("\u{1}\u{7}\u{1b}\t\r\n\u{202E}\u{200B}".as_bytes()),
            raw(lines.as_bytes()),
            from_a_host.encode(),
        ] {
            bo.chat(&mut rig, &bytes, start);
        }
        assert_eq!(rig.host.drops.bad, bad + 5);
        assert!(rig.host.view(start, 0).chat.is_empty());
        assert!(ana.said(start).is_empty());

        let refused: Vec<String> = captured
            .lines()
            .into_iter()
            .filter(|line| line.contains("refused and not handed on"))
            .collect();
        let whys: Vec<&str> = refused
            .iter()
            .map(|line| line.rsplit(": ").next().unwrap_or_default())
            .collect();
        assert_eq!(
            whys,
            [
                "it does not parse",
                "the message is over 900 bytes",
                "nothing is left of the message once cleaned",
                "the message has more than 20 lines",
                "only a host hands messages on",
            ]
        );
        assert!(refused.iter().all(|line| !line.contains("qqq")));

        // The stream itself is fine, and what keeps the rules gets through,
        // cleaned.
        bo.chat(&mut rig, &raw("\u{202E}fine\u{7}".as_bytes()), start);
        assert_eq!(
            said_texts(&ana.said(start)),
            [(String::from("Bo"), String::from("fine"))]
        );
        let lines = captured.lines();
        let line = lines
            .iter()
            .find(|line| line.starts_with("chat: "))
            .expect("a line for the message");
        assert!(
            line.ends_with("wrote 4 bytes, delivery not measured, handed to 1 friend"),
            "{line}"
        );
    }

    fn said_only(messages: Vec<ChatMessage>) -> Vec<String> {
        messages
            .into_iter()
            .filter_map(|message| match message {
                ChatMessage::Said { text, .. } => Some(text),
                ChatMessage::Say { .. } => None,
            })
            .collect()
    }

    // A friend's program saying far more than anyone types: past the burst
    // the host refuses the rest, and every other friend gets exactly the
    // lines the host kept, however slowly they read.
    #[test]
    fn chat_flood_cut_to_rate() {
        let start = Instant::now();
        let mut rig = Rig::new(start);
        let mut ana = Guest::new();
        ana.join(&mut rig, "Ana", start);
        let mut bo = Guest::new();
        bo.join(&mut rig, "Bo", start);

        let burst = chat::SAY_BURST as usize;
        let per_second = chat::SAYS_PER_SECOND as usize;
        let bad = rig.host.drops.bad;
        let later = start + Duration::from_secs(1);
        for (from, count, now) in [(0, burst + 50, start), (burst + 50, 25, later)] {
            for n in from..from + count {
                ana.chat(&mut rig, &say(&format!("line {n}")), now);
                // The host's acks, so the window never holds a line back.
                ana.read(now);
            }
        }

        let want: Vec<String> = (0..burst)
            .chain(burst + 50..burst + 50 + per_second)
            .map(|n| format!("line {n}"))
            .collect();
        let kept: Vec<String> = rig
            .host
            .view(later, 0)
            .chat
            .iter()
            .map(|line| line.text.clone())
            .collect();
        assert_eq!(kept, want);
        assert_eq!(rig.host.drops.bad, bad + 50 + 25 - per_second as u64);
        assert_eq!(said_only(bo.said_all(&mut rig, later)), want);
    }

    fn link_of<'a>(rig: &'a mut Rig, key: &[u8; 32]) -> &'a mut Link {
        let peer = rig.host.peers.iter_mut().find(|peer| peer.key == *key);
        &mut peer.expect("in the room").link
    }

    // The host hands on a friend's send time on its own clock, converted
    // with its offset to that friend, and says "about" when that offset came
    // over a jittery link. A wrong sign or no conversion at all would put the
    // time 5 s out.
    #[test]
    fn send_time_on_host_clock() {
        let start = Instant::now();
        let mut rig = Rig::new(start);
        let mut ana = Guest::new();
        ana.join(&mut rig, "Ana", start);
        let mut bo = Guest::new();
        bo.join(&mut rig, "Bo", start);
        let key = *ana.identity.public();
        let times = |messages: Vec<ChatMessage>| -> Vec<(Option<u64>, bool)> {
            messages
                .into_iter()
                .filter_map(|message| match message {
                    ChatMessage::Said {
                        sent_at_host,
                        about,
                        ..
                    } => Some((sent_at_host, about)),
                    ChatMessage::Say { .. } => None,
                })
                .collect()
        };
        // Ana's clock runs 5 s ahead of the host's.
        let ahead = 5_000_000;
        let host_now = rig.host.clock.micros(start);
        let sent = host_now + ahead as u64;

        // No offset to Ana yet, so nobody can tell when she sent it.
        ana.chat(&mut rig, &say_at("one", sent), start);
        assert_eq!(times(bo.said(start)), [(None, false)]);

        link_of(&mut rig, &key).offset.push(ClockSample {
            rtt_us: 400,
            offset_us: ahead,
        });
        ana.chat(&mut rig, &say_at("two", sent), start);
        assert_eq!(times(bo.said(start)), [(Some(host_now), false)]);

        // Ana's pings arrive 1 ms and 31 ms after she sent them, by turns.
        let stats = &mut link_of(&mut rig, &key).stats;
        for seq in 0..32u32 {
            let at = u64::from(seq) * 100_000;
            let transit = if seq % 2 == 0 { 1_000 } else { 31_000 };
            stats.peer_ping_received(seq, at, at + transit);
        }
        assert!(stats.snapshot().jitter_ms.is_some_and(|ms| ms > 5.0));
        ana.chat(&mut rig, &say_at("three", sent), start);
        assert_eq!(times(bo.said(start)), [(Some(host_now), true)]);
    }

    // What a friend says before the Hello on their link waits for it, so it
    // goes on under the name the Hello brings and not the fallback.
    #[test]
    fn chat_waits_for_the_hello_on_its_link() {
        let start = Instant::now();
        let mut rig = Rig::new(start);
        let mut bo = Guest::new();
        bo.join(&mut rig, "Bo", start);
        let mut ana = Guest::new();
        let initiation = ana.knock(&mut rig, start);
        ana.session = Some(ana.answer(initiation, start).expect("the host answers"));

        ana.chat(&mut rig, &say("you there?"), start);
        assert!(rig.host.view(start, 0).chat.is_empty());
        assert!(bo.said(start).is_empty());

        let hello = Message::Hello {
            version: invite::VERSION,
            name: String::from("Ana"),
            reached: None,
        };
        ana.say(&mut rig, &hello, start);
        assert_eq!(
            said_texts(&bo.said(start)),
            [(String::from("Ana"), String::from("you there?"))]
        );
        assert_eq!(
            texts(&rig.host.view(start, 0).chat),
            [("Ana", "you there?", false)]
        );
    }

    fn problems(view: &View) -> Vec<String> {
        view.chat
            .iter()
            .filter(|line| line.kind == LineKind::Problem)
            .map(|line| line.text.clone())
            .collect()
    }

    // Nothing after the Hello means the same to a friend of another
    // protocol. They get a Bye, and this room's chat says once who has which
    // version and what to do, however often they come back.
    #[test]
    fn other_protocol_friend_sent_away() {
        let start = Instant::now();
        let mut rig = Rig::new(start);
        let mut bo = Guest::new();
        bo.join(&mut rig, "Bo", start);
        let mut ana = Guest::new();
        let later = Version {
            major: 0,
            minor: 9,
            patch: 0,
        };
        for round in 0..3 {
            let now = start + Duration::from_secs(round);
            let initiation = ana.knock(&mut rig, now);
            ana.session = Some(ana.answer(initiation, now).expect("the host answers"));
            let hello = Message::OtherHello {
                protocol: invite::PROTOCOL + 1,
                version: later,
                name: String::from("Ana"),
            };
            ana.say(&mut rig, &hello, now);
            assert_eq!(rig.host.peers.len(), 1, "round {round}: only Bo stays");
            let heard = ana.heard(now);
            assert!(
                matches!(heard.first(), Some(Message::Hello { .. })),
                "{heard:?}"
            );
            assert!(matches!(heard.last(), Some(Message::Bye)), "{heard:?}");
            assert_eq!(ana.said(now), [], "a numbered build says it itself");
            ana.left();
        }
        assert_eq!(
            problems(&rig.host.view(start, 0)),
            [format!(
                "Ana has Booth 0.9.0 and this room runs {}, so they could not join. Get the same version as Ana from {}.",
                invite::VERSION,
                invite::RELEASES_PAGE
            )]
        );
    }

    // Exactly what those builds open with. Their own panel shows the Bye as
    // the host closing the room, with the line ahead of it in their chat.
    #[test]
    fn unversioned_friend_sent_away() {
        let start = Instant::now();
        let mut rig = Rig::new(start);
        let mut ana = Guest::new();
        let initiation = ana.knock(&mut rig, start);
        ana.session = Some(ana.answer(initiation, start).expect("the host answers"));
        let hello = Message::UnversionedHello {
            name: String::from("Ana"),
        };
        ana.say(&mut rig, &hello, start);
        assert!(rig.host.peers.is_empty());
        let took = ana.took(start);
        let this = invite::VERSION;
        let why = ChatMessage::Said {
            author: rig.invite.host_key,
            name: rig.host.name.clone(),
            text: format!(
                "This room runs Booth {this} and you have a test build of Booth made before the first release, so you could not join. Get Booth {this} from {}.",
                invite::RELEASES_PAGE
            ),
            sent_at_host: None,
            about: false,
        };
        assert!(
            matches!(
                &took[..],
                [.., Took::Chat(said), Took::Control(Message::Bye)] if *said == why
            ),
            "{took:?}"
        );
        let lines = took.iter().filter(|took| matches!(took, Took::Chat(_)));
        assert_eq!(lines.count(), 1, "{took:?}");
        assert_eq!(
            problems(&rig.host.view(start, 0)),
            [format!(
                "Ana has a test build of Booth made before the first release, so they could not join. Ask them to get Booth {} from {}.",
                invite::VERSION,
                invite::RELEASES_PAGE
            )]
        );
    }

    #[test]
    fn whoever_is_older_gets_the_newer_version() {
        let this = invite::VERSION;
        let page = invite::RELEASES_PAGE;
        let older = Version {
            major: 0,
            minor: 0,
            patch: 9,
        };
        assert_eq!(
            other_version_line("Ana", Some((invite::PROTOCOL + 1, older))),
            format!(
                "Ana has Booth 0.0.9 and this room runs {this}, so they could not join. Ask them to get Booth {this} from {page}."
            )
        );
        // Two test builds of one version name their protocols.
        let protocol = invite::PROTOCOL;
        assert_eq!(
            other_version_line("Ana", Some((protocol + 1, this))),
            format!(
                "Ana has Booth {this} (protocol {}) and this room runs {this} (protocol {protocol}), so they could not join. Get the same version as Ana from {page}.",
                protocol + 1
            )
        );
    }

    const OUTSIDE: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(203, 0, 113, 7), 41105);

    fn pcp_at(external: SocketAddrV4) -> Report {
        Report::Mapped {
            protocol: Protocol::Pcp,
            external,
            lifetime: 7200,
        }
    }

    fn publics(invite: &Invite) -> Vec<SocketAddr> {
        invite
            .candidates
            .iter()
            .filter(|c| c.kind == CandidateKind::Public)
            .map(|c| c.addr)
            .collect()
    }

    fn shown(host: &Host, now: Instant) -> InviteView {
        host.view(now, 0).invite.expect("an invite, or its slot")
    }

    #[test]
    fn name_checked_against_stun_or_router() {
        let start = Instant::now();
        let (log, captured) = Log::capture(64);
        let (mut host, _socket) = Rig::host(start, log, true);
        let named = String::from("myroom.duckdns.org");
        host.address_name = Some(AddressName::new(named.clone(), Lookup::default()));
        host.stun_resolved(start);
        host.port_mapping(pcp_at(OUTSIDE), start);
        let outcome = Outcome {
            servers: None,
            result: Ok(net::dns::Resolved {
                addrs: vec![net::dns::Found {
                    ip: (*OUTSIDE.ip()).into(),
                    source: net::dns::Source::Authoritative,
                }],
                refused: Vec::new(),
            }),
        };
        assert!(host.name_found(outcome));
        let checked = |host: &Host| {
            let name = host.view(start, 0).numbers.address_name.expect("the name");
            assert_eq!(name.name, named);
            name.outside.expect("both are known")
        };
        let router_says = checked(&host);
        assert_eq!(router_says.outside, *OUTSIDE.ip());
        assert!(router_says.is_this_pc());

        // STUN goes by what the internet sees, so it wins over the router.
        let moved = Ipv4Addr::new(198, 51, 100, 4);
        host.stun.set_public_v4(SocketAddrV4::new(moved, 41105));
        host.stun_answered();
        let stun_says = checked(&host);
        assert_eq!(stun_says.points_to, *OUTSIDE.ip());
        assert_eq!(stun_says.outside, moved);
        assert!(!stun_says.is_this_pc());

        let lines: Vec<String> = captured
            .lines()
            .into_iter()
            .filter(|line| line.starts_with("address name"))
            .collect();
        assert_eq!(
            lines,
            [
                "address name myroom.duckdns.org points to 203.0.113.7, this pc's outside address",
                "address name myroom.duckdns.org points to 203.0.113.7, not this pc's outside address 198.51.100.4; friends who look it up cannot reach this pc until the dynamic dns client updates it",
            ]
        );
    }

    // Behind two routers with STUN blocked, the inner router's outside
    // address is a private one, and a name pointing past it is no mistake.
    #[test]
    fn second_router_address_not_checked() {
        let start = Instant::now();
        let (log, captured) = Log::capture(64);
        let (mut host, _socket) = Rig::host(start, log, true);
        host.address_name = Some(AddressName::new(
            String::from("myroom.duckdns.org"),
            Lookup::default(),
        ));
        host.stun_resolved(start);
        let inner = SocketAddrV4::new(Ipv4Addr::new(192, 168, 0, 10), 41105);
        host.port_mapping(pcp_at(inner), start);
        let outcome = Outcome {
            servers: None,
            result: Ok(net::dns::Resolved {
                addrs: vec![net::dns::Found {
                    ip: Ipv4Addr::new(188, 48, 202, 128).into(),
                    source: net::dns::Source::Authoritative,
                }],
                refused: Vec::new(),
            }),
        };
        assert!(host.name_found(outcome));
        let name = host.view(start, 0).numbers.address_name.expect("the name");
        assert_eq!(name.outside, None);
        let lines: Vec<String> = captured
            .lines()
            .into_iter()
            .filter(|line| line.starts_with("address name"))
            .collect();
        assert_eq!(
            lines,
            [
                "address name myroom.duckdns.org points to 188.48.202.128; stun and the router have not said what this pc's outside address is"
            ]
        );
    }

    // Asking the router, which answers `report` right away.
    fn mapped_rig(start: Instant, report: Report) -> Rig {
        let (mut host, socket) = Rig::host(start, Log::off(), true);
        host.stun_resolved(start);
        host.port_mapping(report, start);
        Rig::holding(host, socket, start)
    }

    #[test]
    fn first_invite_waits_for_mapping() {
        let start = Instant::now();
        let (mut host, socket) = Rig::host(start, Log::off(), true);
        host.stun_resolved(start);
        let waiting = shown(&host, start);
        assert!(waiting.code.is_empty());
        assert_eq!(waiting.router, RouterState::Testing);
        let wait = Timers::default().first_invite_wait;
        assert_eq!(host.next_deadline(), Some(start + wait));

        let now = start + Duration::from_millis(400);
        assert!(host.port_mapping(pcp_at(OUTSIDE), now));
        let rig = Rig::holding(host, socket, now);
        assert!(rig.invite.mapped);
        assert!(!rig.invite.mapped_verified);
        assert!(!rig.invite.second_router);
        assert_eq!(publics(&rig.invite), [SocketAddr::V4(OUTSIDE)]);
        let view = rig.host.view(now, 0);
        assert_eq!(view.invite.unwrap().router, RouterState::Mapped);
        assert_eq!(view.numbers.mapping_protocol.as_deref(), Some("PCP"));
        assert_eq!(view.numbers.mapped_addr, Some(SocketAddr::V4(OUTSIDE)));
    }

    #[test]
    fn first_invite_skips_slow_router() {
        let start = Instant::now();
        let (log, captured) = Log::capture(64);
        let (mut host, socket) = Rig::host(start, log, true);
        host.stun_resolved(start);
        let wait = Timers::default().first_invite_wait;
        let almost = start + wait - Duration::from_millis(1);
        host.on_timer(almost, &socket);
        assert!(shown(&host, almost).code.is_empty());

        // The timer thread can come a little late.
        let now = start + wait + Duration::from_millis(40);
        assert!(host.on_timer(now, &socket));
        let first = shown(&host, now);
        assert!(!first.code.is_empty());
        assert_eq!(first.router, RouterState::Testing);
        assert!(!Invite::decode(&first.code).unwrap().mapped);
        let lines = captured.lines();
        assert!(
            lines.contains(&String::from(
                "the first invite waits no longer for the port mapping, 3.0 s after the room opened"
            )),
            "{lines:#?}"
        );

        // The sentence follows the router at once, the code only when the
        // user asks for a new one.
        let later = now + Duration::from_secs(2);
        host.port_mapping(pcp_at(OUTSIDE), later);
        let after = shown(&host, later);
        assert_eq!(after.router, RouterState::Mapped);
        assert_eq!(after.code, first.code);
        // The panel offers New invite beside Copy for this.
        assert!(!first.mapped_since);
        assert!(after.mapped_since);
        host.new_invite(false, later);
        let fresh = Invite::decode(&shown(&host, later).code).unwrap();
        assert!(!shown(&host, later).mapped_since);
        assert!(fresh.mapped);
        assert_eq!(publics(&fresh), [SocketAddr::V4(OUTSIDE)]);
    }

    // A STUN server name that takes seconds to look up, or to fail, holds up
    // the first invite no longer than a slow router does, and what the
    // router said in the meantime is in it.
    #[test]
    fn first_invite_skips_slow_lookup() {
        let start = Instant::now();
        let (log, captured) = Log::capture(64);
        let (mut host, socket) = Rig::host(start, log, true);
        let wait = Timers::default().first_invite_wait;
        assert_eq!(host.next_deadline(), Some(start + wait));
        host.port_mapping(pcp_at(OUTSIDE), start + Duration::from_millis(1));
        let almost = start + wait - Duration::from_millis(1);
        host.on_timer(almost, &socket);
        assert!(shown(&host, almost).code.is_empty());

        let now = start + wait;
        assert!(host.on_timer(now, &socket));
        let first = shown(&host, now);
        assert_eq!(first.router, RouterState::Testing);
        let invite = Invite::decode(&first.code).expect("an invite at 3 s");
        assert!(invite.mapped);
        assert_eq!(publics(&invite), [SocketAddr::V4(OUTSIDE)]);
        let lines = captured.lines();
        assert!(
            lines.contains(&String::from(
                "the first invite waits no longer for stun, 3.0 s after the room opened"
            )),
            "{lines:#?}"
        );

        // The names resolve at last, and the sentence catches up.
        let later = now + Duration::from_secs(5);
        assert!(host.stun_resolved(later));
        assert_eq!(shown(&host, later).router, RouterState::Mapped);
        assert_eq!(shown(&host, later).code, first.code);
    }

    // STUN's keepalive sees a new outside address long before the router is
    // asked again, so the mapper is asked to renew now.
    #[test]
    fn new_stun_address_renews_mapping() {
        let start = Instant::now();
        let (mut host, _socket) = Rig::host(start, Log::off(), true);
        let (renew, asked) = crossbeam_channel::bounded(1);
        host.renew_mapping_with(renew);
        let first = SocketAddrV4::new(*OUTSIDE.ip(), 52000);
        let moved = SocketAddrV4::new(Ipv4Addr::new(198, 51, 100, 4), 52000);
        host.stun.set_public_v4(first);
        host.stun_resolved(start);
        host.port_mapping(pcp_at(OUTSIDE), start);
        host.stun_answered();
        assert!(asked.try_recv().is_err());

        host.stun.set_public_v4(moved);
        host.stun_answered();
        assert_eq!(asked.try_recv(), Ok(()));
        // Not a second router while the router has not been asked.
        let view = host.view(start, 0);
        assert_eq!(view.invite.unwrap().router, RouterState::Mapped);
    }

    #[test]
    fn friend_through_mapping_verifies_it() {
        let start = Instant::now();
        let (log, captured) = Log::capture(256);
        let (mut host, socket) = Rig::host(start, log, true);
        host.stun_resolved(start);
        host.port_mapping(pcp_at(OUTSIDE), start);
        let mut rig = Rig::holding(host, socket, start);

        let mut ana = Guest::new();
        let lan = Some(SocketAddr::from(([192, 168, 1, 20], OUTSIDE.port())));
        ana.join_saying(&mut rig, "Ana", lan, start);
        assert_eq!(shown(&rig.host, start).router, RouterState::Mapped);

        let mut bo = Guest::new();
        rig.host.new_invite(true, start);
        rig.invite = Invite::decode(&shown(&rig.host, start).code).unwrap();
        bo.join_saying(&mut rig, "Bo", Some(SocketAddr::V4(OUTSIDE)), start);
        assert_eq!(shown(&rig.host, start).router, RouterState::MappedVerified);
        let lines = captured.lines();
        assert!(
            lines.iter().any(|line| line
                .ends_with("reached this host at 203.0.113.7:41105: the PCP mapping works")),
            "{lines:#?}"
        );

        rig.host.new_invite(false, start);
        let next = Invite::decode(&shown(&rig.host, start).code).unwrap();
        assert!(next.mapped);
        assert!(next.mapped_verified);
    }

    #[test]
    fn second_router_keeps_mapping_out() {
        let start = Instant::now();
        let inner = SocketAddrV4::new(Ipv4Addr::new(192, 168, 0, 20), 41105);
        let rig = mapped_rig(start, pcp_at(inner));
        assert!(rig.invite.second_router);
        assert!(!rig.invite.mapped);
        assert!(publics(&rig.invite).is_empty());
        let view = rig.host.view(start, 0);
        assert_eq!(view.invite.unwrap().router, RouterState::SecondRouter);
        // The stats panel still says what the router answered.
        assert_eq!(view.numbers.mapped_addr, Some(SocketAddr::V4(inner)));

        let none = mapped_rig(start, Report::Unmapped { wan: None });
        assert!(!none.invite.second_router);
        assert!(!none.invite.mapped);
        let view = none.host.view(start, 0);
        assert_eq!(view.invite.unwrap().router, RouterState::Unknown);
        assert_eq!(view.numbers.mapping_protocol, None);
    }

    fn code_for(guest: &Guest, answers: Answers, at: Option<SocketAddr>) -> ReplyCode {
        ReplyCode {
            answers,
            client_key: *guest.identity.public(),
            outside_v4: at.and_then(|addr| match addr {
                SocketAddr::V4(v4) => Some(v4),
                SocketAddr::V6(_) => None,
            }),
            outside_v6: None,
            mapping: Mapping::Easy,
            expires_at: crate::unix_now() + invite::REPLY_SECS,
        }
    }

    fn punches(wire: &Wire) -> usize {
        wire.packets()
            .iter()
            .filter(|packet| session::is_punch(packet))
            .count()
    }

    #[test]
    fn pasted_code_sends_ten_punches() {
        let start = Instant::now();
        let (log, captured) = Log::capture(256);
        let mut rig = Rig::with_log(start, log);
        let ana = Guest::new();
        let code = code_for(
            &ana,
            Answers::Invite(rig.invite.invite_id),
            Some(ana.wire.addr()),
        );
        let accepted = rig.host.accept_reply(&code, start).expect("accepted");
        assert_eq!(accepted.to, [ana.wire.addr()]);
        assert_eq!(rig.host.view(start, 0).paste, Some(PasteState::Sent));
        assert_eq!(rig.host.next_deadline(), Some(start));

        let mut now = start;
        let mut rounds = Vec::new();
        for _ in 0..15 {
            rig.tick(now);
            let packets = ana.wire.packets();
            assert!(packets.iter().all(|packet| session::is_punch(packet)));
            rounds.push(packets.len());
            now += crate::reply::PUNCH_GAP;
        }
        assert_eq!(rounds, [[1; 10].as_slice(), &[0; 5]].concat());
        let lines = captured.lines();
        assert!(
            lines.iter().any(|line| line
                == &format!(
                    "punch round 10 of 10 for {} sent to {}",
                    ana.identity.fingerprint(),
                    ana.wire.addr()
                )),
            "{lines:#?}"
        );
    }

    #[test]
    fn the_punches_stop_once_the_friend_is_in() {
        let start = Instant::now();
        let mut rig = Rig::new(start);
        let mut ana = Guest::new();
        let code = code_for(
            &ana,
            Answers::Invite(rig.invite.invite_id),
            Some(ana.wire.addr()),
        );
        rig.host.accept_reply(&code, start).expect("accepted");
        let mut now = start;
        for _ in 0..3 {
            rig.tick(now);
            now += crate::reply::PUNCH_GAP;
        }
        assert_eq!(punches(&ana.wire), 3);
        ana.join(&mut rig, "Ana", now);
        assert_eq!(rig.host.view(now, 0).paste, Some(PasteState::Joined));
        for _ in 0..10 {
            rig.tick(now);
            now += crate::reply::PUNCH_GAP;
        }
        assert_eq!(punches(&ana.wire), 0);
    }

    #[test]
    fn code_refusals() {
        let start = Instant::now();
        let mut rig = Rig::new(start);
        let mut ana = Guest::new();
        let bo = Guest::new();
        let open = Answers::Invite(rig.invite.invite_id);
        let at = Some(bo.wire.addr());
        let refused = |rig: &mut Rig, code: &ReplyCode, now: Instant| {
            let why = rig.host.accept_reply(code, now).expect_err("refused");
            assert_eq!(
                rig.host.view(now, 0).paste,
                Some(PasteState::Refused(why.clone()))
            );
            why
        };

        let mut expired = code_for(&bo, open, at);
        expired.expires_at = crate::unix_now() - 1;
        assert_eq!(refused(&mut rig, &expired, start), ReplyRefused::Expired);

        let unknown = code_for(&bo, Answers::Invite([9; 8]), at);
        assert_eq!(
            refused(&mut rig, &unknown, start),
            ReplyRefused::InviteNotLive
        );

        let rejoin = code_for(&bo, Answers::Rejoin, at);
        assert_eq!(refused(&mut rig, &rejoin, start), ReplyRefused::NotKnown);

        let nowhere = code_for(&bo, open, None);
        assert_eq!(refused(&mut rig, &nowhere, start), ReplyRefused::NoAddress);

        let mut hard = code_for(&bo, open, at);
        hard.mapping = Mapping::Hard;
        assert_eq!(refused(&mut rig, &hard, start), ReplyRefused::FriendHard);

        let good = code_for(&bo, open, at);
        assert!(rig.host.accept_reply(&good, start).is_ok());
        let soon = start + crate::reply::PASTE_GAP - Duration::from_millis(1);
        assert_eq!(refused(&mut rig, &good, soon), ReplyRefused::TooSoon);
        let later = start + crate::reply::PASTE_GAP;
        assert!(rig.host.accept_reply(&good, later).is_ok());

        ana.join(&mut rig, "Ana", later);
        let here = code_for(&ana, open, Some(ana.wire.addr()));
        assert_eq!(
            refused(&mut rig, &here, later),
            ReplyRefused::AlreadyHere {
                name: String::from("Ana")
            }
        );
        // Ana has a secret now, so a rejoin code of hers passes that check.
        let rejoin = code_for(&ana, Answers::Rejoin, Some(ana.wire.addr()));
        assert!(matches!(
            rig.host.accept_reply(&rejoin, later),
            Err(ReplyRefused::AlreadyHere { .. })
        ));

        // The host's own router changes ports: nothing it sends can help.
        rig.host
            .stun
            .set_settled(net::stun::Mapping::Hard, Some(OUTSIDE));
        let fresh = code_for(&Guest::new(), open, at);
        assert_eq!(refused(&mut rig, &fresh, later), ReplyRefused::HostHard);

        rig.host.leave(later, &rig.socket);
        assert_eq!(
            rig.host.accept_reply(&fresh, later),
            Err(ReplyRefused::Closed)
        );
    }

    #[test]
    fn single_use_code_only_for_its_key() {
        let start = Instant::now();
        let (mut host, socket) = Rig::host(start, Log::off(), false);
        host.stun_resolved(start);
        host.new_invite(false, start);
        let mut rig = Rig::holding(host, socket, start);
        let mut ana = Guest::new();
        let bo = Guest::new();
        let id = Answers::Invite(rig.invite.invite_id);

        // Answered is not enough: message 1 says nothing of the secret.
        let initiation = ana.knock(&mut rig, start);
        assert!(ana.answer(initiation, start).is_some());
        let for_bo = code_for(&bo, id, Some(bo.wire.addr()));
        assert!(rig.host.accept_reply(&for_bo, start).is_ok());

        // Confirmed under the new keys: the invite is Ana's now.
        ana.join(&mut rig, "Ana", start);
        assert_eq!(
            rig.host.accept_reply(&for_bo, start),
            Err(ReplyRefused::InviteNotLive)
        );
    }

    #[test]
    fn punched_address_takes_one_key() {
        let start = Instant::now();
        let (log, captured) = Log::capture(256);
        let mut rig = Rig::with_log(start, log);
        let mut ana = Guest::new();
        let mut bo = Guest::new();
        let punched = Wire::new();
        let code = code_for(
            &ana,
            Answers::Invite(rig.invite.invite_id),
            Some(punched.addr()),
        );
        rig.host.accept_reply(&code, start).expect("accepted");
        rig.tick(start);
        punched.packets();

        let psk = bo.invite_psk(&rig.invite);
        let kind = InitKind::Invite(rig.invite.invite_id);
        let bad = rig.host.drops.bad;
        bo.initiate(&mut rig, kind, &psk, punched.addr(), start);
        assert!(punched.packets().is_empty(), "Bo was answered");
        assert_eq!(rig.host.drops.bad, bad + 1);
        let lines = captured.lines();
        let why = format!("was punched open for {} only", ana.identity.fingerprint());
        assert!(lines.iter().any(|line| line.ends_with(&why)), "{lines:#?}");

        // Anywhere else Bo gets the usual rules.
        let initiation = bo.knock(&mut rig, start);
        assert!(bo.answer(initiation, start).is_some());

        let psk = ana.invite_psk(&rig.invite);
        let mut initiation = ana.initiate(&mut rig, kind, &psk, punched.addr(), start);
        let answered = punched
            .packets()
            .iter()
            .any(|packet| initiation.finish(packet, start).is_ok());
        assert!(answered, "Ana was not answered at the punched address");
    }

    #[test]
    fn code_cannot_take_held_address() {
        let start = Instant::now();
        let mut rig = Rig::new(start);
        let open = Answers::Invite(rig.invite.invite_id);
        let mut ana = Guest::new();
        ana.join(&mut rig, "Ana", start);
        let at_ana = ana.wire.addr();

        // Someone holding the multi-use invite names Ana's address as theirs.
        let bo = Guest::new();
        assert_eq!(
            rig.host
                .accept_reply(&code_for(&bo, open, Some(at_ana)), start),
            Err(ReplyRefused::AddressTaken { addr: at_ana })
        );
        // Nothing was punched open for Bo, so Ana's rekey from there is taken.
        let secret = rig.host.keys[ana.identity.public()]
            .secret
            .clone()
            .expect("the host made Ana a secret");
        let now = start + Duration::from_secs(1);
        let rekey = ana.initiate(&mut rig, InitKind::Rekey, &secret, at_ana, now);
        assert!(ana.answer(rekey, now).is_some(), "Ana's rekey was dropped");

        // An address punched open for Cy is not handed to Dee as well.
        let cy = Guest::new();
        let wire = Wire::new();
        let punched = wire.addr();
        let for_cy = code_for(&cy, open, Some(punched));
        assert!(rig.host.accept_reply(&for_cy, start).is_ok());
        let for_dee = code_for(&Guest::new(), open, Some(punched));
        assert_eq!(
            rig.host.accept_reply(&for_dee, start),
            Err(ReplyRefused::AddressTaken { addr: punched })
        );
        // Cy's own code for it is taken again once the paste gap is over.
        let later = start + crate::reply::PASTE_GAP;
        assert!(rig.host.accept_reply(&for_cy, later).is_ok());
    }

    #[test]
    fn code_decode_refuses_is_refused() {
        let start = Instant::now();
        let mut rig = Rig::new(start);
        let ana = Guest::new();
        let open = Answers::Invite(rig.invite.invite_id);
        let refused = |rig: &mut Rig, at: &str| {
            let code = code_for(&ana, open, Some(at.parse().unwrap()));
            match rig.host.accept_reply(&code, start) {
                Err(ReplyRefused::BadCode(invite::BuildError::BadAddress { addr, .. })) => {
                    assert_eq!(addr.to_string(), at);
                }
                other => panic!("{at}: {other:?}"),
            }
        };
        refused(&mut rig, "192.168.1.20:52000");
        refused(&mut rig, "10.0.0.7:52000");
        refused(&mut rig, "100.64.0.9:52000");

        // Loopback only on a host set up for tests on one PC.
        rig.host.punch_loopback = false;
        refused(&mut rig, "127.0.0.1:52000");
        rig.host.punch_loopback = true;
        let loopback = code_for(&ana, open, Some(ana.wire.addr()));
        assert!(rig.host.accept_reply(&loopback, start).is_ok());
    }

    #[test]
    fn unsendable_ipv6_left_out() {
        let start = Instant::now();
        let (log, captured) = Log::capture(64);
        let mut rig = Rig::with_log(start, log);
        let open = Answers::Invite(rig.invite.invite_id);
        let v6: SocketAddrV6 = "[2001:db8::7]:52000".parse().unwrap();
        // The rig's host has no addresses of its own, so no IPv6 either.
        let mut only_v6 = code_for(&Guest::new(), open, None);
        only_v6.outside_v6 = Some(v6);
        assert_eq!(
            rig.host.accept_reply(&only_v6, start),
            Err(ReplyRefused::NoAddress)
        );
        let lines = captured.lines();
        let why = format!("reply code: {v6} left out, this pc has no ipv6 address to send from");
        assert!(lines.contains(&why), "{lines:#?}");

        let ana = Guest::new();
        let mut both = code_for(&ana, open, Some(ana.wire.addr()));
        both.outside_v6 = Some(v6);
        let accepted = rig.host.accept_reply(&both, start).expect("accepted");
        assert_eq!(accepted.to, [ana.wire.addr()]);
    }

    #[test]
    fn reliable_line_once_a_minute() {
        let start = Instant::now();
        let (log, captured) = Log::capture(256);
        let mut rig = Rig::with_log(start, log);
        let mut ana = Guest::new();
        ana.join(&mut rig, "Ana", start);
        let prefix = format!("reliable to {}: ", ana.identity.fingerprint());
        let lines = |captured: &crate::log::Captured| {
            captured
                .lines()
                .into_iter()
                .filter(|line| line.starts_with(&prefix))
                .collect::<Vec<_>>()
        };
        let minute = Duration::from_secs(60);
        // Ana pings as a client does, so she stays in the room.
        let from = ana.wire.addr();
        for second in 1..60 {
            let now = start + Duration::from_secs(second);
            let session = ana.session.as_mut().expect("joined");
            let packet = ping(session, second as u32);
            rig.deliver(&packet, from, now);
            rig.tick(now);
        }
        assert!(lines(&captured).is_empty());
        assert!(
            rig.host
                .next_deadline()
                .is_some_and(|at| at <= start + minute)
        );
        rig.tick(start + minute - Duration::from_millis(1));
        assert!(lines(&captured).is_empty());
        rig.tick(start + minute);
        let written = lines(&captured);
        assert_eq!(written.len(), 1, "{written:#?}");
        assert!(written[0].contains(" messages acked, "), "{}", written[0]);
        rig.tick(start + minute + Duration::from_secs(1));
        assert!(lines(&captured).is_empty());
    }

    fn answer_stun(
        host: &mut Host,
        socket: &Socket,
        server: &Wire,
        seen: SocketAddrV4,
        now: Instant,
    ) {
        answer_stun_seeing(host, socket, server, SocketAddr::V4(seen), now);
    }

    fn answer_stun_seeing(
        host: &mut Host,
        socket: &Socket,
        server: &Wire,
        seen: SocketAddr,
        now: Instant,
    ) {
        let request = server.packets().pop().expect("a stun request");
        let txid: [u8; 12] = request[8..20].try_into().expect("a transaction id");
        let answer = crate::testing::stun_answer_seeing(&txid, seen);
        host.on_packet(&answer, server.addr(), now, socket);
    }

    const MOVED: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(198, 51, 100, 4), 41105);

    // A host whose first STUN round saw OUTSIDE, with Ana in the room from
    // `start`, and then Windows reports an address change and STUN sees
    // MOVED. Returns when that was.
    fn changed_under_ana(start: Instant, name_set: bool) -> (Rig, Guest, Instant) {
        let (mut host, socket) = Rig::host(start, Log::off(), false);
        if name_set {
            let name = AddressName::new(String::from("myroom.duckdns.org"), Lookup::default());
            host.address_name = Some(name);
        }
        let server = Wire::new();
        host.stun_found(vec![server.addr()], &socket);
        assert!(!host.stun_resolved(start));
        answer_stun(&mut host, &socket, &server, OUTSIDE, start);
        let mut rig = Rig::holding(host, socket, start);
        let mut ana = Guest::new();
        ana.join(&mut rig, "Ana", start);
        ana.wire.packets();

        let noticed = start + Duration::from_secs(1);
        rig.host.address_changed(noticed, &rig.socket, None);
        answer_stun(&mut rig.host, &rig.socket, &server, MOVED, noticed);
        (rig, ana, noticed)
    }

    #[test]
    fn new_public_address_pings_everyone() {
        let start = Instant::now();
        let (rig, ana, noticed) = changed_under_ana(start, false);
        let view = rig.host.view(noticed, 0);
        assert!(view.invite.expect("the invite").address_changed_since);
        assert_eq!(view.numbers.public_addr, Some(SocketAddr::V4(MOVED)));
        let change = view.numbers.address_change.expect("the change");
        assert!(change.this_pc);
        assert_eq!(
            (change.from, change.to),
            (SocketAddr::V4(OUTSIDE), SocketAddr::V4(MOVED))
        );
        assert_eq!(view.address_changed, None, "nobody has missed anything yet");
        let mut session = ana.session.expect("joined");
        let mut plain = Vec::new();
        let pinged = ana.wire.packets().iter().any(|packet| {
            session.decrypt(packet, &mut plain).is_ok()
                && matches!(
                    peer::read_plain(&plain),
                    Some(Plain::Ping(PingMessage::Ping { .. }))
                )
        });
        assert!(pinged, "no ping from the new address");
    }

    // The paste field's line for 2 minutes while friends are reconnecting
    // or lost, and the host sentence for up to 5 minutes while a friend who
    // was in the room is lost and not back.
    #[test]
    fn change_sentences_and_their_times() {
        for name_set in [false, true] {
            let start = Instant::now();
            let timers = Timers::default();
            let (mut rig, _ana, noticed) = changed_under_ana(start, name_set);
            let mut shown = |at: Instant| {
                rig.tick(at);
                rig.host.view(at, 0).address_changed
            };
            let sentence = |codes_cannot_help, friends_lost| {
                Some(AddressChanged {
                    codes_cannot_help,
                    friends_lost,
                    name_set,
                })
            };
            assert_eq!(
                shown(start + timers.reconnecting_after),
                sentence(true, false)
            );
            assert_eq!(shown(start + timers.lost_after), sentence(true, true));
            let two_minutes = noticed + CODES_USELESS_FOR;
            assert_eq!(
                shown(two_minutes - Duration::from_millis(1)),
                sentence(true, true)
            );
            assert_eq!(shown(two_minutes), sentence(false, true));
            let five_minutes = noticed + CHANGE_SHOWN_FOR;
            assert_eq!(
                shown(five_minutes - Duration::from_millis(1)),
                sentence(false, true)
            );
            assert_eq!(shown(five_minutes), None);
        }
    }

    #[test]
    fn change_sentence_goes_when_friend_back() {
        let start = Instant::now();
        let timers = Timers::default();
        let (mut rig, mut ana, _) = changed_under_ana(start, false);
        let lost = start + timers.lost_after;
        rig.tick(lost);
        assert!(
            rig.host
                .view(lost, 0)
                .address_changed
                .is_some_and(|shown| shown.friends_lost)
        );

        // Ana's ladder brings her back with the per-peer secret.
        let secret = rig.host.keys[ana.identity.public()]
            .secret
            .clone()
            .expect("a per-peer secret");
        let back = lost + Duration::from_secs(2);
        let from = ana.wire.addr();
        let initiation = ana.initiate(&mut rig, InitKind::Known, &secret, from, back);
        let mut session = ana.answer(initiation, back).expect("the host answers");
        rig.deliver(&ping(&mut session, 0), from, back);
        let view = rig.host.view(back, 0);
        assert_eq!(view.people.len(), 2);
        assert_eq!(view.address_changed, None);
        let took = view.numbers.reconnect_ms.expect("a reconnect time");
        let silence = numbers::millis(back - start);
        assert!((took - silence).abs() < 1.0, "{took} ms, not {silence} ms");
    }

    const OUTSIDE_V6: &str = "[2001:db8::7]:41105";
    const MOVED_V6: &str = "[2001:db8:1::7]:41105";

    // A dual-stack host: one STUN server sees it over IPv4, the other over
    // IPv6, both at `start`. Then the router drops: Ana, in the room from
    // `start`, is let go at lost_after, and Ben joins at 30 s.
    fn dual_stack_after_ana_left(start: Instant) -> (Rig, Wire, Wire, Guest) {
        let (mut host, socket) = Rig::host(start, Log::off(), false);
        let (v4, v6) = (Wire::new(), Wire::new());
        host.stun_found(vec![v4.addr(), v6.addr()], &socket);
        assert!(!host.stun_resolved(start));
        answer_stun(&mut host, &socket, &v4, OUTSIDE, start);
        let seen_v6 = OUTSIDE_V6.parse().unwrap();
        answer_stun_seeing(&mut host, &socket, &v6, seen_v6, start);
        host.new_invite(true, start);
        let mut rig = Rig::holding(host, socket, start);
        let mut ana = Guest::new();
        ana.join(&mut rig, "Ana", start);
        let lost = start + Timers::default().lost_after;
        rig.tick(lost);
        assert_eq!(rig.host.view(lost, 0).people.len(), 1, "Ana was let go");
        let mut ben = Guest::new();
        ben.join(&mut rig, "Ben", start + Duration::from_secs(30));
        ben.wire.packets();
        (rig, v4, v6, ben)
    }

    // The router comes back with a new IPv4 address and the same IPv6
    // prefix, and the IPv6 answer comes in first. It says nothing about
    // when IPv4 was last seen as it was.
    #[test]
    fn lost_friend_counts_in_either_family() {
        let start = Instant::now();
        let (mut rig, v4, v6, _ben) = dual_stack_after_ana_left(start);
        let back = start + Duration::from_secs(40);
        rig.host.address_changed(back, &rig.socket, None);
        let same_v6 = OUTSIDE_V6.parse().unwrap();
        answer_stun_seeing(&mut rig.host, &rig.socket, &v6, same_v6, back);
        answer_stun(&mut rig.host, &rig.socket, &v4, MOVED, back);
        let shown = rig.host.view(back, 0).address_changed;
        assert!(
            shown.is_some_and(|shown| shown.friends_lost),
            "Ana went with it: {shown:?}"
        );
    }

    // Both families come back changed, one answer after the other in the
    // same round: one change, one ping to each friend, and the IPv4 pair in
    // the stats panel.
    #[test]
    fn both_families_are_one_change() {
        let start = Instant::now();
        let (mut rig, v4, v6, mut ben) = dual_stack_after_ana_left(start);
        let back = start + Duration::from_secs(40);
        rig.host.address_changed(back, &rig.socket, None);
        answer_stun(&mut rig.host, &rig.socket, &v4, MOVED, back);
        let later = back + Duration::from_millis(30);
        let moved_v6 = MOVED_V6.parse().unwrap();
        answer_stun_seeing(&mut rig.host, &rig.socket, &v6, moved_v6, later);

        let view = rig.host.view(later, 0);
        assert!(
            view.address_changed.is_some_and(|shown| shown.friends_lost),
            "Ana went with it: {:?}",
            view.address_changed
        );
        let change = view.numbers.address_change.expect("the change");
        assert_eq!(
            (change.from, change.to),
            (SocketAddr::V4(OUTSIDE), SocketAddr::V4(MOVED))
        );
        assert_eq!(rig.host.change.as_ref().map(|change| change.at), Some(back));
        let session = ben.session.as_mut().expect("joined");
        let mut plain = Vec::new();
        let pings = ben
            .wire
            .packets()
            .iter()
            .filter(|packet| {
                session.decrypt(packet, &mut plain).is_ok()
                    && matches!(
                        peer::read_plain(&plain),
                        Some(Plain::Ping(PingMessage::Ping { .. }))
                    )
            })
            .count();
        assert_eq!(pings, 1, "one ping from the new address");
    }

    // After a router restart this PC can have another address toward it,
    // and a mapping made for the old one would lead nowhere.
    #[test]
    fn remap_uses_new_local_address() {
        let start = Instant::now();
        let (log, captured) = Log::capture(256);
        let (mut host, socket) = Rig::host(start, log, true);
        let (remap, asked) = crossbeam_channel::bounded(1);
        host.remap_with(remap);
        let server = Wire::new();
        host.stun_found(vec![server.addr()], &socket);
        assert!(!host.stun_resolved(start));
        answer_stun(&mut host, &socket, &server, OUTSIDE, start);

        let router = Ipv4Addr::new(192, 168, 1, 1);
        let now_at = Ipv4Addr::new(192, 168, 1, 23);
        let listing = Listing {
            addrs: vec![LocalAddr {
                ip: now_at.into(),
                kind: net::addrs::AddrKind::Lan,
                adapter: String::from("Ethernet"),
                has_gateway: true,
                gateway: Some(router.into()),
                vpn_adapter: false,
                hardware_adapter: true,
            }],
            router: Ok(Gateway {
                ip: router,
                local: now_at,
            }),
        };
        let noticed = start + Duration::from_secs(1);
        host.address_changed(noticed, &socket, Some(Ok(listing)));
        answer_stun(&mut host, &socket, &server, MOVED, noticed);
        assert_eq!(
            asked.try_recv(),
            Ok(Gateway {
                ip: router,
                local: now_at
            })
        );
        let lines = captured.lines();
        assert!(
            lines.contains(&String::from(
                "port mapping: the router to ask is now 192.168.1.1, this pc is 192.168.1.23 to it; the next mapping is asked of it"
            )),
            "{lines:#?}"
        );

        // The mapper stops when the router opened no port at the start.
        drop(asked);
        let again = noticed + Duration::from_secs(1);
        host.address_changed(again, &socket, None);
        answer_stun(&mut host, &socket, &server, OUTSIDE, again);
        assert!(captured.lines().contains(&String::from(
            "port mapping: not made again, the mapper stopped after the router opened no port at the start"
        )));
    }

    // What `session` can open of `packets`, as plain frames.
    fn opened(session: &mut Session, packets: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        for packet in packets {
            let mut plain = Vec::new();
            if session.decrypt(&packet, &mut plain).is_ok() {
                out.push(plain);
            }
        }
        out
    }

    fn voice_count(plain: &[Vec<u8>]) -> usize {
        plain
            .iter()
            .filter(|plain| matches!(peer::read_plain(plain), Some(Plain::Voice(_))))
            .count()
    }

    // Someone on the path who sees Ana's packets sends a copy of a fresh one
    // first, from an address of their own. Control follows it at once, and
    // Ana's next packet takes it back; the voice relayed to her never goes
    // there. A real move takes the voice once a ping to the new address is
    // answered from it.
    #[test]
    fn voice_moves_on_answered_ping() {
        use voice::codec::{Encoder, Mode};

        let mut rig = Rig::new(Instant::now());
        let mut ana = Guest::new();
        ana.join(&mut rig, "Ana", Instant::now());
        let mut bo = Guest::new();
        bo.join(&mut rig, "Bo", Instant::now());
        let ana_key = *ana.identity.public();
        let home = ana.wire.addr();
        let copier = Wire::new();
        let moved = Wire::new();

        let mut encoder = Encoder::new(Mode::LowDelay, true).unwrap();
        let pcm: Vec<f32> = (0..240).map(|n| 0.2 * (n as f32 * 0.06).sin()).collect();
        let mut opus = [0u8; 20];
        let len = encoder.encode(&pcm, &mut opus).unwrap();
        let mut seq = 0u16;
        let mut bo_talks = |rig: &mut Rig, bo: &mut Guest| {
            let frame = talk::Frame {
                seq,
                captured: 0,
                mode: Mode::LowDelay,
                redundancy: false,
                last: false,
                frame: &opus[..len],
                previous: None,
                pad: 0,
            };
            seq = seq.wrapping_add(1);
            let mut payload = Vec::new();
            frame.write_spoken(&mut payload);
            let packet = seal(bo.session.as_mut().unwrap(), Channel::Voice, &payload);
            rig.deliver(&packet, bo.wire.addr(), Instant::now());
        };
        let ana_peer = |rig: &Rig| {
            let peer = rig.host.peers.iter().find(|p| p.key == ana_key).unwrap();
            (peer.addr, peer.media.to())
        };

        bo_talks(&mut rig, &mut bo);
        let got = opened(ana.session.as_mut().unwrap(), ana.wire.packets());
        assert_eq!(voice_count(&got), 1);

        // The copy wins the race, and the original is a replay.
        let fresh = ping(ana.session.as_mut().unwrap(), 7);
        rig.deliver(&fresh, copier.addr(), Instant::now());
        rig.deliver(&fresh, home, Instant::now());
        assert_eq!(ana_peer(&rig), (copier.addr(), home));
        rig.tick(Instant::now());
        bo_talks(&mut rig, &mut bo);
        let at_copier = opened(ana.session.as_mut().unwrap(), copier.packets());
        assert!(
            at_copier.iter().any(|plain| matches!(
                peer::read_plain(plain),
                Some(Plain::Ping(PingMessage::Ping { .. }))
            )),
            "the new address is pinged at once"
        );
        assert_eq!(voice_count(&at_copier), 0, "voice went to the copier");
        assert_eq!(
            voice_count(&opened(ana.session.as_mut().unwrap(), ana.wire.packets())),
            1
        );

        // Ana's next packet brings control back, and nothing is left to check.
        let next = ping(ana.session.as_mut().unwrap(), 8);
        rig.deliver(&next, home, Instant::now());
        assert_eq!(ana_peer(&rig), (home, home));

        // A real move: voice stays until the ping to the new address comes
        // back from there.
        let from_there = ping(ana.session.as_mut().unwrap(), 9);
        rig.deliver(&from_there, moved.addr(), Instant::now());
        rig.tick(Instant::now());
        bo_talks(&mut rig, &mut bo);
        assert_eq!(
            voice_count(&opened(ana.session.as_mut().unwrap(), ana.wire.packets())),
            1
        );
        let (seq, t1) = opened(ana.session.as_mut().unwrap(), moved.packets())
            .iter()
            .find_map(|plain| match peer::read_plain(plain) {
                Some(Plain::Ping(PingMessage::Ping { seq, t1 })) => Some((seq, t1)),
                _ => None,
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
        let pong = seal(ana.session.as_mut().unwrap(), Channel::Ping, &pong);
        rig.deliver(&pong, moved.addr(), Instant::now());
        assert_eq!(ana_peer(&rig), (moved.addr(), moved.addr()));
        bo_talks(&mut rig, &mut bo);
        assert_eq!(
            voice_count(&opened(ana.session.as_mut().unwrap(), moved.packets())),
            1
        );
        assert_eq!(
            voice_count(&opened(ana.session.as_mut().unwrap(), ana.wire.packets())),
            0
        );
    }

    // Ana asks to share and the host grants it.
    fn granted(rig: &mut Rig, ana: &mut Guest, now: Instant) -> u32 {
        ana.say(rig, &Message::ShareStart { fps: 120 }, now);
        ana.heard(now)
            .into_iter()
            .find_map(|message| match message {
                Message::ShareAnswer(control::ShareAnswer::Granted { share }) => Some(share),
                _ => None,
            })
            .expect("the share granted")
    }

    // A frame of one data and one parity shard of the smallest size, the
    // cheapest packet a sharer can send.
    fn tiny_packet() -> Vec<u8> {
        let facts = channels::video::FrameFacts::default();
        let payload = channels::video::HEADER + channels::video::MIN_SHARD;
        let mut packetizer = channels::video::Packetizer::new(payload).expect("a packetizer");
        let packets = packetizer.packetize(&facts, &[7], 20).expect("packetized");
        packets.iter().next().expect("a packet").to_vec()
    }

    fn sent_video(guest: &mut Guest, packet: &[u8]) -> Vec<u8> {
        let mut payload = vec![1, 0];
        payload.extend_from_slice(packet);
        seal(
            guest.session.as_mut().expect("joined"),
            Channel::Video,
            &payload,
        )
    }

    fn facts_in(messages: &[Message]) -> Vec<control::Facts> {
        messages
            .iter()
            .filter_map(|message| match message {
                Message::ShareFacts(facts) => Some(*facts),
                _ => None,
            })
            .collect()
    }

    fn peer_at(rig: &mut Rig, guest: &Guest) -> usize {
        let key = *guest.identity.public();
        rig.host
            .peers
            .iter()
            .position(|peer| peer.key == key)
            .expect("in the room")
    }

    // A friend's PC can send as fast as its link goes. With a clock that
    // stands still, a sharer gets its burst of 3072 packets, the largest
    // frame there can be, then 20 000 a second; the rest is dropped and
    // counted, and nothing of it counts as bad.
    #[test]
    fn sharer_video_burst_and_rate() {
        let start = Instant::now();
        let mut rig = Rig::new(start);
        let mut ana = Guest::new();
        ana.join(&mut rig, "Ana", start);
        let mut bo = Guest::new();
        bo.join(&mut rig, "Bo", start);
        let share = granted(&mut rig, &mut ana, start);
        bo.say(
            &mut rig,
            &Message::Watch {
                share,
                on: true,
                hevc: true,
            },
            start,
        );
        let tiny = tiny_packet();
        let from = ana.wire.addr();
        for _ in 0..3100 {
            let packet = sent_video(&mut ana, &tiny);
            rig.deliver(&packet, from, start);
        }
        assert_eq!(
            (rig.host.screen.relayed, rig.host.screen.dropped),
            (3072, 28)
        );
        let later = start + Duration::from_millis(100);
        for _ in 0..2500 {
            let packet = sent_video(&mut ana, &tiny);
            rig.deliver(&packet, from, later);
        }
        println!(
            "5600 packets, 3100 at once and 2500 a tenth of a second later: {} passed on, {} dropped",
            rig.host.screen.relayed, rig.host.screen.dropped
        );
        assert_eq!(
            (rig.host.screen.relayed, rig.host.screen.dropped),
            (3072 + 2000, 28 + 500)
        );
        assert_eq!(rig.host.drops.bad, 0);
    }

    // The host's upload setting divided by the watchers on an internet path,
    // LAN watchers and people not watching not counted, and 1400-byte
    // datagrams only when every link involved is on the LAN. Every link here
    // is loopback, so the path words are set by hand.
    #[test]
    fn upload_split_among_internet_watchers() {
        let start = Instant::now();
        let mut rig = Rig::new(start);
        let mut guests: Vec<Guest> = ["Ana", "Bo", "Cy", "Dee"]
            .iter()
            .map(|name| {
                let mut guest = Guest::new();
                guest.join(&mut rig, name, start);
                guest
            })
            .collect();
        for internet in [1, 3] {
            let at = peer_at(&mut rig, &guests[internet]);
            rig.host.peers[at].path = PathWord::Direct;
        }
        let share = {
            let (ana, _) = guests.split_first_mut().unwrap();
            granted(&mut rig, ana, start)
        };
        // Each press a FACTS_EVERY after the one before, so each is told at
        // once.
        let mut at = start;
        let mut told = |rig: &mut Rig, guests: &mut [Guest], who: usize, on: bool| {
            at += crate::screen::FACTS_EVERY;
            let watch = Message::Watch {
                share,
                on,
                hevc: true,
            };
            guests[who].say(rig, &watch, at);
            let facts = facts_in(&guests[0].heard(at));
            *facts.last().expect("the sharer told")
        };
        // A watcher who starts has its IDR ask in them.
        let facts = |watchers, internet, cap_kbps, lan, idr| control::Facts {
            share,
            watchers,
            internet,
            cap_kbps,
            lan,
            hevc: true,
            idr,
        };
        assert_eq!(
            told(&mut rig, &mut guests, 1, true),
            facts(1, 1, 15_000, false, true)
        );
        assert_eq!(
            told(&mut rig, &mut guests, 2, true),
            facts(2, 1, 15_000, false, true)
        );
        assert_eq!(
            told(&mut rig, &mut guests, 3, true),
            facts(3, 2, 7_500, false, true)
        );
        assert_eq!(
            told(&mut rig, &mut guests, 1, false),
            facts(2, 1, 15_000, false, false)
        );
        assert_eq!(
            told(&mut rig, &mut guests, 3, false),
            facts(1, 0, 0, true, false)
        );
        // The sharer's own link counts too.
        let ana = peer_at(&mut rig, &guests[0]);
        rig.host.peers[ana].path = PathWord::Direct;
        let mut at = at + crate::screen::FACTS_EVERY;
        rig.tick(at);
        let facts_now = facts_in(&guests[0].heard(at));
        assert_eq!(facts_now, [facts(1, 0, 0, false, false)]);

        // The host's own share follows the same rule, and the rate its
        // encoder may use is never more than its own setting.
        guests[0].say(&mut rig, &Message::ShareStop, at);
        rig.host.share(120, at, &rig.socket);
        let own = rig
            .host
            .live
            .as_ref()
            .map(|live| live.number)
            .expect("the host shares");
        for who in [1, 3] {
            at += crate::screen::FACTS_EVERY;
            guests[who].say(
                &mut rig,
                &Message::Watch {
                    share: own,
                    on: true,
                    hevc: true,
                },
                at,
            );
        }
        let facts = rig.host.screen.sharing.facts().expect("facts");
        println!("the host's own share with two watchers over the internet: {facts:?}");
        assert_eq!(
            (
                facts.watchers,
                facts.internet,
                facts.rate_kbps,
                facts.spread,
                facts.lan
            ),
            (2, 2, 7_500, true, false)
        );
    }

    // The largest pointer shape is 256 KB, sent to every watcher over the
    // control channel. The first goes; a second right behind it is past the
    // bytes a sharer may send, and none of its chunks go on.
    #[test]
    fn one_largest_shape_at_once() {
        use crate::screen::wire;
        use crate::screen::{MAX_SHAPE_BYTES, Shape, ShapeKind};

        let start = Instant::now();
        let mut rig = Rig::new(start);
        let mut ana = Guest::new();
        ana.join(&mut rig, "Ana", start);
        let mut bo = Guest::new();
        bo.join(&mut rig, "Bo", start);
        let share = granted(&mut rig, &mut ana, start);
        bo.say(
            &mut rig,
            &Message::Watch {
                share,
                on: true,
                hevc: true,
            },
            start,
        );
        bo.heard(start);
        let largest = Shape {
            kind: ShapeKind::Color,
            width: 256,
            height: 256,
            pitch: 1024,
            hotspot_x: 0,
            hotspot_y: 0,
            scale_milli: 1000,
            bytes: vec![1; MAX_SHAPE_BYTES],
        };
        // The host's acks are read now and then, so the window never holds
        // the chunks back.
        let pump = |rig: &mut Rig, ana: &mut Guest| {
            ana.read(start);
            while let Some(frame) = ana.reliable.poll_transmit(start, None) {
                let packet = seal(ana.session.as_mut().unwrap(), Channel::Control, &frame);
                rig.deliver(&packet, ana.wire.addr(), start);
            }
        };
        for id in [1, 2] {
            for (n, chunk) in wire::chunks(share, id, &largest).enumerate() {
                let message = Message::Shape(chunk).encode();
                ana.reliable.send(&message).expect("queued");
                if n % 32 == 31 {
                    pump(&mut rig, &mut ana);
                }
            }
        }
        let chunks = MAX_SHAPE_BYTES / crate::screen::SHAPE_CHUNK;
        for _ in 0..8 {
            pump(&mut rig, &mut ana);
        }
        assert_eq!(rig.host.screen.dropped, chunks as u64);
        assert_eq!(rig.host.drops.bad, 0);
        println!(
            "two shapes of {MAX_SHAPE_BYTES} bytes, {chunks} chunks each, one right after the other: the second's {} chunks were held back",
            rig.host.screen.dropped
        );
    }

    fn shape_chunks(messages: Vec<Message>) -> Vec<crate::screen::wire::ShapeChunk> {
        messages
            .into_iter()
            .filter_map(|message| match message {
                Message::Shape(chunk) => Some(chunk),
                _ => None,
            })
            .collect()
    }

    // A friend's PC is hostile input. Each chunk is read against the total
    // it carries itself, so a sharer could pay for a 4-byte shape with its
    // first chunk and send the rest as parts of a 256 KB one. Those go
    // nowhere and count as bad; a shape sent by the rules still goes.
    #[test]
    fn shape_held_to_first_chunk_total() {
        use crate::screen::wire::{self, ShapeChunk};
        use crate::screen::{MAX_SHAPE_BYTES, SHAPE_CHUNK, Shape, ShapeKind};

        let start = Instant::now();
        let mut rig = Rig::new(start);
        let mut ana = Guest::new();
        ana.join(&mut rig, "Ana", start);
        let mut bo = Guest::new();
        bo.join(&mut rig, "Bo", start);
        let share = granted(&mut rig, &mut ana, start);
        bo.say(
            &mut rig,
            &Message::Watch {
                share,
                on: true,
                hevc: true,
            },
            start,
        );
        bo.heard(start);
        let color = |width: u16, height: u16| Shape {
            kind: ShapeKind::Color,
            width,
            height,
            pitch: width * 4,
            hotspot_x: 0,
            hotspot_y: 0,
            scale_milli: 1000,
            bytes: vec![1; usize::from(width) * usize::from(height) * 4],
        };
        let tiny = wire::chunks(share, 1, &color(1, 1))
            .next()
            .expect("a chunk");
        ana.say(&mut rig, &Message::Shape(tiny), start);
        for index in 1..4u16 {
            let bigger = ShapeChunk {
                share,
                id: 1,
                total: MAX_SHAPE_BYTES as u32,
                index,
                head: None,
                bytes: vec![7; SHAPE_CHUNK],
            };
            ana.say(&mut rig, &Message::Shape(bigger), start);
        }
        let passed = shape_chunks(bo.heard(start));
        assert_eq!(passed.len(), 1, "only the 4-byte shape's one chunk");
        assert_eq!(passed[0].total, 4);
        assert_eq!(rig.host.drops.bad, 3);

        let two = color(32, 16);
        for chunk in wire::chunks(share, 2, &two) {
            ana.say(&mut rig, &Message::Shape(chunk), start);
        }
        let passed = shape_chunks(bo.heard(start));
        assert_eq!(passed.len(), 2);
        assert!(
            passed
                .iter()
                .all(|chunk| chunk.id == 2 && chunk.total == 2048)
        );
        assert_eq!(rig.host.drops.bad, 3);
    }

    // One frame's packets as the packetizer makes them, frame `number`.
    fn frame_packet(number: u32) -> Vec<u8> {
        let facts = channels::video::FrameFacts {
            number,
            ..channels::video::FrameFacts::default()
        };
        let payload = channels::video::HEADER + channels::video::MIN_SHARD;
        let mut packetizer = channels::video::Packetizer::new(payload).expect("a packetizer");
        let packets = packetizer.packetize(&facts, &[7], 20).expect("packetized");
        packets.iter().next().expect("a packet").to_vec()
    }

    fn recovers_in(messages: &[Message]) -> Vec<(u32, u32)> {
        messages
            .iter()
            .filter_map(|message| match message {
                Message::Recover { first, last, .. } => Some((*first, *last)),
                _ => None,
            })
            .collect()
    }

    // A recover request can only be about a frame the host has passed on:
    // NVENC answers one about a frame it has not made yet with an IDR, so
    // a watcher naming frames ahead could have one every frame.
    #[test]
    fn recover_for_unsent_frame_dropped() {
        let start = Instant::now();
        let mut rig = Rig::new(start);
        let mut ana = Guest::new();
        ana.join(&mut rig, "Ana", start);
        let mut bo = Guest::new();
        bo.join(&mut rig, "Bo", start);
        let share = granted(&mut rig, &mut ana, start);
        bo.say(
            &mut rig,
            &Message::Watch {
                share,
                on: true,
                hevc: true,
            },
            start,
        );
        ana.heard(start);
        let recover = |first, last| Message::Recover { share, first, last };

        bo.say(&mut rig, &recover(0, 0), start);
        assert!(recovers_in(&ana.heard(start)).is_empty());
        assert_eq!(rig.host.drops.bad, 1);

        let from = ana.wire.addr();
        for number in 0..=5 {
            let packet = sent_video(&mut ana, &frame_packet(number));
            rig.deliver(&packet, from, start);
        }
        bo.say(&mut rig, &recover(4, 5), start);
        bo.say(&mut rig, &recover(5, 6), start);
        assert_eq!(recovers_in(&ana.heard(start)), [(4, 5)]);
        assert_eq!(rig.host.drops.bad, 2);

        // The host's own share: what its own threads sent.
        ana.say(&mut rig, &Message::ShareStop, start);
        rig.host.share(120, start, &rig.socket);
        let own = rig
            .host
            .live
            .as_ref()
            .map(|live| live.number)
            .expect("sharing");
        bo.say(
            &mut rig,
            &Message::Watch {
                share: own,
                on: true,
                hevc: true,
            },
            start,
        );
        rig.host.screen.sharing.take_answers();
        let mut outbox = rig.host.screen.sharing.outbox();
        outbox.video(&frame_packet(40));
        for (first, last) in [(41, 41), (39, 40)] {
            bo.say(
                &mut rig,
                &Message::Recover {
                    share: own,
                    first,
                    last,
                },
                start,
            );
        }
        assert_eq!(
            rig.host.screen.sharing.take_answers(),
            [crate::screen::Answer::Recover {
                first: 39,
                last: 40
            }]
        );
        assert_eq!(rig.host.drops.bad, 3);
    }

    // On their own, or in the facts that count a new watcher.
    fn idr_asks_in(messages: &[Message]) -> usize {
        messages
            .iter()
            .filter(|message| match message {
                Message::Idr { .. } => true,
                Message::ShareFacts(facts) => facts.idr,
                _ => false,
            })
            .count()
    }

    // Leaving and joining again gets a friend a new Peer, but not new
    // limits: a Watch right after rejoining asks the sharer for no IDR
    // while the last one is not IDR_ASK_EVERY old.
    #[test]
    fn rejoin_keeps_share_limits() {
        let start = Instant::now();
        let mut rig = Rig::new(start);
        let mut ana = Guest::new();
        ana.join(&mut rig, "Ana", start);
        let mut bo = Guest::new();
        bo.join(&mut rig, "Bo", start);
        let share = granted(&mut rig, &mut ana, start);
        ana.heard(start);
        let mut asks = Vec::new();
        for step in 0..8u64 {
            let at = start + Duration::from_millis(step * 100);
            if step > 0 {
                bo.say(&mut rig, &Message::Bye, at);
                bo.left();
                bo.join(&mut rig, "Bo", at);
            }
            bo.say(
                &mut rig,
                &Message::Watch {
                    share,
                    on: true,
                    hevc: true,
                },
                at,
            );
            asks.push(idr_asks_in(&ana.heard(at)));
        }
        println!("IDR asks from Bo watching, leaving and joining again every 100 ms: {asks:?}");
        assert_eq!(asks, [1, 0, 0, 0, 0, 1, 0, 0]);
        assert_eq!(rig.host.peers.len(), 2);
    }

    // Each ask for control puts a request in front of the sharer, so
    // leaving and joining again gets a friend no fresh asks either.
    #[test]
    fn rejoin_keeps_control_limits() {
        let start = Instant::now();
        let mut rig = Rig::new(start);
        let mut ana = Guest::new();
        ana.join(&mut rig, "Ana", start);
        let mut bo = Guest::new();
        bo.join(&mut rig, "Bo", start);
        let share = granted(&mut rig, &mut ana, start);
        ana.heard(start);
        let mut shown = 0;
        let mut too_soon = 0;
        for step in 0..8u32 {
            let at = start + Duration::from_millis(u64::from(step) * 10);
            if step > 0 {
                bo.say(&mut rig, &Message::Bye, at);
                bo.left();
                bo.join(&mut rig, "Bo", at);
            }
            let watch = Message::Watch {
                share,
                on: true,
                hevc: true,
            };
            bo.say(&mut rig, &watch, at);
            bo.say(
                &mut rig,
                &Message::ControlAsk {
                    share,
                    ask: step + 1,
                },
                at,
            );
            shown += ana
                .heard(at)
                .iter()
                .filter(|message| matches!(message, Message::ControlAsked { .. }))
                .count();
            too_soon += bo
                .heard(at)
                .iter()
                .filter(|message| {
                    matches!(
                        message,
                        Message::ControlAnswer {
                            answer: control::ControlAnswer::TooSoon,
                            ..
                        }
                    )
                })
                .count();
        }
        println!(
            "Bo asking for control, leaving and joining again every 10 ms: {shown} requests shown, {too_soon} refused as too soon"
        );
        assert_eq!(shown, crate::remote::ASK_BURST as usize);
        assert_eq!(too_soon, 8 - shown);
    }

    // A client watching hears of a new frame rate from the roster; this
    // host's own viewer has to be told.
    #[test]
    fn own_viewer_hears_new_fps() {
        use crate::screen::{Batch, WatchEvent};

        let start = Instant::now();
        let mut rig = Rig::new(start);
        let mut ana = Guest::new();
        ana.join(&mut rig, "Ana", start);
        let share = granted(&mut rig, &mut ana, start);
        assert!(rig.host.watch(share, true, start, &rig.socket));
        ana.say(&mut rig, &Message::ShareStart { fps: 60 }, start);
        let mut batch = Batch::default();
        rig.host.screen.watching.take(&mut batch);
        assert_eq!(
            batch.events,
            [
                WatchEvent::Started {
                    share,
                    fps: 120,
                    name: String::from("Ana")
                },
                WatchEvent::Fps { share, fps: 60 }
            ]
        );
    }

    // The host's own share asked for 120 runs stepped down at 60, which its
    // thread says and the roster shows. Picking 60 makes 60 the rate asked,
    // so a step back up opens at 60; picking 120 again changes nothing the
    // watchers see until the thread runs at it.
    #[test]
    fn own_share_asked_and_running_fps() {
        use crate::screen::{OwnShare, ShareNews};

        let start = Instant::now();
        let mut rig = Rig::new(start);
        assert!(rig.host.share(120, start, &rig.socket));
        let share = rig
            .host
            .live
            .as_ref()
            .map(|live| live.number)
            .expect("sharing");
        let runs_at = |rig: &mut Rig, fps: u8| {
            rig.host.screen.sharing.tell(ShareNews::Fps { share, fps });
            rig.host.screen_work(start, &rig.socket);
        };
        let rates = |rig: &Rig| {
            let asked = match rig.host.screen.sharing.state() {
                OwnShare::Sharing { fps, .. } => fps,
                other => panic!("{other:?}"),
            };
            (asked, rig.host.live.as_ref().map_or(0, |live| live.fps))
        };
        runs_at(&mut rig, 60);
        assert_eq!(rates(&rig), (120, 60));
        assert!(rig.host.share(60, start, &rig.socket));
        assert_eq!(rates(&rig), (60, 60));
        assert!(rig.host.share(120, start, &rig.socket));
        assert_eq!(rates(&rig), (120, 60), "the roster waits for the thread");
        runs_at(&mut rig, 120);
        assert_eq!(rates(&rig), (120, 120));
    }

    // Bo presses Watch, Stop watching, and Watch again 300 ms later. The
    // last Watch's IDR waits for the half second to end instead of being
    // dropped: on a still screen nothing else would bring Bo a picture, and
    // without it the sharer sends no pointer shape. An ask about a frame
    // inside the gap is dropped as before, and Stop watching takes a held
    // one back.
    #[test]
    fn watch_in_idr_gap_waits_for_it() {
        use crate::screen::IDR_ASK_EVERY;

        let start = Instant::now();
        let mut rig = Rig::new(start);
        let mut ana = Guest::new();
        ana.join(&mut rig, "Ana", start);
        let mut bo = Guest::new();
        bo.join(&mut rig, "Bo", start);
        let share = granted(&mut rig, &mut ana, start);
        let idrs = |messages: Vec<Message>| -> Vec<Option<u32>> {
            messages
                .into_iter()
                .filter_map(|message| match message {
                    Message::Idr { seen, .. } => Some(seen),
                    Message::ShareFacts(facts) if facts.idr => Some(None),
                    _ => None,
                })
                .collect()
        };
        bo.say(
            &mut rig,
            &Message::Watch {
                share,
                on: true,
                hevc: true,
            },
            start,
        );
        assert_eq!(idrs(ana.heard(start)), [None]);
        bo.say(
            &mut rig,
            &Message::Watch {
                share,
                on: false,
                hevc: true,
            },
            start,
        );
        let again = start + Duration::from_millis(300);
        bo.say(
            &mut rig,
            &Message::Watch {
                share,
                on: true,
                hevc: true,
            },
            again,
        );
        let seen = Message::Idr {
            share,
            seen: Some(4),
        };
        bo.say(&mut rig, &seen, again);
        assert!(idrs(ana.heard(again)).is_empty());
        let due = start + IDR_ASK_EVERY;
        assert_eq!(rig.host.share_deadline(), Some(due));
        rig.tick(due - Duration::from_millis(1));
        assert!(idrs(ana.heard(due)).is_empty());
        rig.tick(due);
        assert_eq!(idrs(ana.heard(due)), [None]);
        assert_eq!(rig.host.share_deadline(), None);

        let later = due + Duration::from_millis(100);
        bo.say(
            &mut rig,
            &Message::Watch {
                share,
                on: false,
                hevc: true,
            },
            later,
        );
        bo.say(
            &mut rig,
            &Message::Watch {
                share,
                on: true,
                hevc: true,
            },
            later,
        );
        bo.say(
            &mut rig,
            &Message::Watch {
                share,
                on: false,
                hevc: true,
            },
            later,
        );
        rig.tick(due + IDR_ASK_EVERY);
        assert!(idrs(ana.heard(due + IDR_ASK_EVERY)).is_empty());
        assert_eq!(rig.host.share_deadline(), None);
    }

    // A friend pressing Watch and Stop watching faster than the limit still
    // gets what they pressed last, so the host and they agree about who
    // watches. The sharer hears the first change at once and the rest at
    // most every FACTS_EVERY, and gets one IDR ask for all of it.
    #[test]
    fn watch_past_limit_still_counts() {
        use crate::screen::FACTS_EVERY;

        let start = Instant::now();
        let mut rig = Rig::new(start);
        let mut ana = Guest::new();
        ana.join(&mut rig, "Ana", start);
        let mut bo = Guest::new();
        bo.join(&mut rig, "Bo", start);
        let share = granted(&mut rig, &mut ana, start);
        ana.heard(start);
        let at = start + FACTS_EVERY;
        for press in 0..21 {
            let on = press % 2 == 0;
            bo.say(
                &mut rig,
                &Message::Watch {
                    share,
                    on,
                    hevc: true,
                },
                at,
            );
        }
        let b = peer_at(&mut rig, &bo);
        assert_eq!(rig.host.peers[b].share.watching, Some(share));
        let heard = ana.heard(at);
        assert_eq!(idr_asks_in(&heard), 1);
        assert_eq!(
            facts_in(&heard)
                .iter()
                .map(|f| f.watchers)
                .collect::<Vec<_>>(),
            [1]
        );

        let soon = at + Duration::from_millis(100);
        bo.say(
            &mut rig,
            &Message::Watch {
                share,
                on: false,
                hevc: true,
            },
            soon,
        );
        assert_eq!(rig.host.peers[b].share.watching, None);
        assert!(facts_in(&ana.heard(soon)).is_empty());
        let due = at + FACTS_EVERY;
        assert_eq!(rig.host.share_deadline(), Some(due));
        rig.tick(due);
        assert_eq!(
            facts_in(&ana.heard(due))
                .iter()
                .map(|f| f.watchers)
                .collect::<Vec<_>>(),
            [0]
        );
    }

    // A share goes in HEVC only while every watcher decodes it. A watcher who
    // does not makes the facts say so at once, inside FACTS_EVERY too, with
    // its IDR ask in them, so the sharer reads both before the same frame and
    // the new encoder's first frame is the one IDR the watcher needs. A
    // watcher whose facts wait for FACTS_EVERY changes no codec, and its ask
    // goes alone. A Watch said again with another answer changes the facts
    // and asks for no IDR, and HEVC comes back when the last watcher without
    // it stops, in the facts' own time.
    #[test]
    fn watcher_without_hevc() {
        use crate::screen::FACTS_EVERY;

        let start = Instant::now();
        let mut rig = Rig::new(start);
        let mut ana = Guest::new();
        ana.join(&mut rig, "Ana", start);
        let mut bo = Guest::new();
        bo.join(&mut rig, "Bo", start);
        let mut cy = Guest::new();
        cy.join(&mut rig, "Cy", start);
        let mut dee = Guest::new();
        dee.join(&mut rig, "Dee", start);
        let share = granted(&mut rig, &mut ana, start);
        let watch = |on, hevc| Message::Watch { share, on, hevc };
        // The sharer's view of what it heard, in order.
        let told_in = |messages: Vec<Message>| -> Vec<String> {
            messages
                .into_iter()
                .filter_map(|message| match message {
                    Message::ShareFacts(facts) => Some(format!(
                        "{} watching, {}{}",
                        facts.watchers,
                        if facts.hevc { "HEVC" } else { "H.264" },
                        if facts.idr { ", IDR" } else { "" }
                    )),
                    Message::Idr { seen: None, .. } => Some(String::from("IDR alone")),
                    _ => None,
                })
                .collect()
        };

        let at = start + FACTS_EVERY;
        cy.say(&mut rig, &watch(true, true), at);
        assert_eq!(told_in(ana.heard(at)), ["1 watching, HEVC, IDR"]);
        let soon = at + Duration::from_millis(10);
        bo.say(&mut rig, &watch(true, false), soon);
        assert_eq!(told_in(ana.heard(soon)), ["2 watching, H.264, IDR"]);

        // Bo's viewer finds out it does after all: no IDR, and the change
        // waits for FACTS_EVERY like any other.
        let then = soon + Duration::from_millis(10);
        bo.say(&mut rig, &watch(true, true), then);
        assert!(told_in(ana.heard(then)).is_empty());
        let due = soon + FACTS_EVERY;
        assert_eq!(rig.host.share_deadline(), Some(due));
        rig.tick(due);
        assert_eq!(told_in(ana.heard(due)), ["2 watching, HEVC"]);
        // And that it does not again, as when its GPU refused a stream of
        // it: at once, still no IDR.
        let refused = due + Duration::from_millis(10);
        bo.say(&mut rig, &watch(true, false), refused);
        assert_eq!(told_in(ana.heard(refused)), ["2 watching, H.264"]);

        // Dee, who takes HEVC, starts watching inside FACTS_EVERY: her
        // facts wait, and her ask goes alone.
        let dee_at = refused + Duration::from_millis(10);
        dee.say(&mut rig, &watch(true, true), dee_at);
        assert_eq!(told_in(ana.heard(dee_at)), ["IDR alone"]);

        // Bo stops watching: HEVC again, in the facts' own time.
        let gone = dee_at + Duration::from_millis(10);
        bo.say(&mut rig, &watch(false, false), gone);
        assert!(told_in(ana.heard(gone)).is_empty());
        let due = refused + FACTS_EVERY;
        rig.tick(due);
        assert_eq!(told_in(ana.heard(due)), ["2 watching, HEVC"]);
    }

    // Video the sharer sent just before Stop sharing arrives after it, over
    // a path of its own: it goes nowhere but is not counted as bad. A
    // second later it is.
    #[test]
    fn late_video_after_share_end() {
        let start = Instant::now();
        let mut rig = Rig::new(start);
        let mut ana = Guest::new();
        ana.join(&mut rig, "Ana", start);
        let mut bo = Guest::new();
        bo.join(&mut rig, "Bo", start);
        let share = granted(&mut rig, &mut ana, start);
        bo.say(
            &mut rig,
            &Message::Watch {
                share,
                on: true,
                hevc: true,
            },
            start,
        );
        ana.say(&mut rig, &Message::ShareStop, start);
        let from = ana.wire.addr();
        let packet = sent_video(&mut ana, &tiny_packet());
        rig.deliver(&packet, from, start + Duration::from_millis(100));
        assert_eq!((rig.host.drops.bad, rig.host.screen.relayed), (0, 0));
        let packet = sent_video(&mut ana, &tiny_packet());
        rig.deliver(&packet, from, start + Duration::from_secs(2));
        assert_eq!(rig.host.drops.bad, 1);
    }

    mod admission;
    mod limits;
    mod media;
}
