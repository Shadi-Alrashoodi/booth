//! Host and client roles: the threads that own the socket, the handshake
//! ladder, the session table, pings, the control channel and the roster.
//! The panel calls Room and reads View snapshots, nothing else.

#![forbid(unsafe_code)]

mod chat;
mod client;
mod config;
mod control;
mod cookies;
mod error;
mod host;
mod invites;
mod known;
mod limit;
mod log;
mod mapper;
mod minute;
mod names;
mod numbers;
mod peer;
pub mod remote;
mod reply;
mod router;
mod saver;
pub mod screen;
mod socket;
mod stun;
mod talk;
#[cfg(test)]
mod testing;
mod threads;
pub mod view;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use invite::{Invite, ReplyCode};
use keys::Identity;
use net::addrs::{AddrKind, LocalAddr, NoRouter};

use crate::client::{Client, ClientSetup, Ticket};
use crate::host::{Host, HostSetup};
use crate::invites::Local;
use crate::known::{DeviceBook, HostBook};
use crate::log::{Log, Writer, log, yes_no};
use crate::mapper::Target;
use crate::screen::ScreenSetup;
pub use crate::screen::StripClick;
use crate::socket::Socket;
use crate::talk::Speaker;
use crate::threads::{Outlets, Side, Threads, VoiceStart};

pub use chat::ChatRefused;
pub use config::{
    Candidates, Config, DEFAULT_VIDEO_UPLOAD_KBPS, Lookup, LossKnob, MAX_VIDEO_UPLOAD_KBPS, Timers,
    VideoConfig, VideoSource,
};
pub use error::RoomError;
pub use known::{
    BlockedKey, DamagedList, KnownDevice, KnownDevices, KnownError, KnownHost, List, ListProblem,
    Manual,
};
pub use remote::{
    Area, Button, Capture, ControlEnd, Controls, Held, Injected, Injection, Injector, InputEvent,
    ScanCode, Started, mouse_report,
};
pub use reply::{ReplyAccepted, ReplyRefused};
// What the panel needs to name a monitor and say how the viewer opens.
pub use share::{Capturing, Monitor, MonitorId, Show, monitors};
pub use talk::{Devices, TalkMode, VoiceConfig};
pub use threads::AddressHook;
pub use view::View;

/// Called after the View changed, from a room thread, never under a lock.
pub type Notify = Arc<dyn Fn() + Send + Sync>;

pub struct Room {
    threads: Threads,
    initiations_read: Arc<AtomicU64>,
}

