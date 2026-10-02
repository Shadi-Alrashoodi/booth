// Each test file uses a different part of this.
#![allow(dead_code)]

pub mod voiced;

use std::fs;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use invite::{Candidate, CandidateKind, Invite};
use keys::Identity;
use net::dns::{Kind, System, SystemError};
use room::view::View;
use room::{
    Candidates, Config, Devices, KnownHost, Lookup, Notify, Room, Show, Timers, VideoConfig,
    VideoSource, VoiceConfig,
};
use session::{InitKind, Initiation, PacketType, TimestampSource};
use voice::audio::fake::{Fake, Setup};

pub fn timers() -> Timers {
    Timers {
        ping_idle: Duration::from_millis(100),
        reconnecting_after: Duration::from_millis(1000),
        lost_after: Duration::from_millis(3000),
        handshake_fast_retry: Duration::from_millis(100),
        handshake_slow_retry: Duration::from_millis(300),
        stun_wait: Duration::from_millis(100),
        stun_retry: Duration::from_millis(300),
        ..Timers::default()
    }
}

// For what no view shows: asked again every 10 ms until it has an answer.
pub fn poll<T>(limit: Duration, what: &str, mut found: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(it) = found() {
            return it;
        }
        assert!(Instant::now() < deadline, "{what}: not within {limit:?}");
        thread::sleep(Duration::from_millis(10));
    }
}

pub fn config(name: &str, timers: Timers) -> Config {
    config_in(name, timers, data_dir())
}

// For a list kept across rooms, in a Folder.
pub fn config_in(name: &str, timers: Timers, data_dir: PathBuf) -> Config {
    Config {
        data_dir,
        port: 0,
        name: name.to_owned(),
        stun_servers: Vec::new(),
        candidates: Candidates::Fixed(Vec::new()),
        punch_loopback: true,
        timers,
        log: None,
        address_name: None,
        lookup: Lookup::default(),
        watch_addresses: false,
        voice: quiet_voice(),
        video_upload_kbps: room::DEFAULT_VIDEO_UPLOAD_KBPS,
        // No test captures or shows anything unless it asks for the
        // pattern: most play the sharer and the viewer by hand.
        video: VideoConfig {
            source: VideoSource::Hooks,
            show: Show::NoActivate,
            ..VideoConfig::default()
        },
    }
}

// Fake devices, so no test ever opens a real microphone or plays a sound: a
// silent microphone and speakers nobody listens to, at 10 ms periods, since
// these rooms do not talk.
pub fn quiet_voice() -> VoiceConfig {
    let setup = Setup {
        period_frames: 480,
        buffer_frames: 960,
        ..Setup::default()
    };
    VoiceConfig {
        devices: Devices::Fake {
            microphone: Fake::new(setup, &[("mic", "Test microphone")], Some("mic")),
            speakers: Fake::new(setup, &[("out", "Test speakers")], Some("out")),
        },
        ..VoiceConfig::default()
    }
}

// A folder of its own for each room's known list, so tests running side by
// side never read or write each other's. The Member that runs the room
// removes it; a test that keeps a list across rooms makes a Folder instead.
pub fn data_dir() -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "{DATA_PREFIX}{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("make a folder for the known list");
    dir
}

const DATA_PREFIX: &str = "booth-room-data-";

// A folder that outlives the rooms using it, gone when the test ends.
pub struct Folder(pub PathBuf);

impl Folder {
    pub fn new(test: &str) -> Folder {
        let dir = std::env::temp_dir().join(format!("booth-room-{test}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("make a folder for the known lists");
        Folder(dir)
    }
}

impl Drop for Folder {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub fn loopback(port: u16) -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, port))
}

pub struct Member {
    room: Option<Room>,
    pub identity: Arc<Identity>,
    changes: Receiver<()>,
    // Made by data_dir() for this room alone, and removed with it.
    owned_dir: Option<PathBuf>,
}

fn owned(config: &Config) -> Option<PathBuf> {
    let name = config.data_dir.file_name()?.to_str()?;
    name.starts_with(DATA_PREFIX)
        .then(|| config.data_dir.clone())
}

fn watcher() -> (Notify, Receiver<()>) {
    let (tx, rx) = mpsc::channel();
    let notify: Notify = Arc::new(move || {
        let _ = tx.send(());
    });
    (notify, rx)
}

impl Member {
    pub fn host(name: &str, timers: Timers) -> Member {
        Member::host_with(config(name, timers))
    }

    pub fn host_with(config: Config) -> Member {
        Member::host_as(Arc::new(Identity::generate()), config)
    }

    pub fn host_as(identity: Arc<Identity>, config: Config) -> Member {
        let (notify, changes) = watcher();
        let room_name = format!("{}'s room", config.name);
        let owned_dir = owned(&config);
        let room =
            Room::host(config, Arc::clone(&identity), room_name, notify).expect("host starts");
        Member {
            room: Some(room),
            identity,
            changes,
            owned_dir,
        }
    }

    pub fn join(name: &str, timers: Timers, invite: Invite) -> Member {
        Member::join_as(Arc::new(Identity::generate()), name, timers, invite)
    }

    pub fn join_as(identity: Arc<Identity>, name: &str, timers: Timers, invite: Invite) -> Member {
        Member::join_with(config(name, timers), identity, invite)
    }

    pub fn join_with(config: Config, identity: Arc<Identity>, invite: Invite) -> Member {
        let (notify, changes) = watcher();
        let owned_dir = owned(&config);
        let room = Room::join(config, Arc::clone(&identity), invite, notify).expect("join starts");
        Member {
            room: Some(room),
            identity,
            changes,
            owned_dir,
        }
    }

