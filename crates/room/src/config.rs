use std::fmt;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use net::dns;
use share::Show;

use crate::remote::{Capture, Injector};

#[derive(Clone, Debug)]
pub struct Config {
    pub data_dir: PathBuf,
    pub port: u16,
    pub name: String,
    pub stun_servers: Vec<String>,
    pub candidates: Candidates,
    // Tests on one PC turn this on so a hand-built reply code may name a
    // loopback address. A host otherwise punches only public addresses, the
    // same rule ReplyCode::decode applies.
    pub punch_loopback: bool,
    pub timers: Timers,
    // The diagnostic log file, when one was asked for.
    pub log: Option<PathBuf>,
    // The host's own dynamic DNS name, which every invite carries. A client
    // uses the name in the invite and ignores this.
    pub address_name: Option<String>,
    pub lookup: Lookup,
    // Have Windows say when an address on this PC changes, so a new outside
    // address is looked for at once rather than at the next STUN round.
    // Tests turn it off: a real change on this PC in the middle of one would
    // start a STUN round the test did not plan for.
    pub watch_addresses: bool,
    // The microphone, the speakers and how this PC talks.
    pub voice: crate::talk::VoiceConfig,
    // This PC's video upload setting in kbit/s, 15 000 by default and at
    // most 80 000. A host divides it among the people who watch a share over
    // an internet path; a share never goes past the sharer's own.
    pub video_upload_kbps: u32,
    // Where this PC's share comes from and how its viewer opens.
    pub video: VideoConfig,
}

pub const DEFAULT_VIDEO_UPLOAD_KBPS: u32 = 15_000;
pub const MAX_VIDEO_UPLOAD_KBPS: u32 = 80_000;

#[derive(Clone)]
pub struct VideoConfig {
    pub source: VideoSource,
    // The viewer takes the focus when it opens, since Watch was a click.
    // Tests open it without.
    pub show: Show,
    // Off by default: on, the picture waits for the monitor's refresh, up
    // to one refresh later.
    pub vsync: bool,
    // The loss knob, for testing a share with a friend. It drops a set
    // percentage of the video packets that arrive for the share this PC
    // watches, before its viewer sees them.
    pub loss: Option<LossKnob>,
    // Whether this PC's viewer may take HEVC, when its GPU decodes it. Off,
    // it tells the host it does not, and a share it watches goes in H.264:
    // a test stands for a GPU without HEVC this way.
    pub hevc: bool,
    // What puts a remote controller's input on this PC, from the app: the
    // real one calls SendInput, a test's records. None, and nobody can be
    // allowed to control this PC.
    pub injector: Option<Arc<dyn Injector>>,
    // What reads this PC's keys and mouse while it controls another, from
    // the app: the real one switches the input crate's feed. The viewer
    // says when; None, and only the viewer's own clicks and points go.
    pub capture: Option<Arc<dyn Capture>>,
}

impl Default for VideoConfig {
    fn default() -> VideoConfig {
        VideoConfig {
            source: VideoSource::Screen,
            show: Show::Activate,
            vsync: false,
            loss: None,
            hevc: true,
            injector: None,
            capture: None,
        }
    }
}

impl fmt::Debug for VideoConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VideoConfig")
            .field("source", &self.source)
            .field("show", &self.show)
            .field("vsync", &self.vsync)
            .field("loss", &self.loss)
            .field("hevc", &self.hevc)
            .field("injector", &self.injector.is_some())
            .field("capture", &self.capture.is_some())
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VideoSource {
    // The monitor Room::share names, or the primary one.
    Screen,
    // Capture's test pattern at this size, on the GPU that drives the
    // primary monitor, busy or not (share::Choice). For tests: nothing of
    // the screen is captured.
    Pattern { width: u32, height: u32, busy: bool },
    // No video threads at all: a test plays the sharer and the viewer by
    // hand through Room::sharing and Room::watching.
    Hooks,
}

// Drops `percent` of the arriving video packets, picked at random from
// `seed`, so a run with a friend can be had again with the same drops.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LossKnob {
    pub percent: f64,
    pub seed: u64,
}

impl Config {
    pub(crate) fn upload_kbps(&self) -> u32 {
        self.video_upload_kbps.clamp(1, MAX_VIDEO_UPLOAD_KBPS)
    }
}

// How an address name is looked up. Tests on one PC stand in for the system
// resolver and give the nameservers a port of their own, so nothing leaves
// loopback.
#[derive(Clone)]
pub struct Lookup {
    pub system: Arc<dyn dns::System + Send + Sync>,
    pub port: u16,
    // Lets a name and its nameservers be on loopback, as punch_loopback lets
    // a reply code name it. invite::check_addr refuses loopback otherwise.
    pub loopback: bool,
}

impl Default for Lookup {
    fn default() -> Lookup {
        Lookup {
            system: Arc::new(dns::Windows),
            port: dns::PORT,
            loopback: false,
        }
    }
}

impl Lookup {
    pub(crate) fn check(&self, addr: SocketAddr) -> Result<(), &'static str> {
        if self.loopback && addr.ip().is_loopback() && addr.port() != 0 {
            return Ok(());
        }
        invite::check_addr(addr)
    }
}