impl Room {
    pub fn host(
        config: Config,
        identity: Arc<Identity>,
        room_name: String,
        notify: Notify,
    ) -> Result<Room, RoomError> {
        let (log, writer) = open_log(&config, "host")?;
        let socket = Arc::new(bind(&config, &log)?);
        let screen = screen_setup(&config, &socket)?;
        let video = config.video.clone();
        let (local, router) = match config.candidates {
            Candidates::Discover => {
                let addrs = net::addrs::local_addresses().map_err(|err| {
                    log!(log, "could not list this pc's addresses: {err}");
                    RoomError::LocalAddresses(err)
                })?;
                log_addresses(&log, &addrs);
                let router = match net::addrs::mapping_gateway(&addrs) {
                    Ok(gateway) => {
                        log!(
                            log,
                            "port mapping: the router to ask is {}, this pc is {} to it",
                            gateway.ip,
                            gateway.local
                        );
                        Some(gateway)
                    }
                    Err(why) => {
                        log_no_router(&log, &why);
                        None
                    }
                };
                (Local::Discover(addrs), router)
            }
            // Tests, which must never ask the real router for a port.
            Candidates::Fixed(fixed) => {
                log!(log, "no address lookup, {} fixed candidates", fixed.len());
                (Local::Fixed(fixed), None)
            }
        };
        socket.set_ipv6_source(local.ipv6_source(socket.has_ipv6()));
        log_ipv6_source(&log, &socket);
        let address_name = checked_name(config.address_name, &log);
        let (opened, turn) = known::room_devices(&config.data_dir);
        let count = opened.list.devices.len();
        known::note_opened(&log, List::Devices, &opened, count);
        if !opened.list.blocked.is_empty() {
            log!(log, "blocked keys: {}", opened.list.blocked.len());
        }
        let list_problem = opened.problem.as_ref().and_then(KnownError::problem);
        let voice = talk::Shared::new(&config.voice, Some(Arc::clone(&socket)), true);
        let (speaker_in, speaker_out) = crossbeam_channel::unbounded();
        let initiations_read = Arc::new(AtomicU64::new(0));
        let host = Host::new(HostSetup {
            identity,
            name: config.name,
            room_name,
            timers: config.timers,
            local,
            port: socket.local_port(),
            has_ipv6: socket.has_ipv6(),
            router,
            punch_loopback: config.punch_loopback,
            address_name,
            lookup: config.lookup,
            devices: DeviceBook::new(opened.list, opened.writable),
            list_problem,
            initiations_read: Arc::clone(&initiations_read),
            voice: Arc::clone(&voice),
            speaker: speaker_in,
            screen,
            now: Instant::now(),
            log: log.clone(),
        });
        let clock = host.clock();
        let mut threads = Threads::start(
            Side::Host(Box::new(host)),
            socket,
            config.stun_servers,
            router.map(Target::router),
            Outlets {
                notify,
                log,
                writer,
                turn,
            },
            VoiceStart {
                speaker: Speaker::new(speaker_out, Arc::clone(&voice), clock),
                config: config.voice,
                shared: voice,
                clock,
            },
            video,
        )
        .map_err(RoomError::Start)?;
        if config.watch_addresses {
            threads.watch_addresses();
        }
        Ok(Room {
            threads,
            initiations_read,
        })
    }

    pub fn join(
        config: Config,
        identity: Arc<Identity>,
        invite: Invite,
        notify: Notify,
    ) -> Result<Room, RoomError> {
        let joined = Instant::now();
        if invite.is_expired(unix_now()) {
            return Err(RoomError::InviteExpired);
        }
        Room::client(config, identity, Ticket::Invite(&invite), joined, notify)
    }

    /// Joins a host this PC joined before, with the secret it was given
    /// then and no invite: the same ladder as a join, from the stored
    /// addresses, with the reply code naming this PC's key if none of them
    /// answers.
    pub fn rejoin(
        config: Config,
        identity: Arc<Identity>,
        known: KnownHost,
        notify: Notify,
    ) -> Result<Room, RoomError> {
        let joined = Instant::now();
        Room::client(config, identity, Ticket::Known(&known), joined, notify)
    }

    fn client(
        config: Config,
        identity: Arc<Identity>,
        ticket: Ticket,
        joined: Instant,
        notify: Notify,
    ) -> Result<Room, RoomError> {
        let host_key = match &ticket {
            Ticket::Invite(invite) => invite.host_key,
            Ticket::Known(known) => known.host_key,
        };
        if host_key == *identity.public() {
            return Err(RoomError::OwnInvite);
        }
        if !client::host_key_usable(&identity, &host_key) {
            return Err(RoomError::BadHostKey);
        }
        let (log, writer) = open_log(&config, "client")?;
        let socket = Arc::new(bind(&config, &log)?);
        let screen = screen_setup(&config, &socket)?;
        let video = config.video.clone();
        // Only for the log: the client needs no addresses of its own, and
        // listing them takes Windows a few milliseconds.
        if log.is_on() {
            match net::addrs::local_addresses() {
                Ok(addrs) => log_addresses(&log, &addrs),
                Err(err) => log!(log, "could not list this pc's addresses: {err}"),
            }
        }
        log_ipv6_source(&log, &socket);
        let (opened, turn) = known::room_hosts(&config.data_dir);
        let count = opened.list.len();
        known::note_opened(&log, List::Hosts, &opened, count);
        let list_problem = opened.problem.as_ref().and_then(KnownError::problem);
        let voice = talk::Shared::new(&config.voice, Some(Arc::clone(&socket)), false);
        let (speaker_in, speaker_out) = crossbeam_channel::unbounded();
        let client = Client::new(ClientSetup {
            identity,
            name: config.name,
            ticket,
            timers: config.timers,
            port: socket.local_port(),
            has_ipv6: socket.has_ipv6(),
            lookup: config.lookup,
            joined,
            hosts: HostBook::new(opened.list, opened.writable),
            list_problem,
            voice: Arc::clone(&voice),
            speaker: speaker_in,
            screen,
            log: log.clone(),
        });
        let clock = client.clock();
        let mut threads = Threads::start(
            Side::Client(Box::new(client)),
            socket,
            config.stun_servers,
            None,
            Outlets {
                notify,
                log,
                writer,
                turn,
            },
            VoiceStart {
                speaker: Speaker::new(speaker_out, Arc::clone(&voice), clock),
                config: config.voice,
                shared: voice,
                clock,
            },
            video,
        )
        .map_err(RoomError::Start)?;
        if config.watch_addresses {
            threads.watch_addresses();
        }
        Ok(Room {
            threads,
            initiations_read: Arc::default(),
        })
    }

