use std::fmt;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use voice::audio::{AudioError, Microphone};

pub use share::rate::SteppedDown;

pub use crate::screen::{OwnShare, Refusal};
pub use crate::talk::TalkMode;

// Everything the panel shows, copied out of the room's state after each change.
// The panel holds none of the room's locks while it draws, so a slow frame
// cannot delay a packet. A pasted reply code takes the state lock for its
// checks and nothing more.
#[derive(Clone, Debug)]
pub struct View {
    pub role: Role,
    pub room_name: String,
    pub strip: Strip,
    pub people: Vec<Person>,
    pub invite: Option<InviteView>,
    pub numbers: Numbers,
    // The chat of this room, oldest first, the newest 2000 lines. Shared
    // with the room, so a view costs no copy of it.
    pub chat: Arc<Vec<Arc<ChatLine>>>,
    pub notice: Option<Notice>,
    // Client: the reply code screen, from still_trying_after without an
    // answer until one comes. In a room (people is not empty) it is the code
    // block above the people list: the host has been silent for
    // reconnecting_after and this PC's own outside address changed. It is
    // only ever a Code or Expired there, and goes on the first packet back.
    pub reply: Option<ReplyView>,
    // Host: what came of the last code pasted back, for the line under the
    // paste field.
    pub paste: Option<PasteState>,
    // Host: its own public address changed during this room and it still
    // matters to the panel. Always None on a client.
    pub address_changed: Option<AddressChanged>,
    // The known list could not be used when the room opened: put aside,
    // or left where it was and not written to. Either way the room started
    // with an empty one.
    pub list_problem: Option<crate::known::ListProblem>,
    // Who shares, whether this PC watches, and this PC's own share.
    pub share: ShareView,
    // This PC's own voice: how it talks, the buttons on its row, and why a
    // device is not running.
    pub voice: Voice,
}

// One share at a time, with Watch on the sharer's row.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ShareView {
    pub current: Option<CurrentShare>,
    // This PC watches the current share.
    pub watching: bool,
    // The viewer's window is open: from a moment after Watch until it
    // closes.
    pub viewer_open: bool,
    // This PC's own share: asked for, granted, or refused and why.
    pub own: OwnShare,
    // This PC's own share as its thread runs it: None until the capture and
    // the encoder are open, and again once it stops.
    pub running: Option<RunningShare>,
    // The newest thing that went wrong with this PC's share or its viewer,
    // as the chat's system line says it. A new Share or Watch clears it.
    pub problem: Option<String>,
    // Remote control of the current share, from this PC's side.
    pub control: ControlView,
}

// At most one of asked_by and controlled_by, and one of asking and
// controlling.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ControlView {
    // Who controls the current share, as the room knows it: every panel
    // shows "controlling" on their row, and the host's row menu has End
    // control.
    pub controller: Option<[u8; 32]>,
    // Someone asks to control this PC's share: the request block.
    pub asked_by: Option<ControlRequest>,
    // Someone controls this PC: the indicator, and Stop control.
    pub controlled_by: Option<Party>,
    // An administrator window is in front here, so the controller's input
    // is paused.
    pub admin_here: bool,
    // This PC asked to control this share: the Control button reads Asked.
    pub asking: Option<u32>,
    // This PC controls this person's share: Stop control in place of Share.
    pub controlling: Option<Party>,
    // The shared PC has an administrator window in front: the viewer says
    // control is paused.
    pub paused: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Party {
    pub key: [u8; 32],
    pub name: String,
}