impl fmt::Debug for Lookup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Lookup")
            .field("port", &self.port)
            .field("loopback", &self.loopback)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Candidates {
    Discover,
    // Invite::new refuses loopback, so tests hand the host its list instead.
    Fixed(Vec<invite::Candidate>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timers {
    // How often each link is pinged while no media flows on it, which keeps
    // every router mapping alive with margin...
    pub ping_idle: Duration,
    // ...and while this PC sent or heard voice on it in the last 2 s, so
    // the round trip and the trace keep up with the talk.
    pub ping_media: Duration,
    pub reconnecting_after: Duration,
    pub lost_after: Duration,
    pub handshake_fast_retry: Duration,
    pub handshake_slow_retry: Duration,
    // A client with no answer says it is still trying after this long, and
    // shows its reply code.
    pub still_trying_after: Duration,
    pub stun_every: Duration,
    pub stun_wait: Duration,
    // When either side's address changes, a router still coming back up
    // answers nothing, so a check for a new outside address asks STUN again
    // this often while no server answers, for up to a minute.
    pub stun_retry: Duration,
    // How long the first invite waits for STUN and the port mapping together.
    // A mapping that comes later reaches the next invite the user asks for.
    pub first_invite_wait: Duration,
    pub rekey_after: Duration,
    pub reject_after: Duration,
    pub invite_single_use_lifetime: Duration,
    pub invite_multi_use_lifetime: Duration,
    // This many initiations that pass mac1 within one second, from anyone,
    // put the host under load, where it wants a cookie back before it does
    // any key math...
    pub load_initiations: u32,
    // ...until the count has stayed under that for this long.
    pub load_calm: Duration,
    // Cookie replies to every source together. A flood from many or spoofed
    // addresses could otherwise fill the host's upload with them and stall
    // the room's voice and video. Past the cap an initiation without a valid
    // mac2 gets nothing.
    pub cookie_replies_per_second: u32,
    pub cookie_reply_burst: u32,
    // Each listener says how much of every talker it lost this often, the
    // host tells each talker the worst of it as often, and the talker
    // switches on what it is told.
    pub voice_report_every: Duration,
    // Redundancy goes off after this long with no loss reported...
    pub redundancy_off_after: Duration,
    // ...10 ms frames start after this long at 5 percent or more...
    pub repair_after: Duration,
    // ...and stop after this long under 1 percent.
    pub repair_off_after: Duration,
    // A person shows as talking until this long after their last voice
    // packet, unless it said it was the last.
    pub talking_for: Duration,
}

impl Default for Timers {
    fn default() -> Timers {
        Timers {
            ping_idle: Duration::from_secs(1),
            ping_media: Duration::from_millis(100),
            reconnecting_after: Duration::from_secs(3),
            lost_after: Duration::from_secs(15),
            handshake_fast_retry: Duration::from_millis(200),
            handshake_slow_retry: Duration::from_secs(2),
            // 5 s is when a person gives up waiting.
            still_trying_after: Duration::from_secs(5),
            stun_every: Duration::from_secs(20),
            stun_wait: Duration::from_millis(1500),
            stun_retry: Duration::from_secs(2),
            first_invite_wait: Duration::from_secs(3),
            rekey_after: session::REKEY_AFTER,
            reject_after: session::REJECT_AFTER,
            invite_single_use_lifetime: Duration::from_secs(invite::SINGLE_USE_SECS),
            invite_multi_use_lifetime: Duration::from_secs(invite::MULTI_USE_SECS),
            // A friend's ladder tries 5 times a second, once at each address
            // in the invite, and seldom reaches the host at more than two or
            // three of them, so one friend joining is not load.
            load_initiations: 32,
            load_calm: Duration::from_secs(5),
            // A reply is 92 bytes with its headers over IPv4 and 112 over
            // IPv6, so this is at most 0.9 Mbit/s, and a burst is at most
            // 22 KB, which a 20 Mbit/s upload sends in 9 ms.
            cookie_replies_per_second: 1000,
            cookie_reply_burst: 200,
            voice_report_every: Duration::from_secs(1),
            redundancy_off_after: Duration::from_secs(30),
            repair_after: Duration::from_secs(2),
            repair_off_after: Duration::from_secs(30),
            talking_for: Duration::from_millis(200),
        }
    }
}

impl Timers {
    // The same cut Session::with_timers makes, so the deadlines the timer
    // thread waits for match what the sessions will report.
    pub(crate) fn session(&self) -> session::Timers {
        let reject_after = self.reject_after.min(session::REJECT_AFTER);
        session::Timers {
            rekey_after: self.rekey_after.min(session::REKEY_AFTER).min(reject_after),
            reject_after,
        }
    }

    // Both sides ping every ping_idle, so this long without a packet means
    // some were lost, and whatever the control stream sent meanwhile has had
    // its retransmits backed off.
    pub(crate) fn quiet_after(&self) -> Duration {
        self.ping_idle.saturating_mul(2)
    }
}