    /// The hosts this PC joined before, newest first. A damaged list is put
    /// aside as hosts.bin.bad and the error says so; the next call finds
    /// none. One a newer Booth wrote, or one Windows will not open, is left
    /// where it is and every call says so. Only while no room runs.
    pub fn known_hosts(data_dir: &Path) -> Result<Vec<KnownHost>, KnownError> {
        known::hosts(data_dir)
    }

    /// Takes the host and its secret off the list at once. Joining it again
    /// needs a new invite.
    pub fn forget_host(data_dir: &Path, host_key: &[u8; 32]) -> Result<(), KnownError> {
        known::forget_host(data_dir, host_key)
    }

    /// An address or name typed in for a known host, tried before any it
    /// stored; None clears it.
    pub fn set_manual(
        data_dir: &Path,
        host_key: &[u8; 32],
        manual: Option<Manual>,
    ) -> Result<(), KnownError> {
        known::set_manual(data_dir, host_key, manual)
    }

    /// The devices that joined rooms this PC hosted, newest first, and the
    /// keys it refuses. Only while no room runs.
    pub fn known_devices(data_dir: &Path) -> Result<KnownDevices, KnownError> {
        known::devices(data_dir)
    }

    /// The device and its secret go at once; it needs a new invite to join
    /// again.
    pub fn remove_device(data_dir: &Path, key: &[u8; 32]) -> Result<(), KnownError> {
        known::remove_device(data_dir, key)
    }

    pub fn unblock(data_dir: &Path, key: &[u8; 32]) -> Result<(), KnownError> {
        known::unblock(data_dir, key)
    }

    pub fn view(&self) -> View {
        self.threads.view()
    }

    /// The panel's Hold to talk, held down or let go. Only push to talk
    /// listens to it; open mic ignores it.
    pub fn talk(&self, held: bool) {
        self.threads.talk(held);
    }

    /// Muted, the microphone closes and nothing is sent. Unmuting also
    /// undeafens.
    pub fn mute(&self, muted: bool) {
        self.threads.mute(muted);
    }

    /// Deafened, the speakers play silence and the microphone closes too.
    /// Undeafened, the microphone opens again unless muted before.
    pub fn deafen(&self, deafened: bool) {
        self.threads.deafen(deafened);
    }