    pub fn rejoin_with(config: Config, identity: Arc<Identity>, known: KnownHost) -> Member {
        let (notify, changes) = watcher();
        let owned_dir = owned(&config);
        let room =
            Room::rejoin(config, Arc::clone(&identity), known, notify).expect("rejoin starts");
        Member {
            room: Some(room),
            identity,
            changes,
            owned_dir,
        }
    }

    pub fn room(&self) -> &Room {
        self.room.as_ref().expect("room is open")
    }

    pub fn view(&self) -> View {
        self.room().view()
    }

    pub fn port(&self) -> u16 {
        self.view().numbers.local_port
    }

    // Wakes on every notify, and every 20 ms as well, so a missed notify
    // shows up as a slow test rather than a hang.
    pub fn wait_for(&self, limit: Duration, what: &str, ok: impl Fn(&View) -> bool) -> View {
        let deadline = Instant::now() + limit;
        loop {
            let view = self.view();
            if ok(&view) {
                return view;
            }
            let now = Instant::now();
            if now >= deadline {
                panic!("{what}: not within {limit:?}. Last view: {view:#?}");
            }
            let _ = self
                .changes
                .recv_timeout((deadline - now).min(Duration::from_millis(20)));
        }
    }

    pub fn holds_for(&self, span: Duration, what: &str, ok: impl Fn(&View) -> bool) {
        let end = Instant::now() + span;
        while Instant::now() < end {
            let view = self.view();
            assert!(ok(&view), "{what}. View: {view:#?}");
            let _ = self.changes.recv_timeout(Duration::from_millis(20));
        }
    }

    pub fn leave(&mut self) -> Duration {
        let room = self.room.take().expect("room is open");
        let started = Instant::now();
        room.leave();
        started.elapsed()
    }
}

// The room goes first: leaving it writes its list for the last time.
impl Drop for Member {
    fn drop(&mut self) {
        drop(self.room.take());
        if let Some(dir) = self.owned_dir.take() {
            let _ = fs::remove_dir_all(dir);
        }
    }
}

// The host's own code has no addresses (Candidates::Fixed of nothing), so
// every test says where the host is.
pub fn invite_to(host: &Member, addr: SocketAddr) -> Invite {
    let view = host.wait_for(Duration::from_secs(2), "invite code", |v| {
        v.invite.as_ref().is_some_and(|i| !i.code.is_empty())
    });
    code_to_invite(&view.invite.expect("invite").code, addr)
}

pub fn code_to_invite(code: &str, addr: SocketAddr) -> Invite {
    let mut invite = Invite::decode(code).expect("host's code decodes");
    invite.candidates.push(Candidate {
        kind: CandidateKind::Lan,
        addr,
    });
    invite
}

pub fn host_invite(host: &Member) -> Invite {
    invite_to(host, loopback(host.port()))
}

// Sits between a client and the host: the client sends to `addr`, the host
// sees packets from `host_side()`, and rebind() moves that to a new port as
// a router does when it forgets a mapping. Seen the other way it is the
// host's router, and move_outside() gives the host a new outside address.
pub struct Forwarder {
    pub addr: SocketAddr,
    shared: Arc<Forwarding>,
    threads: Vec<JoinHandle<()>>,
}

struct Forwarding {
    stop: AtomicBool,
    // Drops everything both ways, as a Wi-Fi blip does.
    blocked: AtomicBool,
    // Drops this percent of packets each way, picked by a seeded sequence
    // per direction.
    loss: AtomicU32,
    lost: AtomicU64,
    client_side: Mutex<Arc<UdpSocket>>,
    host_side: Mutex<Arc<UdpSocket>>,
    host: SocketAddr,
    client: Mutex<Option<SocketAddr>>,
    initiations: Mutex<Vec<Vec<u8>>>,
    from_host: AtomicU64,
    // The length of every datagram toward the host, while asked to keep them.
    sizes: Mutex<Option<Vec<usize>>>,
    // Toward the host, each datagram is held between these two, a different
    // time for each. Order is kept.
    hold_least_us: AtomicU64,
    hold_most_us: AtomicU64,
    // Toward the host, every this many voice packets one is dropped, and
    // how many were dropped and let through since.
    voice_every: AtomicU32,
    voice_dropped: AtomicU64,
    voice_passed: AtomicU64,
    // Voice packets toward the host since the forwarder opened, or since
    // number_voice_from_now, dropped ones included, so the first of them is
    // number 0.
    voice_seen: AtomicU64,
    // While not 0, a block ends at the next voice packet toward the host
    // whose number is a multiple of this, and that packet goes through.
    unblock_every: AtomicU64,
}

// The forwarder sees only sizes, and sizes tell voice apart. On the way to
// the host a control message is 63 bytes at most in these tests (a Periods
// message, or a loss report about one or two talkers or of the video), and
// voice is 67 bytes or more. Voice is also at most 434 bytes: 401 of voice
// (talk::MAX_VOICE), the channel byte and 32 of session overhead. No test
// drops voice while video flows, since a small frame's video packets can be
// as short as voice. A pointer update is 52 and a shape chunk over 1000.
const VOICE_AT_LEAST: usize = 64;
const VOICE_AT_MOST: usize = 401 + 1 + 32;
const DATA: u8 = 0x15;