// A request on show until it is answered. Allow and Don't allow name its
// number (Room::answer_control), so a click never answers a request that
// took its place a moment before.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ControlRequest {
    pub number: u32,
    pub key: [u8; 32],
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunningShare {
    // Windows' software encoder: the panel warns that the share runs at up
    // to 1080p60.
    pub software: bool,
    // Windows took the desktop away; the share goes on by itself when it
    // gives it back.
    pub paused: Option<Paused>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Paused {
    // A UAC prompt, the lock screen or Ctrl+Alt+Del.
    SecureDesktop,
    // Other programs hold every copy of the screen Windows allows.
    Taken,
    // The Windows session is disconnected or locked from Remote Desktop.
    Disconnected,
    // Windows is changing the display.
    Changing,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CurrentShare {
    pub key: [u8; 32],
    pub name: String,
    // A new number for every new share.
    pub number: u32,
    pub fps: u8,
    // This PC shares it.
    pub yours: bool,
    // How many watch it: known to the host and, from what the host says, to
    // the sharer.
    pub watchers: Option<u8>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Voice {
    pub mode: TalkMode,
    pub muted: bool,
    pub deafened: bool,
    // Voice is going out now: Hold to talk is held, or open mic hears a
    // voice or is in its tail.
    pub sending: bool,
    // Why the microphone is not running while it should be. The room goes
    // on for listening.
    pub microphone: Option<AudioError>,
    pub speakers: Option<AudioError>,
}

// The host's public address changed mid-session. The panel picks the words.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AddressChanged {
    // Within 2 minutes of the change some friends are still reconnecting or
    // lost: the line under the paste field says a code from a friend will
    // not help.
    pub codes_cannot_help: bool,
    // A friend who was in the room before the change was lost and is not
    // back, within 5 minutes of the change: the host sentence shows.
    pub friends_lost: bool,
    // An address name is set, so the host sentence asks to check that it
    // points to this PC instead of asking to set one.
    pub name_set: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Host,
    Client,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Level {
    #[default]
    Good,
    Warn,
    Bad,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathWord {
    Lan,
    Direct,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TracePoint {
    Rtt(f32),
    Lost,
}

// Where the strip's jitter or loss was taken from: from the voice of the
// person at the other end of the link while they talk, from their video
// while they share and do not talk, and from the pings when neither flows.
// While only video flows, the loss comes from the frames whose first packet
// never came, and the jitter from when those first packets arrived against
// when their frames were encoded: the first packet of a frame leaves as the
// encoder finishes it, spread or not.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Source {
    #[default]
    Pings,
    Voice,
    Video,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum LinkState {
    // A host with nobody connected. The strip stays empty.
    #[default]
    Alone,
    Connecting,
    Live,
    Reconnecting,
    Lost,
    Closed,
}

// The strip follows one link: the client's link to the host, or on the host
// the worst client link (highest round trip).
#[derive(Clone, Debug, Default)]
pub struct Strip {
    pub state: LinkState,
    pub rtt_ms: Option<f32>,
    pub rtt_level: Level,
    pub jitter_ms: Option<f32>,
    pub jitter_level: Level,
    pub jitter_from: Source,
    pub loss_pct: Option<f32>,
    pub loss_level: Level,
    pub loss_from: Source,
    pub path: Option<PathWord>,
    // Oldest first, at most 120 points: one per ping, whichever rate the
    // pings go at.
    pub trace: Vec<TracePoint>,
}

// One message in this room's chat, as this PC showed it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatLine {
    pub author: [u8; 32],
    pub name: String,
    pub text: String,
    // When this PC showed it, for the time of day next to the name.
    pub at_unix_ms: u64,
    // Written on this PC: shown at once, before the host has it.
    pub mine: bool,
    pub kind: LineKind,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LineKind {
    // Someone said it.
    #[default]
    Said,
    // The room says it, time first and in ash: `text` is the whole
    // sentence, and `author` and `name` whom it is about.
    System,
    // A system line in warn: a share or a viewer ran into something, and
    // the sentence says what to do.
    Problem,
}

#[derive(Clone, Debug)]
pub struct Person {
    pub key: [u8; 32],
    pub name: String,
    pub fingerprint: String,
    // Round trip to the host, measured by the host, so every panel shows the same number.
    pub rtt_ms: Option<f32>,
    pub rtt_level: Level,
    pub is_you: bool,
    pub is_host: bool,
    // Joined with an invite during this session, so the panel shows the fingerprint.
    pub joined_by_invite: bool,
    pub reconnecting: bool,
    // Their voice is arriving here, or for this PC, it is going out.
    pub talking: bool,
    pub sharing: bool,
}

#[derive(Clone)]
pub struct InviteView {
    pub code: String,
    pub multi_use: bool,
    pub expires_at_unix: u64,
    pub used: bool,
    pub expired: bool,
    pub router: RouterState,
    // The router opened a port after this invite was made, so a new one
    // would carry an address this one does not.
    pub mapped_since: bool,
    // This PC's public address changed after this invite was made, so the
    // outside addresses in it lead nowhere now.
    pub address_changed_since: bool,
}

// The code carries the invite's secret, so a View printed into a log or a
// panic message must not be something a stranger can join with.
impl fmt::Debug for InviteView {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InviteView")
            .field(
                "code",
                &format_args!("({} characters, hidden)", self.code.len()),
            )
            .field("multi_use", &self.multi_use)
            .field("expires_at_unix", &self.expires_at_unix)
            .field("used", &self.used)
            .field("expired", &self.expired)
            .field("router", &self.router)
            .field("mapped_since", &self.mapped_since)
            .field("address_changed_since", &self.address_changed_since)
            .finish()
    }
}

// What the host learned about its own router. The panel turns it into one sentence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouterState {
    Testing,
    Unknown,
    // STUN gave no outside address and the router mapped no port, so the
    // invite carries no address a friend on the internet can use.
    NoAddress,
    Easy,
    Hard,
    // The router said it mapped the port, and no friend has come in through it yet.
    Mapped,
    MappedVerified,
    // A port mapping cannot cross it, whatever this PC's own router does.
    SecondRouter,
    // This PC's own adapter toward the internet has a carrier-grade NAT
    // address, as on a phone tether: the provider's router is the only one.
    CarrierNat,
}

// The reply code holds no secret, only the client's key and outside address,
// so it prints as it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplyView {
    pub state: ReplyState,
    // Empty unless the state is Code.
    pub code: String,
    pub expires_at_unix: u64,
    // The port the host is bound to, which a port forward would lead to.
    // None when the invite did not show it.
    pub host_port: Option<u16>,
}

// Which reply screen applies. The panel picks the words.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplyState {
    // A code to send back. Behind a second router it is the one thing left
    // that can work over IPv4, and the screen says so.
    Code { second_router: bool },
    // The invite says the host's router changes ports for every connection.
    HostHard,
    // STUN says this PC's router does.
    OwnHard,
    // STUN gave this PC no outside address at all.
    NoAddress,
    // The invite has no outside address and no address name, so the host
    // could punch toward this PC but this PC would not know where to answer.
    HostNoAddress,
    // The code ran out with no handshake. The rung that failed is the second
    // router when the invite named one, easy or unknown mapping otherwise.
    // `punched` when the host's punch packets reached this PC, which says
    // the code was pasted.
    Expired { second_router: bool, punched: bool },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PasteState {
    // The punch packets are going out.
    Sent,
    // The friend it named is in the room.
    Joined,
    // The punches went out and the friend did not come in after them.
    Missed,
    Refused(crate::ReplyRefused),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MappingWord {
    Easy,
    Hard,
    Unknown,
}

// The stats panel. None means not measured yet.
#[derive(Clone, Debug, Default)]
pub struct Numbers {
    // Which link these numbers belong to: on the host, the worst client.
    pub link_name: Option<String>,
    pub rtt_ms: Option<f32>,
    pub rtt_avg_ms: Option<f32>,
    pub rtt_min_ms: Option<f32>,
    pub rtt_max_ms: Option<f32>,
    pub rtt_p95_ms: Option<f32>,
    // The strip's jitter and loss. From pings, the loss is of this PC's own
    // pings there and back over the last 100 of them, so it agrees with
    // the gaps in the trace; from voice, of the voice of the person at the
    // other end over the last 2 s.
    pub jitter_ms: Option<f32>,
    pub jitter_from: Source,
    pub loss_pct: Option<f32>,
    pub loss_from: Source,
    // Of the peer's last 100 pings to this PC.
    pub inbound_loss_pct: Option<f32>,
    pub path: Option<PathWord>,
    pub peer_addr: Option<SocketAddr>,
    pub local_port: u16,
    pub handshake_ms: Option<f32>,
    pub connect_ms: Option<f32>,
    pub clock_offset_ms: Option<f32>,
    pub session_age: Option<Duration>,
    pub rekeys: u32,
    // The one in use on the link: faster while voice flows on it.
    pub ping_interval: Duration,
    pub packets_sent: u64,
    pub packets_received: u64,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub dropped_replay: u64,
    pub dropped_bad: u64,
    // Host, under load: the cookie replies it sent, and the initiations it
    // dropped with no key math for want of a valid mac2. Zero on a client,
    // and on a host that was never under load.
    pub cookie_replies: u64,
    pub dropped_no_cookie: u64,
    // From a control or chat message's first send to its ack, averaged over
    // the last minute; messages that were resent are left out.
    pub ack_delay_ms: Option<f32>,
    pub retransmits: u64,
    pub mapping: Option<MappingWord>,
    pub public_addr: Option<SocketAddr>,
    // "PCP", "NAT-PMP" or "UPnP" while the router holds a port for the host.
    pub mapping_protocol: Option<String>,
    // The outside address and port the router says it opened.
    pub mapped_addr: Option<SocketAddr>,
    // Host: its own address name. Client: the one the invite carries.
    pub address_name: Option<NameView>,
    // The last address change this side saw.
    pub address_change: Option<AddressChange>,
    // Reconnect time: from the last packet before a silence of
    // reconnecting_after or more to the first packet after it, for the link
    // that came back last. A client counts a return through a new handshake
    // too, and the host a friend it had let go who came back.
    pub reconnect_ms: Option<f32>,
    // A message from someone else, from when its author sent it to when it
    // arrived here: the newest, and the average over the last minute. The
    // client measures what the host hands on, the host what clients say.
    // Messages from this PC are not measured.
    pub chat_delivery_last_ms: Option<f32>,
    pub chat_delivery_avg_ms: Option<f32>,
    // Taken over a link with more than 5 ms of jitter, where the clock offset
    // behind the number can be off by as much.
    pub chat_delivery_about: bool,
    // Voice. This PC's audio periods as its streams opened them, and the
    // render side's latency: a period, what Windows adds, and what Booth
    // keeps queued.
    pub audio_in_ms: Option<f32>,
    pub audio_out_ms: Option<f32>,
    pub render_latency_ms: Option<f32>,
    // The microphone this PC has open, while it is open.
    pub microphone: Option<Microphone>,
    // The same numbers from the far side, as it reported them: the host's
    // on a client, on the host those of the link these numbers follow.
    pub far_audio_in_ms: Option<f32>,
    pub far_audio_out_ms: Option<f32>,
    pub far_render_latency_ms: Option<f32>,
    // How this PC sends: 5 or 10 ms frames, and whether each 5 ms frame
    // carries a copy of the one before.
    pub send_frame_ms: u32,
    pub send_repair_copy: bool,
    // The jitter buffer of whoever was heard most recently.
    pub buffer: Option<Buffer>,
    // Of the frames each person heard lately sent over the last 2 s, the
    // share that was lost: on the host the worst any listener reported, on
    // a client what this PC lost.
    pub voice_loss: Vec<(String, VoiceLoss)>,
    // The worst any listener reported for this PC's own voice.
    pub own_voice_loss: Option<VoiceLoss>,
    pub mouth_to_ear: Option<MouthToEar>,
    // Frames of this PC's voice sent, and voice packets from others that
    // did not parse and were dropped.
    pub voice_sent: u64,
    pub voice_dropped: u64,
    pub capture_callback: Option<Spread>,
    pub render_callback: Option<Spread>,
    // How late the room's timer thread woke for its deadlines, over its last
    // thousand or so waits that a deadline ended.
    pub timer_late: Option<Spread>,
    // Video. Packets of this PC's own share that went out, each counted
    // once however many links took it.
    pub video_sent: u64,
    // Host: video and pointer packets it passed on to watchers, and what of
    // a friend's sharing it dropped for being over their limits: video,
    // pointers, shape chunks, and asks to share, watch, recover, send an IDR
    // or control. Input over the limit is counted with control's numbers.
    pub video_relayed: u64,
    pub video_dropped: u64,
    // Watching: packets the viewer had not taken yet when more came, past
    // what the inbox holds, and dropped.
    pub video_overflow: u64,
    // What the bitrate rule gives this PC's own share, while it shares.
    pub share_rate_kbps: Option<u32>,
    // The stats panel's Video group: while this PC's share runs, and while
    // it watches one.
    pub sharing: Option<SharingNumbers>,
    pub watching: Option<WatchingNumbers>,
    // Remote control, both ways, since the room opened.
    pub control: ControlNumbers,
}

// What remote control costs, from viewer capture to sharer inject, and the
// part on this PC, which has to stay under 5 ms. The latencies are over the
// last 10 s of input on this PC, the one controlled; the counts since the
// room opened.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ControlNumbers {
    // From the controller's capture to the injector's call, through the
    // clock offsets, "about" when either was taken over a jittery link.
    pub capture_to_inject: Option<Latency>,
    // From the packet's arrival here to the injector's call.
    pub receive_to_inject: Option<Latency>,
    // How long the injector's call took.
    pub inject_call: Option<Latency>,
    // Events that went to SendInput.
    pub injected: u64,
    pub dropped: InputDrops,
    // Times CUTOFF passed without a packet and everything held went up.
    pub cutoffs: u64,
    // Input packets this PC sent as the controller.
    pub packets_sent: u64,
}

// Input dropped, by why. Past the host's own limit, the rest are what the
// injector on the PC controlled refused.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InputDrops {
    // The block list: the panic chord, this PC's hotkeys, Win+L, and
    // anything aimed at Booth's own windows.
    pub blocked: u64,
    // Key events past about 40 a second.
    pub over_rate: u64,
    // The owner's own keyboard or mouse was in use.
    pub local: u64,
    // After the panic key.
    pub cut: u64,
    // An administrator window was in front.
    pub paused: u64,
    // Packets older than one already taken.
    pub late: u64,
    // Events that waited here past remote::STALE for the injector, still
    // busy with ones before them: dropped, never injected late.
    pub stale: u64,
    // Host: a friend's packets past INPUT_PER_SECOND and its burst.
    pub host_over_rate: u64,
}

// This PC's share, once a second.
#[derive(Clone, Debug, PartialEq)]
pub struct SharingNumbers {
    pub encoder: String,
    // Windows' software encoder, which caps a share at 1080p60: in warn.
    pub software: bool,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    // In warn while encode time is past the frame interval.
    pub fps_level: Level,
    // Frames encoded in the last second: fewer than fps on a still screen.
    pub encoded_fps: u32,
    // Over the last second; None when no frame was encoded in it.
    pub encode_ms: Option<Latency>,
    // The encoder's output over the last second, without parity, against
    // the rate in use; and what the bitrate rule allows before any backoff.
    pub video_kbps: u32,
    pub rate_kbps: u32,
    pub allowed_kbps: u32,
    // The rate the encoder took: the rate in use unless the encoder
    // refused a change, which the log says.
    pub encoder_kbps: u32,
    // How often the backoff cut the rate since the share started.
    pub backoffs: u32,
    pub parity_pct: u32,
    pub idrs_last_minute: u32,
    pub invalidations_last_minute: u32,
    // Since the share started, the first one included.
    pub idrs: u64,
    // Frames the send thread let go since the share started, because the
    // next was ready before they went out: a busy PC, not the network. The
    // encoder recovered each.
    pub let_go: u64,
    // Of those, IDRs: each was made again at once, and `idrs` counts both.
    pub idrs_let_go: u64,
    pub stepped_down: Option<SteppedDown>,
    // Everything this PC sent over the last second, voice and control
    // included, with IP and UDP headers: what the upload setting has to
    // leave room for.
    pub upload_kbps: u32,
}

// The share this PC watches, once a second.
#[derive(Clone, Debug, PartialEq)]
pub struct WatchingNumbers {
    // On the GPU, over the last second.
    pub decode_ms: Option<f32>,
    pub decode_level: Level,
    // Over the last 10 s, with "about" when either clock offset behind it
    // was taken over a jittery link.
    pub capture_to_display: Option<Latency>,
    pub capture_to_display_level: Level,
    // Frames shown in the last second.
    pub fps: u32,
    // Of the share's packets over the last 2 s, the number that sets the
    // sharer's parity.
    pub video_loss_pct: Option<f32>,
    // Since watching started: frames shown, frames parity put back
    // together, and frames lost for good.
    pub shown: u64,
    pub repaired: u64,
    pub dropped: u64,
    // Frames that came whole but did not decode, and frames that came whole
    // before the first IDR and so could not.
    pub decode_failed: u64,
    pub before_first_idr: u64,
    // Flip when DWM stepped aside, composed when it did not.
    pub present_path: Option<PresentPath>,
    // Only when the loss knob is set.
    pub knob: Option<KnobNumbers>,
    // The codec of the last picture decoded; None before the first.
    pub codec: Option<Codec>,
}

// A share's video codec: HEVC when every watcher's GPU decodes it and the
// sharer's encodes it, H.264 otherwise.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    H264,
    Hevc,
}