    /// Asks to share this PC's screen at `fps` frames a second: `monitor`,
    /// from room::monitors, or the primary one with None. A host decides at
    /// once; a client asks its host, and the view's `share.own` says when
    /// the answer came: Sharing, or Refused with the sentence to show.
    /// Nothing is captured before it. Then the room's own thread opens the
    /// capture and the encoder, and captures, encodes and sends while
    /// someone watches; `share.running` and `numbers.sharing` follow it.
    /// Once they are open the rising cue plays in this PC's own speakers,
    /// and the falling one when the share ends, however it ends. A share
    /// that cannot start ends, with a problem line in the chat and no cue.
    /// Asked again while sharing, it changes the frame rate; the monitor is
    /// the one it started with.
    pub fn share(&self, fps: u8, monitor: Option<MonitorId>) {
        self.threads.share(fps.clamp(1, control::MAX_FPS), monitor);
    }

    /// Needs no answer: the share is over for everyone at once, and its
    /// thread stops capturing at its next frame, or within 100 ms on a still
    /// screen. Leaving the room stops it before anything else.
    pub fn stop_sharing(&self) {
        self.threads.stop_sharing();
    }

    /// Watch, or Stop watching, the share the view named with `share`. The
    /// host passes video on only to those who watch. Watching opens the
    /// viewer's window, on the room's own thread; closing the window is the
    /// same as Stop watching, and the share ending closes it.
    pub fn watch(&self, share: u32, on: bool) {
        self.threads.watch(share, on);
    }

    /// Called on the viewer's thread when someone clicks the viewer's strip,
    /// to bring the panel forward with the stats panel open. It must return
    /// at once.
    pub fn on_strip_click(&self, click: StripClick) {
        self.threads.watching().on_strip_click(click);
    }

    /// For tests, which never click on a window: closes the viewer the way
    /// the person closing it would, which stops watching.
    #[doc(hidden)]
    pub fn close_viewer(&self) {
        self.threads.watching().close_viewer();
    }

    /// For tests that play the sharer by hand (VideoSource::Hooks): where
    /// its packets go out and the watchers' answers come back.
    #[doc(hidden)]
    pub fn sharing(&self) -> Arc<screen::Sharing> {
        self.threads.sharing()
    }

    /// For tests that play the viewer by hand (VideoSource::Hooks): where
    /// what arrives for the share watched waits, and what goes back.
    #[doc(hidden)]
    pub fn watching(&self) -> Arc<screen::Watching> {
        self.threads.watching()
    }

    /// Asks the person sharing `share`, which this PC watches, to let it
    /// control their PC. The view's `share.control.asking` says it waits,
    /// and `controlling` names them once they allow it; a refusal is a
    /// system line. One controller at a time.
    pub fn ask_control(&self, share: u32) {
        self.threads.ask_control(share);
    }

    /// This PC's owner answers the request on show, named by the `number`
    /// in the view's `share.control.asked_by`. An answer to a request that
    /// is no longer on show does nothing, so a click never allows someone
    /// whose request took the place of the one the owner read. Allowed, the
    /// injector hears `started` and the controller's input reaches it; it
    /// stays allowed until the panic key, Stop control, the host's End
    /// control, the share ending, the session ending or the room closing,
    /// and never longer.
    pub fn answer_control(&self, request: u32, allow: bool) {
        self.threads.answer_control(request, allow);
    }

    /// Stop control: control of this PC ends as stopped, control this PC
    /// has of another, or its ask, as released. A request on show is
    /// declined.
    pub fn stop_control(&self) {
        self.threads.stop_control(false);
    }

    /// The panic key on this PC's own keyboard: control of this PC ends as
    /// the panic key, and a request on show is declined; on the controller's
    /// side it is the release key. The app cuts the injector itself first,
    /// without waiting for this.
    pub fn panic_key(&self) {
        self.threads.stop_control(true);
    }

    /// Host only, the row menu's End control: whoever controls the share
    /// stops, and both sides are told the host ended it.
    pub fn end_control(&self) {
        self.threads.end_control();
    }

    /// Where the app's capture hands the controller's input while this PC
    /// controls someone's share and its viewer is in front. It sends
    /// nothing while this PC controls nothing.
    pub fn controls(&self) -> Arc<Controls> {
        self.threads.controls()
    }