// xorshift64*, so a run does the same whatever the timing.
fn roll(dice: &mut u64) -> u64 {
    *dice ^= *dice >> 12;
    *dice ^= *dice << 25;
    *dice ^= *dice >> 27;
    dice.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

impl Forwarding {
    fn loses(&self, dice: &mut u64) -> bool {
        let percent = self.loss.load(Ordering::Acquire);
        if percent == 0 {
            return false;
        }
        let lost = roll(dice) % 100 < u64::from(percent);
        if lost {
            self.lost.fetch_add(1, Ordering::Relaxed);
        }
        lost
    }

    fn drops_voice(&self, packet: &[u8]) -> bool {
        let every = self.voice_every.load(Ordering::Acquire);
        if every == 0 || !is_voice(packet) {
            return false;
        }
        let dropped = self.voice_dropped.load(Ordering::Relaxed);
        let passed = self.voice_passed.load(Ordering::Relaxed);
        if (dropped + passed + 1).is_multiple_of(u64::from(every)) {
            self.voice_dropped.fetch_add(1, Ordering::Relaxed);
            true
        } else {
            self.voice_passed.fetch_add(1, Ordering::Relaxed);
            false
        }
    }

    // Numbers a voice packet toward the host, and ends a block that was
    // asked to end on that number.
    fn count_voice(&self, packet: &[u8]) {
        if !is_voice(packet) {
            return;
        }
        let number = self.voice_seen.fetch_add(1, Ordering::Relaxed);
        let every = self.unblock_every.load(Ordering::Acquire);
        if every != 0 && number.is_multiple_of(every) {
            self.unblock_every.store(0, Ordering::Release);
            self.blocked.store(false, Ordering::Release);
        }
    }

    fn delay(&self, dice: &mut u64) -> Duration {
        let (least, most) = (
            self.hold_least_us.load(Ordering::Acquire),
            self.hold_most_us.load(Ordering::Acquire),
        );
        if most == 0 {
            return Duration::ZERO;
        }
        Duration::from_micros(least + roll(dice) % (most.saturating_sub(least) + 1))
    }
}

fn is_voice(packet: &[u8]) -> bool {
    packet.first() == Some(&DATA) && (VOICE_AT_LEAST..=VOICE_AT_MOST).contains(&packet.len())
}

fn open_side() -> UdpSocket {
    open_side_at(loopback(0))
}

fn open_side_at(addr: SocketAddr) -> UdpSocket {
    UdpSocket::bind(addr).expect("bind forwarder socket")
}

// On Windows a read timeout that runs out just as a datagram comes in can
// lose it, so the sockets in these fakes block with no timeout. An empty
// datagram, which nothing real sends, wakes the thread reading one to look
// at its stop flag again, or at a socket that took this one's place.
pub fn wake(socket: &UdpSocket) {
    if let Ok(addr) = socket.local_addr() {
        let _ = socket.send_to(&[], addr);
    }
}

impl Forwarder {
    pub fn new(host: SocketAddr) -> Forwarder {
        Forwarder::at(loopback(0), host)
    }

    // The client sends to `addr`, which can be any loopback address.
    pub fn at(addr: SocketAddr, host: SocketAddr) -> Forwarder {
        let client_side = open_side_at(addr);
        let addr = client_side.local_addr().expect("local addr");
        let shared = Arc::new(Forwarding {
            stop: AtomicBool::new(false),
            blocked: AtomicBool::new(false),
            loss: AtomicU32::new(0),
            lost: AtomicU64::new(0),
            client_side: Mutex::new(Arc::new(client_side)),
            host_side: Mutex::new(Arc::new(open_side())),
            host,
            client: Mutex::new(None),
            initiations: Mutex::new(Vec::new()),
            from_host: AtomicU64::new(0),
            sizes: Mutex::new(None),
            hold_least_us: AtomicU64::new(0),
            hold_most_us: AtomicU64::new(0),
            voice_every: AtomicU32::new(0),
            voice_dropped: AtomicU64::new(0),
            voice_passed: AtomicU64::new(0),
            voice_seen: AtomicU64::new(0),
            unblock_every: AtomicU64::new(0),
        });
        // What is held up waits here for its time, in the order it came.
        // The line ends when the thread feeding it does.
        let (held, due) = mpsc::channel::<(Instant, Vec<u8>)>();
        let delay_line = {
            let shared = Arc::clone(&shared);
            thread::spawn(move || {
                for (at, packet) in due {
                    thread::sleep(at.saturating_duration_since(Instant::now()));
                    let side = Arc::clone(&shared.host_side.lock().unwrap());
                    let _ = side.send_to(&packet, shared.host);
                }
            })
        };
        let toward_host = {
            let shared = Arc::clone(&shared);
            thread::spawn(move || {
                let mut buf = [0u8; 2048];
                let mut dice = 0x9E37_79B9_7F4A_7C15;
                let mut delay_dice = 0x2545_F491_4F6C_DD1D;
                let mut last_due = Instant::now();
                while !shared.stop.load(Ordering::Acquire) {
                    let side = Arc::clone(&shared.client_side.lock().unwrap());
                    let Ok((len @ 1.., from)) = side.recv_from(&mut buf) else {
                        continue;
                    };
                    let packet = &buf[..len];
                    *shared.client.lock().unwrap() = Some(from);
                    shared.count_voice(packet);
                    if shared.blocked.load(Ordering::Acquire)
                        || shared.loses(&mut dice)
                        || shared.drops_voice(packet)
                    {
                        continue;
                    }
                    if packet.first() == Some(&0x11) {
                        shared.initiations.lock().unwrap().push(packet.to_vec());
                    }
                    if let Some(sizes) = shared.sizes.lock().unwrap().as_mut() {
                        sizes.push(len);
                    }
                    let now = Instant::now();
                    let delay = shared.delay(&mut delay_dice);
                    if !delay.is_zero() || last_due > now {
                        last_due = last_due.max(now + delay);
                        let _ = held.send((last_due, packet.to_vec()));
                        continue;
                    }
                    let side = Arc::clone(&shared.host_side.lock().unwrap());
                    let _ = side.send_to(packet, shared.host);
                }
            })
        };
        let toward_client = {
            let shared = Arc::clone(&shared);
            thread::spawn(move || {
                let mut buf = [0u8; 2048];
                let mut dice = 0xD1B5_4A32_D192_ED03;
                while !shared.stop.load(Ordering::Acquire) {
                    let side = Arc::clone(&shared.host_side.lock().unwrap());
                    let Ok((len @ 1.., _)) = side.recv_from(&mut buf) else {
                        continue;
                    };
                    if shared.blocked.load(Ordering::Acquire) || shared.loses(&mut dice) {
                        continue;
                    }
                    shared.from_host.fetch_add(1, Ordering::Relaxed);
                    if let Some(client) = *shared.client.lock().unwrap() {
                        let side = Arc::clone(&shared.client_side.lock().unwrap());
                        let _ = side.send_to(&buf[..len], client);
                    }
                }
            })
        };
        Forwarder {
            addr,
            shared,
            // The delay line last: it ends once the thread feeding it has.
            threads: vec![toward_host, toward_client, delay_line],
        }
    }

    pub fn host_side(&self) -> SocketAddr {
        self.shared
            .host_side
            .lock()
            .unwrap()
            .local_addr()
            .expect("local addr")
    }

    pub fn rebind(&self) -> SocketAddr {
        let fresh = open_side();
        let addr = fresh.local_addr().expect("local addr");
        let old = std::mem::replace(&mut *self.shared.host_side.lock().unwrap(), Arc::new(fresh));
        wake(&old);
        addr
    }

    // The client side moves to a new port and the old one closes, as the
    // host's router does when it comes back with a new outside address.
    // What the host sends toward the client leaves from the new one.
    pub fn move_outside(&self) -> SocketAddr {
        let fresh = open_side();
        let addr = fresh.local_addr().expect("local addr");
        let old = std::mem::replace(
            &mut *self.shared.client_side.lock().unwrap(),
            Arc::new(fresh),
        );
        wake(&old);
        addr
    }

    pub fn block(&self, blocked: bool) {
        self.shared.blocked.store(blocked, Ordering::Release);
    }

    // Voice is told apart by size alone, so anything before a talker starts
    // that is as long as voice would be numbered as voice too. Called just
    // before, it makes the talker's first packet number 0 whatever came
    // before.
    pub fn number_voice_from_now(&self) {
        self.shared.voice_seen.store(0, Ordering::Relaxed);
    }

    // Ends the block at the next voice packet toward the host whose number,
    // counted as voice_seen says, is a multiple of `every`. That packet is
    // the first one through.
    pub fn unblock_at_voice(&self, every: u64) {
        self.shared
            .unblock_every
            .store(every.max(1), Ordering::Release);
    }

    pub fn blocked(&self) -> bool {
        self.shared.blocked.load(Ordering::Acquire)
    }

    pub fn lose(&self, percent: u32) {
        self.shared.loss.store(percent, Ordering::Release);
    }

    // Packets dropped by lose(), both ways together.
    pub fn lost(&self) -> u64 {
        self.shared.lost.load(Ordering::Relaxed)
    }

    // Toward the host, each datagram is held a time of its own between
    // `least` and `most`, and none overtakes another. Zero for both ends it.
    pub fn hold(&self, least: Duration, most: Duration) {
        let shared = &self.shared;
        let most = most.max(least);
        shared
            .hold_least_us
            .store(least.as_micros() as u64, Ordering::Release);
        shared
            .hold_most_us
            .store(most.as_micros() as u64, Ordering::Release);
    }

    // Toward the host, one voice packet in `every` is dropped, the others
    // counted as let through. Zero ends it.
    pub fn drop_voice_every(&self, every: u32) {
        self.shared.voice_every.store(every, Ordering::Release);
    }

    // Voice packets dropped by drop_voice_every, and let through by it.
    pub fn voice_counts(&self) -> (u64, u64) {
        (
            self.shared.voice_dropped.load(Ordering::Relaxed),
            self.shared.voice_passed.load(Ordering::Relaxed),
        )
    }

    // block(false) for another thread to call when the moment comes.
    pub fn unblocker(&self) -> impl Fn() + Send + 'static {
        let shared = Arc::clone(&self.shared);
        move || shared.blocked.store(false, Ordering::Release)
    }

    pub fn initiations(&self) -> Vec<Vec<u8>> {
        self.shared.initiations.lock().unwrap().clone()
    }

    pub fn packets_from_host(&self) -> u64 {
        self.shared.from_host.load(Ordering::Relaxed)
    }

    // From now on, the length of every datagram that goes on toward the host.
    pub fn keep_sizes(&self) {
        *self.shared.sizes.lock().unwrap() = Some(Vec::new());
    }

    // The lengths kept since keep_sizes, and nothing kept from here on.
    pub fn take_sizes(&self) -> Vec<usize> {
        self.shared.sizes.lock().unwrap().take().unwrap_or_default()
    }
}

