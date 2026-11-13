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
use crate::sound::{Sound, Switch};
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
// ping or two that was already on a slow path. A friend let go for silence
// said no Bye and goes on sending until their own lost_after runs out, so
// theirs are recognised for that much longer.
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
    // What its Hello says (Config::version).
    pub version: invite::Version,
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
    // Session indexes of people who just left, and until when: a Bye goes out
    // twice and its second copy lands after they are gone, and a friend let
    // go for silence goes on sending for a while.
    just_left: Vec<(u32, Instant)>,
    // Friends this room turned away for their version, so the chat says it
    // once each, however often they try again.
    other_versions: Vec<[u8; 32]>,
    // Reply codes pasted back: where the punches go and for which key.
    punches: Punches,
    punch_loopback: bool,
    version: invite::Version,
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
    // This host as a watcher of a friend's share, and that share's PC sound.
    own_watcher: Watcher,
    sound: Sound,
    // Share sound on this host's own share.
    own_sound: Switch,
    // The friend whose share ended last, until when what they sent before
    // it ended is let go quietly.
    share_ended: Option<([u8; 32], Instant)>,
    // The sharing limits of friends who left lately.
    left_limits: Vec<Kept>,
    // What a video packet is sealed into for each watcher, kept so passing
    // one on allocates nothing, and the same for PC sound.
    video_sealed: Vec<u8>,
    sound_sealed: Vec<u8>,
    // This host's upload setting, which the bitrate rule divides.
    upload_kbps: u32,
    // What a friend's share passed on over internet paths takes of that
    // upload. The cap in the facts is only advice to the sharer; this holds
    // the host to RELAY_SHARE times the setting whatever the sharer sends.
    relay_upload: Bucket,
    // Where the next relayed packet starts in the list of friends, which
    // turns with each one as the sharer's own Outbox does.
    relay_first: usize,
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
        let sound = Sound::new(setup.speaker.clone(), clock, setup.log.clone(), setup.now);
        let (pc_sound, upload_kbps) = (setup.screen.pc_sound.clone(), setup.screen.upload_kbps);
        let loopback = setup.screen.process_loopback;
        let screen = Screen::new(clock, true, setup.screen, setup.now);
        let own_sound = Switch::new(
            Arc::clone(screen.sharing.sound()),
            pc_sound,
            loopback,
            setup.log.clone(),
        );
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
            version: setup.version,
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
            upload_kbps,
            relay_upload: Bucket::full(setup.now, sharing::relay_per_second(upload_kbps)),
            relay_first: 0,
            screen,
            live: None,
            // At random and never 0, which no message carries: a friend's PC
            // that slept through a restart asks again by number for the share
            // it watched, and must find no share of this run under it.
            next_share: u32::from_le_bytes(peer::random()).max(1),
            own_watcher: Watcher::default(),
            sound,
            own_sound,
            share_ended: None,
            left_limits: Vec::new(),
            video_sealed: Vec::with_capacity(crate::screen::LAN_DATAGRAM),
            sound_sealed: Vec::with_capacity(crate::sound::MAX_SOUND + 1 + session::DATA_OVERHEAD),
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
    // message at once and the invite only when the user asks for a new one.
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
        // Its handshake would be dropped as the room is full, and the punches
        // would end in a line blaming the routers.
        if !self.is_peer(key) && self.seats_taken(key) >= MAX_CLIENTS {
            return Err(ReplyRefused::RoomFull);
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
        let word = self.stun.mapping_word();
        if self.stun.tick(now, socket) {
            self.stun_settled(now);
            changed = true;
        }
        // A round that ended at stun_wait: the panel shows the word.
        changed |= self.stun.mapping_word() != word;
        changed |= self.first_invite(now);
        self.note_load(now);
        if let Some(count) = self.cookies.capped_over(now) {
            self.say_capped(count);
        }
        self.punches.send_due(now, socket, &self.log);
        // When there is a path at all, the friend's tries reach this PC within
        // a second or two of the punches, so lost_after past the last round
        // means the routers keep the two apart. A code that ran out first,
        // pasted close to its end, means the same.
        let unanswered = self.punches.unanswered(now, self.timers.lost_after);
        let ran_out = self.punches.expire(now, &self.log);
        for key in unanswered.into_iter().chain(ran_out) {
            if let Some((pasted, state)) = self.last_paste.as_mut()
                && *pasted == key
                && *state == PasteState::Sent
            {
                log!(
                    self.log,
                    "{} did not come in after the punches",
                    keys::fingerprint(&key)
                );
                *state = PasteState::Missed;
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
            self.keep_left(&peer.sessions, lost_after + LEFT_GRACE, now);
            self.keep_limits(peer, now);
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
        self.sound.tick(now);
        if self.talk.report_due(now) {
            self.voice_reports(now, socket);
            self.share_reports(now, socket);
        }
        changed |= self.share_step(now, socket);
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
        self.gauge();
        changed
    }

    // Instant counted the time this PC was asleep, when nobody could be
    // heard. A friend heard since the wake, on the receive thread before this
    // pass, stays heard now rather than ahead of it.
    pub(crate) fn slept(&mut self, asleep: Duration, now: Instant) {
        if self.closed {
            return;
        }
        for peer in &mut self.peers {
            peer.last_heard = (peer.last_heard + asleep).min(now);
        }
        log!(
            self.log,
            "this pc was asleep for {}; the silence of {} in the room counts from the wake",
            secs(asleep),
            counted(self.peers.len() as u64, "friend", "friends")
        );
    }

    fn gauge(&self) {
        stats::gauge!("room.host.keys", self.keys);
        stats::gauge!("room.host.peers", self.peers);
        stats::gauge!(
            "room.host.peer.name",
            self,
            self.peers.iter().map(|peer| peer.name.len()).sum(),
            self.peers.iter().map(|peer| peer.name.capacity()).sum()
        );
        stats::gauge!(
            "room.link.minute.samples",
            self,
            self.peers
                .iter()
                .map(|peer| peer.link.minute_samples().0)
                .sum(),
            self.peers
                .iter()
                .map(|peer| peer.link.minute_samples().1)
                .sum()
        );
        stats::gauge!("room.host.pending", self.pending);
        stats::gauge!("room.host.lost", self.lost);
        stats::gauge!("room.host.scratch", self.scratch);
        stats::gauge!("room.host.just_left", self.just_left);
        stats::gauge!("room.host.other_versions", self.other_versions);
        stats::gauge!("room.host.reports", self.reports);
        stats::gauge!("room.host.voice_payload", self.voice_payload);
        stats::gauge!("room.host.voice_plain", self.voice_plain);
        stats::gauge!("room.host.voice_sealed", self.voice_sealed);
        stats::gauge!("room.host.left_limits", self.left_limits);
        stats::gauge!("room.host.video_sealed", self.video_sealed);
        stats::gauge!("room.host.sound_sealed", self.sound_sealed);
        stats::gauge!("room.host.input_events", self.input_events);
        stats::gauge!("room.host.input_sealed", self.input_sealed);
        self.invites.gauge();
        self.limit.gauge();
        self.cookies.gauge();
        self.history.gauge();
        self.delivery.gauge();
        self.booth_lines.gauge();
        self.other_lines.gauge();
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
        soonest.add(self.punches.next_deadline(self.timers.lost_after));
        soonest.add(self.cookies.next_deadline());
        soonest.add(self.booth_lines.next_deadline());
        soonest.add(self.other_lines.next_deadline());
        soonest.add(self.talk.next_deadline(!self.peers.is_empty()));
        soonest.add(self.sound.next_deadline());
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

    pub(crate) fn set_volume(&mut self, key: [u8; 32], percent: u8) {
        self.talk.set_volume(key, percent);
    }

    pub(crate) fn set_sound_volume(&mut self, key: [u8; 32], percent: u8) {
        self.sound.set_volume(key, percent);
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
        self.sound.fill(&mut numbers, now);

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
            peer.link.this_pc_moved();
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

    // The two messages the panel shows after a change, while they apply.
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
            // The second copy of a Bye, a packet that was on its way when
            // they left, or one from a friend let go for silence who has not
            // given up yet. Not junk, and nothing to write down.
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
                version: self.version,
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
        // Video, pointers, input and PC sound are passed on with their prefix
        // turned around in place, so they are taken before the read that
        // borrows it.
        match plain.first().map(|&byte| Channel::try_from(byte)) {
            Some(Ok(Channel::Video)) => return self.on_video(i, plain, now, socket),
            Some(Ok(Channel::Cursor)) => return self.on_cursor(i, plain, now, socket),
            Some(Ok(Channel::Input)) => return self.on_input(i, plain, now, socket),
            Some(Ok(Channel::Sound)) => return self.on_sound(i, plain, now, socket),
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
            Some(Plain::Feedback(frame)) => self.on_feedback(i, frame, now, socket),
            Some(Plain::Voice(payload)) => self.on_voice(i, payload, now, socket),
            // Taken above.
            Some(Plain::Video(_) | Plain::Cursor(_) | Plain::Input(_) | Plain::Sound(_)) | None => {
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

    // A watcher's recover requests, IDR asks and loss reports on their
    // stream; the same messages as on the control one, and as hostile.
    fn on_feedback(&mut self, i: usize, frame: &[u8], now: Instant, socket: &Socket) -> bool {
        let peer = &mut self.peers[i];
        if peer.link.receive_feedback(frame, now).is_err() {
            return self.drops.bad(now);
        }
        let mut delivered = Vec::new();
        while let Some(message) = peer.link.feedback.next_delivered() {
            delivered.push(message);
        }
        peer.flush(socket, now);
        for bytes in delivered {
            match Message::decode(&bytes) {
                Some(
                    message @ (Message::Recover { .. }
                    | Message::Idr { .. }
                    | Message::VideoLoss { .. }),
                ) => {
                    self.on_share_message(i, message, now, socket);
                }
                _ => {
                    self.drops.bad(now);
                }
            }
        }
        true
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
                    self.peers[i].link.feedback_read = version >= peer::FEEDBACK_SINCE;
                    self.peers[i].link.limits_read = version >= peer::LIMITS_SINCE;
                    self.peers[i].link.state_read = version >= peer::SHARE_STATE_SINCE;
                    self.peers[i].link.sound_read = version >= peer::PC_SOUND_SINCE;
                    self.peers[i].link.marks_read = version >= peer::MARK_SINCE;
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
                    | Message::WatchLimits { .. }
                    | Message::ShareState { .. }
                    | Message::ShareSound { .. }
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
        self.keep_left(&gone.sessions, LEFT_GRACE, now);
        self.pending.retain(|p| p.key != gone.key);
        if let Some(record) = self.keys.get_mut(&gone.key) {
            record.last_seen = now;
        }
        self.devices.seen(&gone.key, crate::unix_now());
        self.roster_changed(now, socket);
        self.left_share(&gone.key, now, socket);
        self.keep_limits(gone, now);
    }

    // A friend of another protocol cannot stay: nothing after the Hello
    // means the same to both sides. `theirs` is None for a test build from
    // before version numbers. The Bye ends it on their side at once. Such a
    // test build reads nothing of this host's Hello and shows the Bye as the
    // host closing the room, so one line in its own chat, under this host's
    // name, goes ahead of the Bye and says why. A numbered build has already
    // left over this host's Hello, with its own message.
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

    fn keep_left(&mut self, sessions: &Sessions, grace: Duration, now: Instant) {
        self.just_left.retain(|(_, until)| now < *until);
        for session in [&sessions.current, &sessions.previous]
            .into_iter()
            .flatten()
        {
            self.just_left.push((session.local_index(), now + grace));
        }
        // Only a full handshake per entry can add one, so this stays small;
        // the cap is for a program that joins and leaves in a loop.
        let over = self.just_left.len().saturating_sub(JUST_LEFT_KEPT);
        self.just_left.drain(..over);
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
mod tests;