    /// For tests: how many initiations a host did key math for, counted as
    /// it happens, where the View is a copy made after a change. Always zero
    /// on a client.
    #[doc(hidden)]
    pub fn initiations_read(&self) -> u64 {
        self.initiations_read.load(Ordering::Relaxed)
    }

    /// Host only; a client ignores it.
    pub fn new_invite(&self, multi_use: bool) {
        self.threads.new_invite(multi_use);
    }

    /// Client only: a fresh reply code in place of the one on show or the
    /// one that expired. A host ignores it.
    pub fn new_code(&self) {
        self.threads.new_code();
    }

    /// Says something in the room's chat. The text is cleaned the way the
    /// host will clean it again, and what is left shows in the view at once.
    /// Refused while a client has lost the host or the room has closed;
    /// while the host is only quiet the line waits and goes when it is heard
    /// again, or on the next session if the host is lost before that.
    pub fn say(&self, text: &str) -> Result<(), ChatRefused> {
        self.threads.say(text)
    }

    /// Host only: a reply code a friend sent back, decoded with
    /// ReplyCode::decode. Accepted, it sends a few punch packets toward the
    /// friend's address. A code decode would refuse is refused here too.
    /// The view's paste line follows either way.
    pub fn accept_reply(&self, code: ReplyCode) -> Result<ReplyAccepted, ReplyRefused> {
        self.threads.accept_reply(&code)
    }

    /// What the address watch calls when Windows reports that an address on
    /// this PC changed; with Config::watch_addresses on, the room connects
    /// it itself. Either side then asks STUN whether its outside address
    /// changed too. A hook that outlives the room does nothing.
    pub fn address_hook(&self) -> AddressHook {
        self.threads.address_hook()
    }

    pub fn leave(mut self) {
        self.threads.stop();
    }
}

/// Waits up to `within` for the routers of rooms already left to confirm
/// their port mappings are gone; true when all did. Leave hands a slow
/// deletion to a thread of its own, which ends with the process, so call
/// this before exiting or the mapping can stay on the router.
pub fn finish_port_mappings(within: Duration) -> bool {
    mapper::finish(within)
}

fn open_log(config: &Config, role: &'static str) -> Result<(Log, Option<Writer>), RoomError> {
    let (log, writer) =
        log::open(config.log.as_deref(), role).map_err(|source| RoomError::Log {
            path: config.log.clone().unwrap_or_default(),
            source,
        })?;
    let exe = std::env::current_exe().map_or_else(
        |err| format!("unknown ({err})"),
        |path| path.display().to_string(),
    );
    log!(
        log,
        "booth {} starting as {role}, exe {exe}",
        env!("CARGO_PKG_VERSION")
    );
    log!(log, "data dir {}", config.data_dir.display());
    Ok((log, writer))
}

fn bind(config: &Config, log: &Log) -> Result<Socket, RoomError> {
    let asked = match config.port {
        0 => String::from("any udp port"),
        port => format!("udp port {port}"),
    };
    let socket = Socket::bind(config.port, log.clone()).map_err(|err| {
        log!(log, "asked for {asked}: {err}");
        RoomError::Bind(err)
    })?;
    // What net's bind does: one dual-stack socket on [::], or 0.0.0.0 on a
    // PC without IPv6. Either way every adapter's address reaches it.
    let port = socket.local_port();
    let bound = if socket.has_ipv6() {
        format!("[::]:{port}, dual-stack, ipv4 and ipv6")
    } else {
        format!("0.0.0.0:{port}, ipv4 only")
    };
    log!(log, "asked for {asked}, bound {bound}");
    Ok(socket)
}

fn screen_setup(config: &Config, socket: &Arc<Socket>) -> Result<ScreenSetup, RoomError> {
    Ok(ScreenSetup {
        socket: Some(Arc::clone(socket)),
        upload_kbps: config.upload_kbps(),
        wake: net::pace::Signal::new().map_err(RoomError::Start)?,
        answered: net::pace::Signal::new().map_err(RoomError::Start)?,
        threads: config.video.source != VideoSource::Hooks,
        knob: config.video.loss,
        hevc: config.video.hevc,
        injector: config.video.injector.clone(),
        control_wake: net::pace::Signal::new().map_err(RoomError::Start)?,
    })
}