impl Drop for Forwarder {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        wake(&self.shared.client_side.lock().unwrap());
        wake(&self.shared.host_side.lock().unwrap());
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

// TEST-NET-3: public as far as the invite rules go, and never anyone's real address.
pub const OUTSIDE: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(203, 0, 113, 9), 52000);

// A STUN server on loopback that answers every binding request with OUTSIDE
// after `delay`, or never when there is no delay.
pub struct FakeStun {
    pub addr: SocketAddr,
    socket: Arc<UdpSocket>,
    requests: Arc<AtomicUsize>,
    answered: Arc<AtomicUsize>,
    silent: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl FakeStun {
    pub fn start(delay: Option<Duration>) -> FakeStun {
        FakeStun::answering(delay, |_| OUTSIDE)
    }

    // `seen` turns the address a request came from into the one the answer
    // reports, as the router in between would.
    pub fn answering(
        delay: Option<Duration>,
        seen: impl Fn(SocketAddr) -> SocketAddrV4 + Send + 'static,
    ) -> FakeStun {
        FakeStun::on(Ipv4Addr::LOCALHOST, delay, seen)
    }

    // Mapping is told apart by answers from different server IPs, and all
    // of 127.0.0.0/8 is loopback on Windows.
    pub fn on(
        ip: Ipv4Addr,
        delay: Option<Duration>,
        seen: impl Fn(SocketAddr) -> SocketAddrV4 + Send + 'static,
    ) -> FakeStun {
        let socket = Arc::new(UdpSocket::bind((ip, 0)).unwrap());
        let addr = socket.local_addr().unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let answered = Arc::new(AtomicUsize::new(0));
        let silent = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = thread::spawn({
            let socket = Arc::clone(&socket);
            let requests = Arc::clone(&requests);
            let answered = Arc::clone(&answered);
            let silent = Arc::clone(&silent);
            let stop = Arc::clone(&stop);
            move || {
                let mut buf = [0u8; 1500];
                while !stop.load(Ordering::Acquire) {
                    let Ok((len @ 1.., from)) = socket.recv_from(&mut buf) else {
                        continue;
                    };
                    let Some(answer) = answer(&buf[..len], seen(from)) else {
                        continue;
                    };
                    // Decided before the count goes up, so a test that saw
                    // the count knows what became of that request.
                    let dropped = silent.load(Ordering::Acquire);
                    requests.fetch_add(1, Ordering::Release);
                    let Some(delay) = delay.filter(|_| !dropped) else {
                        continue;
                    };
                    thread::sleep(delay);
                    let _ = socket.send_to(&answer, from);
                    answered.fetch_add(1, Ordering::Release);
                }
            }
        });
        FakeStun {
            addr,
            socket,
            requests,
            answered,
            silent,
            stop,
            thread: Some(thread),
        }
    }

    // Every binding request that came in, answered or not.
    pub fn requests(&self) -> usize {
        self.requests.load(Ordering::Acquire)
    }

    // Counted once the answer has left.
    pub fn answered(&self) -> usize {
        self.answered.load(Ordering::Acquire)
    }

    // A request that comes in while silent is counted and never answered,
    // as behind a router that is still coming back up.
    pub fn silence(&self, silent: bool) {
        self.silent.store(silent, Ordering::Release);
    }
}

impl Drop for FakeStun {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        wake(&self.socket);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn answer(request: &[u8], mapped: SocketAddrV4) -> Option<Vec<u8>> {
    if request.len() != 20 || request[..2] != [0x00, 0x01] {
        return None;
    }
    let mut out = vec![0x01, 0x01, 0x00, 12];
    out.extend_from_slice(&request[4..20]);
    out.extend_from_slice(&[0x00, 0x20, 0x00, 8, 0x00, 0x01]);
    out.extend_from_slice(&(mapped.port() ^ 0x2112).to_be_bytes());
    let cookie = 0x2112_A442u32.to_be_bytes();
    out.extend(mapped.ip().octets().iter().zip(cookie).map(|(b, c)| b ^ c));
    Some(out)
}

pub const NAME: &str = "myroom.example.net";

// Stands in for Windows' resolver: example.net has one nameserver, on
// loopback, and its own answer for NAME is `v4`.
pub struct FakeSystem {
    v4: Ipv4Addr,
    asked: Mutex<Vec<String>>,
}

impl FakeSystem {
    pub fn new(v4: Ipv4Addr) -> Arc<FakeSystem> {
        Arc::new(FakeSystem {
            v4,
            asked: Mutex::new(Vec::new()),
        })
    }