impl fmt::Display for Codec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Codec::H264 => "H.264",
            Codec::Hevc => "HEVC",
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Latency {
    pub median_ms: f32,
    pub p95_ms: f32,
    pub about: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PresentPath {
    Flip,
    Composed,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct KnobNumbers {
    pub percent: f64,
    pub seed: u64,
    // Packets it dropped since watching started.
    pub dropped: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AddressChange {
    pub at_unix: u64,
    // This PC's own outside address changed, as STUN saw it: always on the
    // host. False on a client that followed the host to a new address.
    pub this_pc: bool,
    pub from: SocketAddr,
    pub to: SocketAddr,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NameView {
    pub name: String,
    pub answer: NameAnswer,
    // Host only, once the name gave an IPv4 address and STUN or the router
    // gave the outside one.
    pub outside: Option<NameMatch>,
}

// What looking up the name gave. The host looks up its own when the room
// opens; a client looks up the invite's once the fast round went unanswered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NameAnswer {
    // Client: an invite address answered first, or the fast round is not
    // over yet.
    NotAsked,
    Looking,
    // IPv4 first. Refused holds what invite::check_addr turned down, and why;
    // nothing is ever sent there.
    Found {
        addrs: Vec<IpAddr>,
        refused: Vec<(IpAddr, &'static str)>,
    },
    NoSuchName,
    NoAddress,
    // Neither the name's nameservers nor the system resolver answered.
    Unanswered,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NameMatch {
    // The name's IPv4 address: the one that is this PC's, when one of
    // several is.
    pub points_to: Ipv4Addr,
    // This PC's outside address, as STUN saw it, or the router when STUN
    // has not answered.
    pub outside: Ipv4Addr,
}

impl NameMatch {
    pub fn is_this_pc(&self) -> bool {
        self.points_to == self.outside
    }
}

// States the panel explains in a sentence. The words live in the app, not here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Notice {
    // Client: 5 s without an answer from any candidate.
    StillTrying,
    // Client: 15 s of silence from the host.
    LostHost,
    // Client: in place of LostHost when the host's address name gave a new
    // address and nothing answered there within lost_after of the first try.
    // The tries go on underneath either way.
    HostMoved,
    // Client: the host closed the room.
    RoomClosed,
    // Client: the invite had expired before joining.
    InviteExpired,
    // Either side: this PC's socket stopped receiving (the network stack
    // failed under it). The room has stopped; leaving and starting again is
    // the way out.
    SocketFailed,
    // Client: the host's Hello is of another protocol, so this PC left.
    OtherVersion {
        protocol: u16,
        version: invite::Version,
    },
    // Client: the host sent something before any Hello, which only test
    // builds from before version numbers do, so this PC left.
    UnversionedHost,
}

// Of a talker's frames sent over the last 2 s, the share lost, in percent:
// all of it, and what was lost one or two frames in a row. Only that part
// turns on the repair copy and the 10 ms mode, since a longer run is an
// outage neither can repair.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct VoiceLoss {
    pub all_pct: f32,
    pub scattered_pct: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Buffer {
    pub name: String,
    pub ms: u32,
    pub frames: u32,
}

// From the capture of a frame's first sample to the moment it leaves the
// speaker, through the host's clock like chat delivery.
#[derive(Clone, Debug, PartialEq)]
pub struct MouthToEar {
    pub name: String,
    pub last_ms: f32,
    // Over the last 10 s.
    pub avg_ms: f32,
    pub p95_ms: f32,
    // Taken over a link with more than 5 ms of jitter.
    pub about: bool,
}

// How long an audio callback took, over the last thousand or so.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Spread {
    pub median_ms: f32,
    pub p99_ms: f32,
    pub count: usize,
}