fn log_addresses(log: &Log, addrs: &[LocalAddr]) {
    if addrs.is_empty() {
        log!(log, "no local addresses found");
    }
    for addr in addrs {
        let kind = match addr.kind {
            AddrKind::Lan => "lan",
            AddrKind::Vpn => "vpn",
            AddrKind::Ipv6Global => "global ipv6",
        };
        // An adapter can have a gateway of the other family only.
        let gateway = match addr.gateway {
            Some(ip) => ip.to_string(),
            None => String::from(yes_no(addr.has_gateway)),
        };
        log!(
            log,
            "local address {}: {kind}, adapter {}, gateway {gateway}, vpn adapter {}, hardware adapter {}",
            addr.ip,
            log::quoted(&addr.adapter),
            yes_no(addr.vpn_adapter),
            yes_no(addr.hardware_adapter)
        );
    }
}

fn log_no_router(log: &Log, why: &NoRouter) {
    match why {
        NoRouter::Tunnel(adapter) => log!(
            log,
            "port mapping: the route to the internet goes through the tunnel adapter {}, which has no router to ask, left out",
            log::quoted(adapter)
        ),
        NoRouter::NoGateway(adapter) => log!(
            log,
            "port mapping: the route to the internet goes through adapter {}, which has no ipv4 router, left out",
            log::quoted(adapter)
        ),
        NoRouter::Nowhere => log!(
            log,
            "port mapping: no adapter has an ipv4 router to ask, left out"
        ),
    }
}

// Settings takes only names the invite can carry. One that got past it would
// leave the host with no invite at all, so it is left out instead.
fn checked_name(name: Option<String>, log: &Log) -> Option<String> {
    let name = name?;
    match invite::check_hostname(&name) {
        Ok(()) => {
            log!(log, "address name {name}, which every invite carries");
            Some(name)
        }
        Err(err) => {
            log!(log, "address name {} left out: {err}", log::quoted(&name));
            None
        }
    }
}

fn log_ipv6_source(log: &Log, socket: &Socket) {
    match socket.ipv6_source() {
        Some(ip) => log!(log, "ipv6 replies to global addresses leave from {ip}"),
        None => log!(log, "ipv6 source not pinned, windows picks it"),
    }
}

pub(crate) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

pub(crate) fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

#[derive(Default)]
pub(crate) struct Soonest(pub Option<Instant>);

impl Soonest {
    pub(crate) fn add(&mut self, at: Option<Instant>) {
        if let Some(at) = at {
            self.0 = Some(self.0.map_or(at, |soonest| soonest.min(at)));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use super::*;

    // Invites leave out a gatewayless address only on an adapter Windows
    // does not mark as hardware, such as the Hyper-V and WSL switches no
    // friend can reach, so the log says which is which.
    #[test]
    fn address_lines_name_hardware_adapters() {
        let addr = |ip: [u8; 4], adapter: &str, hardware_adapter: bool| LocalAddr {
            ip: IpAddr::V4(Ipv4Addr::from(ip)),
            kind: AddrKind::Lan,
            adapter: adapter.to_owned(),
            has_gateway: false,
            gateway: None,
            vpn_adapter: false,
            hardware_adapter,
        };
        let (log, captured) = Log::capture(8);
        log_addresses(
            &log,
            &[
                addr([192, 168, 1, 20], "Ethernet 2", true),
                addr([172, 24, 64, 1], "vEthernet (WSL)", false),
            ],
        );
        assert_eq!(
            captured.lines(),
            [
                "local address 192.168.1.20: lan, adapter \"Ethernet 2\", gateway no, vpn adapter no, hardware adapter yes",
                "local address 172.24.64.1: lan, adapter \"vEthernet (WSL)\", gateway no, vpn adapter no, hardware adapter no",
            ]
        );
    }
}