    pub fn asked(&self) -> Vec<String> {
        self.asked.lock().unwrap().clone()
    }
}

impl System for FakeSystem {
    fn nameservers(&self, name: &str) -> Result<Vec<String>, SystemError> {
        self.asked.lock().unwrap().push(format!("NS {name}"));
        Ok(match name {
            "example.net" => vec![String::from("ns1.example.net")],
            _ => Vec::new(),
        })
    }

    fn addresses(&self, name: &str, kind: Kind, fresh: bool) -> Result<Vec<IpAddr>, SystemError> {
        let fresh_word = if fresh { ", cache bypassed" } else { "" };
        self.asked
            .lock()
            .unwrap()
            .push(format!("{kind} {name}{fresh_word}"));
        Ok(match (name, kind) {
            ("ns1.example.net", Kind::A) => vec![IpAddr::V4(Ipv4Addr::LOCALHOST)],
            (NAME, Kind::A) => vec![IpAddr::V4(self.v4)],
            _ => Vec::new(),
        })
    }
}

// The name's own nameserver: answers A for NAME with what point_to() last
// set and AAAA with nothing, both with the authoritative flag, and notes
// when each question came in.
pub struct FakeNameserver {
    pub port: u16,
    socket: Arc<UdpSocket>,
    points_to: Arc<Mutex<Ipv4Addr>>,
    asked: Arc<Mutex<Vec<Instant>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl FakeNameserver {
    pub fn start(v4: Ipv4Addr) -> FakeNameserver {
        let socket = Arc::new(UdpSocket::bind(loopback(0)).expect("bind the nameserver"));
        let port = socket.local_addr().expect("local addr").port();
        let points_to = Arc::new(Mutex::new(v4));
        let asked = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = thread::spawn({
            let socket = Arc::clone(&socket);
            let points_to = Arc::clone(&points_to);
            let asked = Arc::clone(&asked);
            let stop = Arc::clone(&stop);
            move || {
                let mut buf = [0u8; 512];
                while !stop.load(Ordering::Acquire) {
                    let Ok((len @ 1.., from)) = socket.recv_from(&mut buf) else {
                        continue;
                    };
                    asked.lock().unwrap().push(Instant::now());
                    let v4 = *points_to.lock().unwrap();
                    if let Some(answer) = dns_answer(&buf[..len], v4) {
                        let _ = socket.send_to(&answer, from);
                    }
                }
            }
        });
        FakeNameserver {
            port,
            socket,
            points_to,
            asked,
            stop,
            thread: Some(thread),
        }
    }

    // The host's dynamic DNS client updated the name.
    pub fn point_to(&self, v4: Ipv4Addr) {
        *self.points_to.lock().unwrap() = v4;
    }

    pub fn asked(&self) -> Vec<Instant> {
        self.asked.lock().unwrap().clone()
    }
}

impl Drop for FakeNameserver {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        wake(&self.socket);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

// The question copied back, then for an A question one record whose name
// points back at the question's.
fn dns_answer(query: &[u8], v4: Ipv4Addr) -> Option<Vec<u8>> {
    let mut at = 12;
    let mut labels = Vec::new();
    loop {
        let len = usize::from(*query.get(at)?);
        if len == 0 {
            break;
        }
        labels.push(String::from_utf8(query.get(at + 1..at + 1 + len)?.to_vec()).ok()?);
        at += 1 + len;
    }
    let kind = u16::from_be_bytes([*query.get(at + 1)?, *query.get(at + 2)?]);
    if !labels.join(".").eq_ignore_ascii_case(NAME) {
        return None;
    }
    let records: &[Ipv4Addr] = if kind == 1 { &[v4] } else { &[] };
    let mut out = query.get(..2)?.to_vec();
    out.extend_from_slice(&[0x84, 0x00, 0, 1, 0, records.len() as u8, 0, 0, 0, 0]);
    out.extend_from_slice(query.get(12..at + 5)?);
    for ip in records {
        out.extend_from_slice(&[0xC0, 12, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4]);
        out.extend_from_slice(&ip.octets());
    }
    Some(out)
}

pub fn lookup(system: &Arc<FakeSystem>, port: u16, loopback: bool) -> Lookup {
    Lookup {
        system: Arc::clone(system) as Arc<dyn System + Send + Sync>,
        port,
        loopback,
    }
}

// A folder of its own for one side's log, empty.
pub fn fresh_log(file: &str, test: &str, side: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("booth-{file}-{}-{test}-{side}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("make a folder for the log");
    dir.join("booth.log")
}

// The log's text, and its folder gone.
pub fn read_log(path: &PathBuf) -> String {
    let text = fs::read_to_string(path).expect("the log was written");
    let _ = fs::remove_dir_all(path.parent().expect("a folder"));
    text
}

// A strict home router in front of the host, with no port mapped: a packet
// from outside gets in only once the host has sent toward that friend. The
// client sends to `outside`. The host sees the client at `inside`, which is
// the friend's address as far as the host's side can tell, so sending there
// is what opens the way, as the host's outgoing packet would on a real one.
pub struct StrictRouter {
    pub outside: SocketAddr,
    pub inside: SocketAddr,
    shared: Arc<Routing>,
    threads: Vec<JoinHandle<()>>,
}

struct Routing {
    stop: AtomicBool,
    open: AtomicBool,
    dropped: AtomicU64,
    outside: UdpSocket,
    inside: UdpSocket,
    host: SocketAddr,
    client: Mutex<Option<SocketAddr>>,
}

impl StrictRouter {
    pub fn new(host: SocketAddr) -> StrictRouter {
        StrictRouter::at(loopback(0), host)
    }

    // The client sends to `outside`, which can be an address it reached the
    // host at before, through something else.
    pub fn at(outside: SocketAddr, host: SocketAddr) -> StrictRouter {
        let shared = Arc::new(Routing {
            stop: AtomicBool::new(false),
            open: AtomicBool::new(false),
            dropped: AtomicU64::new(0),
            outside: open_side_at(outside),
            inside: open_side(),
            host,
            client: Mutex::new(None),
        });
        let outside = shared.outside.local_addr().expect("local addr");
        let inside = shared.inside.local_addr().expect("local addr");
        let coming_in = {
            let shared = Arc::clone(&shared);
            thread::spawn(move || {
                let mut buf = [0u8; 2048];
                while !shared.stop.load(Ordering::Acquire) {
                    let Ok((len @ 1.., from)) = shared.outside.recv_from(&mut buf) else {
                        continue;
                    };
                    *shared.client.lock().unwrap() = Some(from);
                    if shared.open.load(Ordering::Acquire) {
                        let _ = shared.inside.send_to(&buf[..len], shared.host);
                    } else {
                        shared.dropped.fetch_add(1, Ordering::Relaxed);
                    }
                }
            })
        };
        let going_out = {
            let shared = Arc::clone(&shared);
            thread::spawn(move || {
                let mut buf = [0u8; 2048];
                while !shared.stop.load(Ordering::Acquire) {
                    let Ok((len @ 1.., from)) = shared.inside.recv_from(&mut buf) else {
                        continue;
                    };
                    if from != shared.host {
                        continue;
                    }
                    shared.open.store(true, Ordering::Release);
                    if let Some(client) = *shared.client.lock().unwrap() {
                        let _ = shared.outside.send_to(&buf[..len], client);
                    }
                }
            })
        };
        StrictRouter {
            outside,
            inside,
            shared,
            threads: vec![coming_in, going_out],
        }
    }

    pub fn is_open(&self) -> bool {
        self.shared.open.load(Ordering::Acquire)
    }

    pub fn dropped(&self) -> u64 {
        self.shared.dropped.load(Ordering::Relaxed)
    }
}

impl Drop for StrictRouter {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        wake(&self.shared.outside);
        wake(&self.shared.inside);
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

// Reads `socket` on a thread of its own until `stop` is set, handing each
// packet to `take`. It blocks with no timeout, as the fakes above do, so
// whoever sets `stop` wakes the socket after.
fn read_on_thread(
    socket: Arc<UdpSocket>,
    stop: Arc<AtomicBool>,
    mut take: impl FnMut(&[u8]) + Send + 'static,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut buf = [0u8; 2048];
        while !stop.load(Ordering::Acquire) {
            let Ok((len @ 1.., _)) = socket.recv_from(&mut buf) else {
                continue;
            };
            take(&buf[..len]);
        }
    })
}

// What someone with the host key and no invite can send: an initiation with
// a key of its own and a pre-shared key it can only guess.
fn stranger_initiation(
    identity: &Identity,
    host_key: &[u8; 32],
    stamps: &mut TimestampSource,
    cookie: Option<&[u8; 16]>,
) -> (Initiation, Vec<u8>) {
    Initiation::start_with_cookie(
        &identity.private_bytes(),
        identity.public(),
        host_key,
        &[0; 32],
        InitKind::Known,
        stamps.next_stamp(),
        1,
        cookie,
    )
    .expect("start an initiation")
}

// A stranger flooding the host from `ports` sockets on each of `ips`, at a
// steady `per_second` in all. Under load the host never looks past mac2, so
// each socket sends the same packet over and over. Whatever comes back is
// counted: cookie replies, and anything else.
pub struct Flooder {
    sockets: Vec<Arc<UdpSocket>>,
    stop_sending: Arc<AtomicBool>,
    sending: Option<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
    readers: Vec<JoinHandle<()>>,
    cookie_replies: Arc<AtomicU64>,
    others: Arc<AtomicU64>,
}

impl Flooder {
    pub fn start(
        identity: &Identity,
        host: SocketAddr,
        host_key: &[u8; 32],
        ips: &[Ipv4Addr],
        ports: usize,
        per_second: u32,
    ) -> Flooder {
        let sockets: Vec<Arc<UdpSocket>> = ips
            .iter()
            .flat_map(|&ip| std::iter::repeat_n(ip, ports))
            .map(|ip| Arc::new(UdpSocket::bind((ip, 0)).expect("bind a flood socket")))
            .collect();
        let mut stamps = TimestampSource::new();
        let packets: Vec<Vec<u8>> = sockets
            .iter()
            .map(|_| stranger_initiation(identity, host_key, &mut stamps, None).1)
            .collect();
        let stop = Arc::new(AtomicBool::new(false));
        let cookie_replies = Arc::new(AtomicU64::new(0));
        let others = Arc::new(AtomicU64::new(0));
        let readers = sockets
            .iter()
            .map(|socket| {
                let cookie_replies = Arc::clone(&cookie_replies);
                let others = Arc::clone(&others);
                read_on_thread(Arc::clone(socket), Arc::clone(&stop), move |packet| {
                    let counter = match session::packet_type(packet) {
                        Some(PacketType::CookieReply) => &cookie_replies,
                        _ => &others,
                    };
                    counter.fetch_add(1, Ordering::Release);
                })
            })
            .collect();
        let stop_sending = Arc::new(AtomicBool::new(false));
        let sending = thread::spawn({
            let sockets = sockets.clone();
            let stop_sending = Arc::clone(&stop_sending);
            move || {
                // Paced by the clock rather than by sleeps, which Windows
                // may stretch.
                let started = Instant::now();
                let mut sent = 0u64;
                while !stop_sending.load(Ordering::Acquire) {
                    let due = (started.elapsed().as_secs_f64() * f64::from(per_second)) as u64;
                    while sent < due {
                        let n = (sent % sockets.len() as u64) as usize;
                        let _ = sockets[n].send_to(&packets[n], host);
                        sent += 1;
                    }
                    thread::sleep(Duration::from_millis(2));
                }
            }
        });
        Flooder {
            sockets,
            stop_sending,
            sending: Some(sending),
            stop,
            readers,
            cookie_replies,
            others,
        }
    }

    // Returns once the last packet has left.
    pub fn stop(&mut self) -> Instant {
        self.stop_sending.store(true, Ordering::Release);
        if let Some(sending) = self.sending.take() {
            let _ = sending.join();
        }
        Instant::now()
    }

    pub fn cookie_replies(&self) -> u64 {
        self.cookie_replies.load(Ordering::Acquire)
    }

    pub fn other_replies(&self) -> u64 {
        self.others.load(Ordering::Acquire)
    }
}

impl Drop for Flooder {
    fn drop(&mut self) {
        self.stop();
        self.stop.store(true, Ordering::Release);
        for socket in &self.sockets {
            wake(socket);
        }
        for reader in self.readers.drain(..) {
            let _ = reader.join();
        }
    }
}

// A stranger at an address of its own who does with a cookie what a real
// client does: sends it back in mac2.
pub struct Stranger {
    identity: Arc<Identity>,
    host: SocketAddr,
    host_key: [u8; 32],
    stamps: TimestampSource,
    socket: Arc<UdpSocket>,
    replies: Receiver<Vec<u8>>,
    stop: Arc<AtomicBool>,
    reader: Option<JoinHandle<()>>,
}

impl Stranger {
    pub fn at(
        identity: Arc<Identity>,
        ip: Ipv4Addr,
        host: SocketAddr,
        host_key: [u8; 32],
    ) -> Stranger {
        let socket = Arc::new(UdpSocket::bind((ip, 0)).expect("bind the stranger's socket"));
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, replies) = mpsc::channel();
        let reader = read_on_thread(Arc::clone(&socket), Arc::clone(&stop), move |packet| {
            let _ = tx.send(packet.to_vec());
        });
        Stranger {
            identity,
            host,
            host_key,
            stamps: TimestampSource::new(),
            socket,
            replies,
            stop,
            reader: Some(reader),
        }
    }

    // With mac2 made from `cookie` when there is one. The mac1 is what a
    // cookie reply to it is sealed against.
    pub fn initiation(&mut self, cookie: Option<&[u8; 16]>) -> (Vec<u8>, [u8; 16]) {
        let (initiation, packet) =
            stranger_initiation(&self.identity, &self.host_key, &mut self.stamps, cookie);
        (packet, initiation.mac1())
    }

    pub fn send(&self, packet: &[u8]) {
        let _ = self.socket.send_to(packet, self.host);
    }

    // The cookie in a reply sealed against `mac1`, if one comes within
    // `wait`. Other replies are passed over.
    pub fn cookie(&self, mac1: &[u8; 16], wait: Duration) -> Option<[u8; 16]> {
        let deadline = Instant::now() + wait;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let reply = self.replies.recv_timeout(left).ok()?;
            if let Ok((_, cookie)) = session::read_cookie_reply(&reply, &self.host_key, mac1) {
                return Some(cookie);
            }
        }
    }
}

impl Drop for Stranger {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        wake(&self.socket);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

// A client played by hand, for what a friend's program could send that
// Booth never would. It joins with an invite like anyone, says its name, and
// then sends whatever the test gives it. It answers nothing, so it keeps its
// place in the room only while it keeps sending.
pub struct Hand {
    pub identity: Identity,
    socket: Arc<UdpSocket>,
    host: SocketAddr,
    session: session::Session,
    reliable: channels::Reliable,
    replies: Receiver<Vec<u8>>,
    // What came from the host and was not looked at yet.
    inbox: Vec<(channels::Channel, Vec<u8>)>,
    stop: Arc<AtomicBool>,
    reader: Option<JoinHandle<()>>,
}

impl Hand {
    pub fn join(invite: &Invite, host: SocketAddr, name: &str) -> Hand {
        let identity = Identity::generate();
        let socket = Arc::new(UdpSocket::bind(loopback(0)).expect("bind the hand's socket"));
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, replies) = mpsc::channel();
        let reader = read_on_thread(Arc::clone(&socket), Arc::clone(&stop), move |packet| {
            let _ = tx.send(packet.to_vec());
        });
        let psk = session::invite_psk(&invite.secret, &invite.host_key, identity.public());
        let (mut initiation, packet) = Initiation::start(
            &identity.private_bytes(),
            identity.public(),
            &invite.host_key,
            &psk,
            InitKind::Invite(invite.invite_id),
            TimestampSource::new().next_stamp(),
            7,
        )
        .expect("start an initiation");
        socket.send_to(&packet, host).expect("send the initiation");
        let deadline = Instant::now() + Duration::from_secs(3);
        let session = loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let reply = replies.recv_timeout(left).expect("the host answers");
            if let Ok(session) = initiation.finish(&reply, Instant::now()) {
                break session;
            }
        };
        let mut hand = Hand {
            identity,
            socket,
            host,
            session,
            reliable: channels::Reliable::new(),
            replies,
            inbox: Vec::new(),
            stop,
            reader: Some(reader),
        };
        // The control message Hello: kind 24, the protocol and the version,
        // each little-endian, the name, and no address.
        let mut hello = vec![24];
        let version = invite::VERSION;
        for part in [
            invite::PROTOCOL,
            version.major,
            version.minor,
            version.patch,
        ] {
            hello.extend_from_slice(&part.to_le_bytes());
        }
        hello.push(name.len() as u8);
        hello.extend_from_slice(name.as_bytes());
        hello.push(0);
        hand.say(&hello);
        hand
    }

    // Any bytes as a control message, on the reliable stream as a real
    // client sends one. The host's acks are taken first, so the window
    // never fills; nothing is ever sent twice.
    pub fn say(&mut self, message: &[u8]) {
        self.pump();
        self.reliable
            .send(message)
            .expect("queue a control message");
        while let Some(frame) = self.reliable.poll_transmit(Instant::now(), None) {
            self.send(channels::Channel::Control, &frame);
        }
    }

    // What the host sent since the last look that opens on the session,
    // as channel and payload.
    pub fn received(&mut self) -> Vec<(channels::Channel, Vec<u8>)> {
        self.pump();
        std::mem::take(&mut self.inbox)
    }

    fn pump(&mut self) {
        let mut plain = Vec::new();
        while let Ok(packet) = self.replies.try_recv() {
            if self.session.decrypt(&packet, &mut plain).is_ok()
                && let Ok((channel, payload)) = channels::unframe(&plain)
            {
                if channel == channels::Channel::Control {
                    let _ = self.reliable.receive(payload, Instant::now());
                }
                self.inbox.push((channel, payload.to_vec()));
            }
        }
    }

    pub fn send(&mut self, channel: channels::Channel, payload: &[u8]) {
        let mut plain = Vec::new();
        channels::frame(channel, payload, &mut plain);
        let mut packet = Vec::new();
        self.session
            .encrypt(&plain, &mut packet)
            .expect("encrypt for the host");
        let _ = self.socket.send_to(&packet, self.host);
    }
}

impl Drop for Hand {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        wake(&self.socket);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}
