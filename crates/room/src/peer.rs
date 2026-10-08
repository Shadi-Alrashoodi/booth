// What both roles keep per link: the sessions, the reliable control stream,
// ping state and counters. The host has one per client, the client one.

use std::net::SocketAddr;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use channels::{
    Channel, OffsetEstimator, PingMessage, Reliable, ReliableError, RttEstimator, clock_sample,
};
use session::{Received, Session, SessionError};
use stats::LinkStats;
use zeroize::Zeroizing;

use crate::config::Timers;
use crate::control::Message;
use crate::minute::Minute;
use crate::socket::Socket;

// Media flows on a link while this PC sent or heard voice on it this
// recently. The same 2 s the strip's voice numbers are counted over.
pub(crate) const MEDIA_FLOWS_FOR: Duration = stats::STREAM_WINDOW;

// Video feedback, recover requests, IDR asks and loss reports, goes on a
// stream of its own (Link::queue_feedback), once the other side says it is
// FEEDBACK_SINCE or later; before that, on the control stream. A watcher
// behind a 2.5 s queue sent 29 recover requests a second, the control
// stream carried 25 with 64 in flight, and every control message waited
// 43 s at the 99th percentile, voice loss reports and room messages too.
pub(crate) const FEEDBACK_SINCE: invite::Version = invite::Version {
    major: 0,
    minor: 2,
    patch: 2,
};

// Feedback waiting on its stream, in flight or queued, past which new
// feedback is held and merged until it drains: recover requests into one
// span, and the newest IDR ask and loss report. A request that waits behind
// a full stream is about frames long gone; merged, the feedback above came
// in 55 to 390 ms at the median and under 3.1 s at the 99th percentile,
// where it took 1 to 34 s and up to 50 s.
const FEEDBACK_WAITING: usize = 4;

// A pong after no pong for this long, with more pings than one unanswered
// in between, ends a loss burst or outage: the control and feedback streams'
// retransmissions that backed off during it go one timeout from now, as
// after a silence (Link::path_recovered). During a call nothing is silent
// for the 2 s a silence takes: a message caught by a 300 ms burst waited
// out its own backoff, up to 260 ms after the burst ended.
const PONG_GAP: Duration = Duration::from_millis(250);
const PINGS_IN_A_GAP: u32 = 3;

// Ping times are monotonic, but counted from the wall clock at the start, so
// the offset between two PCs is their real clock difference and not how far
// apart the two programs were started. A watcher turns a sharer's capture
// time into its own clock with it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Clock {
    epoch: Instant,
    wall_at_epoch: u64,
}

impl Clock {
    pub(crate) fn new(epoch: Instant) -> Clock {
        let wall_at_epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_micros() as u64);
        Clock {
            epoch,
            wall_at_epoch,
        }
    }

    pub(crate) fn micros(&self, at: Instant) -> u64 {
        self.wall_at_epoch + at.saturating_duration_since(self.epoch).as_micros() as u64
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Traffic {
    pub packets_sent: u64,
    pub packets_received: u64,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    // From reliable streams that were started over since.
    pub earlier_retransmits: u64,
}

impl Traffic {
    pub(crate) fn received(&mut self, len: usize) {
        self.packets_received += 1;
        self.bytes_received += len as u64;
    }

    pub(crate) fn send(&mut self, socket: &Socket, packet: &[u8], to: SocketAddr) {
        // A failed send is a dead route or an IPv6 target on an IPv4-only PC.
        // Silence from the peer is what the room acts on, not the error.
        if socket.send_to(packet, to).is_ok() {
            self.packets_sent += 1;
            self.bytes_sent += packet.len() as u64;
        }
    }
}

// Packets dropped before they reached any link. Each one is counted, but the
// view is refreshed for them at most twice a second, so a flood of junk
// cannot keep the panel repainting.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Drops {
    pub bad: u64,
    pub replayed: u64,
    // Host, under load: initiations without a valid mac2, dropped with no
    // key math whether a cookie reply went out or the source's rate limit
    // or the cap on cookie replies held it back.
    pub no_cookie: u64,
    quiet_until: Option<Instant>,
}

impl Drops {
    pub(crate) fn bad(&mut self, now: Instant) -> bool {
        self.bad += 1;
        self.worth_showing(now)
    }

    pub(crate) fn replayed(&mut self, now: Instant) -> bool {
        self.replayed += 1;
        self.worth_showing(now)
    }

    pub(crate) fn no_cookie(&mut self, now: Instant) -> bool {
        self.no_cookie += 1;
        self.worth_showing(now)
    }

    fn worth_showing(&mut self, now: Instant) -> bool {
        if self.quiet_until.is_some_and(|until| now < until) {
            return false;
        }
        self.quiet_until = Some(now + Duration::from_millis(500));
        true
    }
}

#[derive(Default)]
pub(crate) struct Sessions {
    pub current: Option<Session>,
    // Kept after a rekey for packets the peer sent before it switched.
    pub previous: Option<Session>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Which {
    Current,
    Previous,
}

impl Sessions {
    pub(crate) fn which(&self, index: u32) -> Option<Which> {
        if self
            .current
            .as_ref()
            .is_some_and(|s| s.local_index() == index)
        {
            Some(Which::Current)
        } else if self
            .previous
            .as_ref()
            .is_some_and(|s| s.local_index() == index)
        {
            Some(Which::Previous)
        } else {
            None
        }
    }

    pub(crate) fn get_mut(&mut self, which: Which) -> Option<&mut Session> {
        match which {
            Which::Current => self.current.as_mut(),
            Which::Previous => self.previous.as_mut(),
        }
    }

    pub(crate) fn holds(&self, index: u32) -> bool {
        self.which(index).is_some()
    }

    // Returns true when the current session was dropped.
    pub(crate) fn expire(&mut self, now: Instant) -> bool {
        if self.previous.as_ref().is_some_and(|s| s.is_expired(now)) {
            self.previous = None;
        }
        if self.current.as_ref().is_some_and(|s| s.is_expired(now)) {
            self.current = None;
            return true;
        }
        false
    }

    pub(crate) fn clear(&mut self) {
        self.current = None;
        self.previous = None;
    }

    pub(crate) fn deadlines(&self, reject_after: Duration) -> impl Iterator<Item = Instant> + '_ {
        [&self.current, &self.previous]
            .into_iter()
            .flatten()
            .map(move |s| s.created() + reject_after)
    }
}

pub(crate) enum Opened {
    Data(Received),
    Replayed,
    Bad,
}

pub(crate) fn open(session: &mut Session, packet: &[u8], plain: &mut Vec<u8>) -> Opened {
    match session.decrypt(packet, plain) {
        Ok(received) => Opened::Data(received),
        Err(SessionError::Replayed) => Opened::Replayed,
        Err(_) => Opened::Bad,
    }
}

pub(crate) fn send_on(
    socket: &Socket,
    session: &mut Session,
    channel: Channel,
    payload: &[u8],
    to: &[SocketAddr],
    traffic: &mut Traffic,
) {
    // A PeerSecret goes through here on its way out.
    let mut plain = Zeroizing::new(Vec::with_capacity(1 + payload.len()));
    channels::frame(channel, payload, &mut plain);
    let mut packet = Vec::with_capacity(plain.len() + session::DATA_OVERHEAD);
    if session.encrypt(&plain, &mut packet).is_err() {
        return;
    }
    for &addr in to {
        traffic.send(socket, &packet, addr);
    }
}

// Control and pings follow a peer to a new address at once, media only once
// a ping sent there after the move is answered from there. Someone on the
// path who can see and spoof packets can copy a fresh one and send it first
// from an address of their own, which moves the control stream until the
// peer's next packet; to take the voice with it they would have to answer
// that ping as well.
#[derive(Clone, Copy, Debug)]
pub(crate) struct MediaPath {
    to: SocketAddr,
    // Where the peer was last heard from, when that is not `to`, and the
    // ping clock at the move: a pong for a ping sent before it proves
    // nothing about the new address.
    checking: Option<(SocketAddr, u64)>,
    // The seqs of the last pings sent to that address alone. The peer holds
    // the session keys and can seal a pong for any ping it can guess, so
    // these are random, and only someone at that address has seen them.
    probes: [Option<u32>; PROBES],
}

const PROBES: usize = 4;

impl MediaPath {
    // A handshake answered from `to` is a round trip there too.
    pub(crate) fn new(to: SocketAddr) -> MediaPath {
        MediaPath {
            to,
            checking: None,
            probes: [None; PROBES],
        }
    }

    pub(crate) fn to(&self) -> SocketAddr {
        self.to
    }

    // The peer's packets now come from `addr`. True when a ping should go
    // there now, so media follows a real move within a round trip.
    pub(crate) fn moved(&mut self, addr: SocketAddr, at_us: u64) -> bool {
        self.probes = [None; PROBES];
        if addr == self.to {
            self.checking = None;
            return false;
        }
        self.checking = Some((addr, at_us));
        true
    }

    // A ping to `to` checks the new address when it goes there and nowhere
    // else; it is then sent as a Link::probe.
    pub(crate) fn checks(&self, to: &[SocketAddr]) -> bool {
        self.checking.is_some_and(|(addr, _)| to == [addr])
    }

    pub(crate) fn probed(&mut self, seq: u32) {
        self.probes.rotate_right(1);
        self.probes[0] = Some(seq);
    }

    // A pong the link took, from `from`, for ping `seq` sent at `t1`. True
    // when media moved.
    pub(crate) fn answered(&mut self, from: SocketAddr, seq: u32, t1: u64) -> bool {
        match self.checking {
            Some((addr, since))
                if addr == from && t1 >= since && self.probes.contains(&Some(seq)) =>
            {
                self.to = addr;
                self.checking = None;
                self.probes = [None; PROBES];
                true
            }
            _ => false,
        }
    }
}

pub(crate) enum Plain<'a> {
    Ping(PingMessage),
    Control(&'a [u8]),
    Chat(&'a [u8]),
    Feedback(&'a [u8]),
    Voice(&'a [u8]),
    Video(&'a [u8]),
    Cursor(&'a [u8]),
    Input(&'a [u8]),
}

pub(crate) fn read_plain(plain: &[u8]) -> Option<Plain<'_>> {
    match channels::unframe(plain).ok()? {
        (Channel::Ping, payload) => PingMessage::decode(payload).ok().map(Plain::Ping),
        (Channel::Control, payload) => Some(Plain::Control(payload)),
        (Channel::Chat, payload) => Some(Plain::Chat(payload)),
        (Channel::Feedback, payload) => Some(Plain::Feedback(payload)),
        (Channel::Voice, payload) => Some(Plain::Voice(payload)),
        (Channel::Video, payload) => Some(Plain::Video(payload)),
        (Channel::Cursor, payload) => Some(Plain::Cursor(payload)),
        (Channel::Input, payload) => Some(Plain::Input(payload)),
    }
}

pub(crate) struct Link {
    pub reliable: Reliable,
    // Chat has a stream of its own, so a slow control message never holds a
    // line of chat up, or the other way round. It lives as long as the
    // control one: kept across a rekey, started over with a new link.
    pub chat: Reliable,
    // Video feedback's stream, and whether the other side reads it, which
    // its Hello says (FEEDBACK_SINCE). Feedback held while it is backed up.
    pub feedback: Reliable,
    pub feedback_read: bool,
    held: Held,
    pub stats: LinkStats,
    pub offset: OffsetEstimator,
    pub next_ping: Instant,
    next_seq: u32,
    rtt: RttEstimator,
    minute: Minute,
    // The last voice or video packet this link carried either way, as the
    // room saw it under the state lock. This PC's own voice and video leave
    // from threads of their own and are not in it (Talk::sent_lately,
    // Sharing::sent_lately).
    last_media: Option<Instant>,
    // The last pong, and our pings sent since (PONG_GAP).
    last_pong: Option<Instant>,
    pings_since_pong: u32,
}

// Feedback held for the stream (FEEDBACK_WAITING): a recover span from the
// oldest first frame to the newest last, for one share, and the newest IDR
// ask and loss report.
#[derive(Default)]
struct Held {
    recover: Option<(u32, u32, u32)>,
    idr: Option<(u32, Option<u32>)>,
    loss: Option<(u32, Option<u16>)>,
}

impl Held {
    fn is_empty(&self) -> bool {
        self.recover.is_none() && self.idr.is_none() && self.loss.is_none()
    }

    // False for a message that is not feedback.
    fn add(&mut self, message: &Message) -> bool {
        match *message {
            Message::Recover { share, first, last } => {
                self.recover = Some(match self.recover {
                    Some((held, from, _)) if held == share => (share, from, last),
                    _ => (share, first, last),
                });
            }
            Message::Idr { share, seen } => self.idr = Some((share, seen)),
            Message::VideoLoss { share, loss } => self.loss = Some((share, loss)),
            _ => return false,
        }
        true
    }

    fn take(&mut self) -> Vec<Message> {
        let mut out = Vec::new();
        if let Some((share, first, last)) = self.recover.take() {
            out.push(Message::Recover { share, first, last });
        }
        if let Some((share, seen)) = self.idr.take() {
            out.push(Message::Idr { share, seen });
        }
        if let Some((share, loss)) = self.loss.take() {
            out.push(Message::VideoLoss { share, loss });
        }
        out
    }
}

impl Link {
    pub(crate) fn new(now: Instant) -> Link {
        Link {
            reliable: Reliable::new(),
            chat: Reliable::new(),
            feedback: Reliable::new(),
            feedback_read: false,
            held: Held::default(),
            stats: LinkStats::new(),
            offset: OffsetEstimator::new(),
            next_ping: now,
            next_seq: 0,
            rtt: RttEstimator::new(),
            minute: Minute::new(now),
            last_media: None,
            last_pong: None,
            pings_since_pong: 0,
        }
    }

    // Voice or video went through this link, either way. A link that was
    // idle has its next ping brought forward to the media rate, so the
    // faster pings start within one of the media.
    pub(crate) fn media_passed(&mut self, now: Instant, timers: &Timers) {
        self.last_media = Some(now);
        self.next_ping = self.next_ping.min(now + timers.ping_media);
    }

    // Pings go faster while media flows, so the round trip and the trace keep
    // up with it. `sending` is this PC's own voice or video going out on
    // every link lately.
    pub(crate) fn ping_every(&self, sending: bool, now: Instant, timers: &Timers) -> Duration {
        let carried = self
            .last_media
            .is_some_and(|at| now.saturating_duration_since(at) < MEDIA_FLOWS_FOR);
        if sending || carried {
            timers.ping_media
        } else {
            timers.ping_idle
        }
    }

    // A control frame from the peer. Its ack may time some of ours.
    pub(crate) fn receive(&mut self, frame: &[u8], now: Instant) -> Result<(), ReliableError> {
        self.reliable.receive(frame, now)?;
        for delay in self.reliable.ack_delays() {
            self.minute.ack(now, delay);
        }
        Ok(())
    }

    pub(crate) fn receive_chat(&mut self, frame: &[u8], now: Instant) -> Result<(), ReliableError> {
        self.chat.receive(frame, now)?;
        for delay in self.chat.ack_delays() {
            self.minute.ack(now, delay);
        }
        Ok(())
    }

    pub(crate) fn receive_feedback(
        &mut self,
        frame: &[u8],
        now: Instant,
    ) -> Result<(), ReliableError> {
        self.feedback.receive(frame, now)?;
        for delay in self.feedback.ack_delays() {
            self.minute.ack(now, delay);
        }
        Ok(())
    }

    // Every stream: a message that waited out an outage goes one normal
    // timeout from now, not at the end of its backoff.
    pub(crate) fn path_recovered(&mut self, now: Instant) {
        self.reliable.path_recovered(now);
        self.chat.path_recovered(now);
        self.feedback.path_recovered(now);
    }

    pub(crate) fn next_timeout(&self) -> Option<Instant> {
        let mut soonest = crate::Soonest(self.reliable.next_timeout());
        soonest.add(self.chat.next_timeout());
        soonest.add(self.feedback.next_timeout());
        soonest.0
    }

    pub(crate) fn retransmissions(&self) -> u64 {
        self.reliable.counters().retransmissions
            + self.chat.counters().retransmissions
            + self.feedback.counters().retransmissions
    }

    pub(crate) fn ack_delay_ms(&self, now: Instant) -> Option<f32> {
        self.minute
            .ack_delay_avg(now)
            .map(|avg| avg.as_secs_f32() * 1000.0)
    }

    // The log's line about this link, once a minute.
    pub(crate) fn minute_line(&mut self, now: Instant) -> Option<String> {
        let retransmits = self.retransmissions();
        let timer = self.rtt.retransmit_timeout();
        self.minute.line(now, retransmits, timer)
    }

    pub(crate) fn next_minute_line(&self) -> Instant {
        self.minute.next_line()
    }

    pub(crate) fn round_trip(&self, now: Instant) -> Option<share::rate::RoundTrip> {
        self.minute.round_trip(now)
    }

    // How long the oldest of our pings still out has waited for its pong.
    pub(crate) fn unanswered_ms(&self, now: Instant) -> Option<f32> {
        self.stats
            .waiting_for(now)
            .map(|waited| waited.as_secs_f32() * 1000.0)
    }

    // Stamped when built, not when the timer pass began, so waiting for the
    // lock and the rest of the pass do not count as round trip.
    pub(crate) fn ping(&mut self, clock: Clock) -> Vec<u8> {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        self.ping_numbered(seq, clock)
    }

    // A ping for MediaPath to check a new address with, and its seq: random,
    // from the half of the numbers behind the running count, which the
    // peer's ping numbers set aside as a stray. On a new link it is the
    // first ping the peer counts, so the count starts there instead.
    pub(crate) fn probe(&mut self, clock: Clock) -> (Vec<u8>, u32) {
        let seq = if self.next_seq == 0 {
            let seq = u32::from_le_bytes(random());
            self.next_seq = seq.wrapping_add(1);
            seq
        } else {
            let back = 256 + (u32::from_le_bytes(random()) >> 2);
            self.next_seq.wrapping_sub(back)
        };
        (self.ping_numbered(seq, clock), seq)
    }

    fn ping_numbered(&mut self, seq: u32, clock: Clock) -> Vec<u8> {
        let sent = Instant::now();
        self.stats.ping_sent(seq, sent);
        self.pings_since_pong = self.pings_since_pong.saturating_add(1);
        encode(PingMessage::Ping {
            seq,
            t1: clock.micros(sent),
        })
    }

    // The pong is stamped as late as possible so the peer can take our own
    // handling time out of its round trip.
    pub(crate) fn answer(&mut self, seq: u32, t1: u64, now: Instant, clock: Clock) -> Vec<u8> {
        let t2 = clock.micros(now);
        self.stats.peer_ping_received(seq, t1, t2);
        encode(PingMessage::Pong {
            seq,
            t1,
            t2,
            t3: clock.micros(Instant::now()),
        })
    }

    // False when the four timestamps cannot be true.
    pub(crate) fn pong(&mut self, pong: PingMessage, now: Instant, clock: Clock) -> bool {
        let PingMessage::Pong { seq, t1, t2, t3 } = pong else {
            return false;
        };
        let Some(sample) = clock_sample(t1, t2, t3, clock.micros(now)) else {
            return false;
        };
        let rtt = Duration::from_micros(sample.rtt_us.max(0) as u64);
        // An ack pays the peer's time to answer, so the retransmit timer is
        // fed our own clock's round trip, not the network-only one above.
        if let Some(elapsed) = self.stats.pong_received(seq, rtt, now) {
            self.rtt.sample(elapsed);
            self.minute.ping(now, elapsed);
            let gap = self
                .last_pong
                .is_some_and(|at| now.saturating_duration_since(at) > PONG_GAP);
            if gap && self.pings_since_pong >= PINGS_IN_A_GAP {
                self.path_recovered(now);
            }
            self.last_pong = Some(now);
            self.pings_since_pong = 0;
        }
        self.offset.push(sample);
        true
    }

    pub(crate) fn path_changed(&mut self, now: Instant) {
        self.path_recovered(now);
        self.offset.clear();
        self.rtt.clear();
    }

    pub(crate) fn queue(&mut self, message: &Message) {
        // Full means about a thousand messages waiting on a dead path. The
        // silence timers end that link long before this matters.
        let _ = self.reliable.send(&message.encode());
    }

    // An encoded ChatMessage. Full is the same thousand messages on a dead
    // path, and the caller says whether that matters.
    pub(crate) fn queue_chat(&mut self, message: &[u8]) -> Result<(), ReliableError> {
        self.chat.send(message)
    }

    // A recover request, an IDR ask or a loss report: on the feedback stream
    // when the other side reads it, held and merged while that stream is
    // backed up (FEEDBACK_WAITING); otherwise on the control stream.
    pub(crate) fn queue_feedback(&mut self, message: &Message) {
        if !self.feedback_read {
            self.queue(message);
            return;
        }
        if (!self.held.is_empty() || self.feedback_waiting() >= FEEDBACK_WAITING)
            && self.held.add(message)
        {
            return;
        }
        let _ = self.feedback.send(&message.encode());
    }

    fn feedback_waiting(&self) -> usize {
        let counters = self.feedback.counters();
        counters.in_flight + counters.queued
    }

    fn release_held(&mut self) {
        if self.feedback_waiting() < FEEDBACK_WAITING {
            for message in self.held.take() {
                let _ = self.feedback.send(&message.encode());
            }
        }
    }

    pub(crate) fn has_unacked(&self) -> bool {
        self.waiting() > 0
    }

    // Control messages sent and not acked yet, or not sent yet.
    pub(crate) fn waiting(&self) -> usize {
        let counters = self.reliable.counters();
        counters.in_flight + counters.queued
    }

    pub(crate) fn flush(
        &mut self,
        socket: &Socket,
        session: &mut Session,
        to: &[SocketAddr],
        now: Instant,
        traffic: &mut Traffic,
    ) {
        let timeout = self.rtt.retransmit_timeout();
        while let Some(frame) = self.reliable.poll_transmit(now, timeout) {
            let frame = Zeroizing::new(frame);
            send_on(socket, session, Channel::Control, &frame, to, traffic);
        }
        while let Some(frame) = self.chat.poll_transmit(now, timeout) {
            send_on(socket, session, Channel::Chat, &frame, to, traffic);
        }
        self.release_held();
        while let Some(frame) = self.feedback.poll_transmit(now, timeout) {
            send_on(socket, session, Channel::Feedback, &frame, to, traffic);
        }
    }

    // Leaving gets no retransmits, so every frame goes out twice. Chat still
    // waiting then is not sent: the room is over for this PC.
    pub(crate) fn flush_twice(
        &mut self,
        socket: &Socket,
        session: &mut Session,
        to: &[SocketAddr],
        now: Instant,
        traffic: &mut Traffic,
    ) {
        let timeout = self.rtt.retransmit_timeout();
        while let Some(frame) = self.reliable.poll_transmit(now, timeout) {
            let frame = Zeroizing::new(frame);
            send_on(socket, session, Channel::Control, &frame, to, traffic);
            send_on(socket, session, Channel::Control, &frame, to, traffic);
        }
    }

    // A last line for a build that is about to be sent a Bye: the control
    // stream, then the chat, each frame twice, all ahead of the Bye.
    pub(crate) fn flush_all_twice(
        &mut self,
        socket: &Socket,
        session: &mut Session,
        to: &[SocketAddr],
        now: Instant,
        traffic: &mut Traffic,
    ) {
        self.flush_twice(socket, session, to, now, traffic);
        let timeout = self.rtt.retransmit_timeout();
        while let Some(frame) = self.chat.poll_transmit(now, timeout) {
            send_on(socket, session, Channel::Chat, &frame, to, traffic);
            send_on(socket, session, Channel::Chat, &frame, to, traffic);
        }
    }

    pub(crate) fn clock_offset_ms(&self) -> Option<f32> {
        self.offset
            .best()
            .map(|sample| sample.offset_us as f32 / 1000.0)
    }
}

fn encode(message: PingMessage) -> Vec<u8> {
    let mut out = Vec::with_capacity(32);
    message.encode(&mut out);
    out
}

pub(crate) fn random_index(taken: impl Fn(u32) -> bool) -> u32 {
    loop {
        let index = u32::from_le_bytes(random());
        if index != 0 && !taken(index) {
            return index;
        }
    }
}

pub(crate) fn random<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    // getrandom's Windows 10+ backend is ProcessPrng, which cannot fail.
    getrandom::fill(&mut bytes).expect("the Windows random number generator failed");
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    // The pong echoes our send time, so a peer can claim a round trip of
    // years. Fed to the estimator, that held the control stream's timer at
    // its 1 s cap for minutes; our own clock's time cannot be forged.
    #[test]
    fn forged_pong_keeps_timer() {
        let start = Instant::now();
        let clock = Clock::new(start);
        let mut link = Link::new(start);
        let Ok(PingMessage::Ping { seq, .. }) = PingMessage::decode(&link.ping(clock)) else {
            panic!("ping() built something else");
        };
        let arrived = Instant::now() + Duration::from_millis(30);
        let t2 = clock.micros(arrived);
        let forged = PingMessage::Pong {
            seq,
            t1: 0,
            t2,
            t3: t2,
        };
        assert!(link.pong(forged, arrived, clock));
        let timeout = link.rtt.retransmit_timeout().unwrap();
        assert!(
            timeout >= Duration::from_millis(90) && timeout < Duration::from_millis(100),
            "{timeout:?}"
        );

        // A second pong for the same ping is not a second sample.
        assert!(link.pong(forged, arrived + Duration::from_millis(500), clock));
        assert_eq!(link.rtt.retransmit_timeout(), Some(timeout));
    }

    #[test]
    fn media_moves_on_later_pong() {
        let home: SocketAddr = "192.0.2.10:41000".parse().unwrap();
        let copier: SocketAddr = "192.0.2.66:50000".parse().unwrap();
        let mut media = MediaPath::new(home);

        // A copied packet from elsewhere, and pongs that prove nothing: from
        // another address, or for a ping sent before the move.
        assert!(media.moved(copier, 1_000));
        assert!(media.checks(&[copier]));
        media.probed(7);
        assert!(!media.answered(home, 7, 2_000));
        assert!(!media.answered(copier, 7, 999));
        assert_eq!(media.to(), home);
        // The friend's next packet takes the address back, and nothing is
        // left to check.
        assert!(!media.moved(home, 1_500));
        assert!(!media.checks(&[home]));
        assert!(!media.answered(copier, 7, 2_000));
        assert_eq!(media.to(), home);

        // A real move: the ping sent after it is answered from there.
        let new_home: SocketAddr = "198.51.100.7:41000".parse().unwrap();
        assert!(media.moved(new_home, 3_000));
        assert!(
            !media.checks(&[new_home, home]),
            "a ping to both proves nothing"
        );
        media.probed(9);
        assert!(media.answered(new_home, 9, 3_000));
        assert_eq!(media.to(), new_home);
        assert!(!media.answered(new_home, 9, 4_000), "once");
    }

    // Frames over to the other side's feedback stream and its ack back.
    fn carry_feedback(link: &mut Link, other: &mut Reliable, now: Instant) -> Vec<Message> {
        let timeout = Some(Duration::from_millis(100));
        while let Some(frame) = link.feedback.poll_transmit(now, timeout) {
            other.receive(&frame, now).unwrap();
        }
        let mut read = Vec::new();
        while let Some(message) = other.next_delivered() {
            read.push(Message::decode(&message).expect("feedback that decodes"));
        }
        while let Some(ack) = other.poll_transmit(now, timeout) {
            link.receive_feedback(&ack, now).unwrap();
        }
        read
    }

    #[test]
    fn feedback_goes_on_control_until_the_other_side_reads_it() {
        let now = Instant::now();
        let mut link = Link::new(now);
        link.queue_feedback(&Message::Idr {
            share: 3,
            seen: None,
        });
        assert_eq!((link.waiting(), link.feedback_waiting()), (1, 0));

        link.feedback_read = true;
        link.queue_feedback(&Message::Idr {
            share: 3,
            seen: None,
        });
        assert_eq!((link.waiting(), link.feedback_waiting()), (1, 1));
    }

    #[test]
    fn feedback_is_held_and_merged_while_its_stream_is_backed_up() {
        let now = Instant::now();
        let mut link = Link::new(now);
        link.feedback_read = true;
        let mut other = Reliable::new();
        let recover = |first, last| Message::Recover {
            share: 3,
            first,
            last,
        };
        for frame in 0..FEEDBACK_WAITING as u32 {
            link.queue_feedback(&recover(frame, frame));
        }
        link.queue_feedback(&recover(10, 11));
        link.queue_feedback(&Message::Idr {
            share: 3,
            seen: Some(9),
        });
        link.queue_feedback(&recover(14, 16));
        link.queue_feedback(&Message::VideoLoss {
            share: 3,
            loss: Some(120),
        });
        link.queue_feedback(&Message::Idr {
            share: 3,
            seen: Some(13),
        });
        link.release_held();
        assert_eq!(link.feedback_waiting(), FEEDBACK_WAITING);
        assert_eq!(carry_feedback(&mut link, &mut other, now).len(), 4);
        assert_eq!(link.feedback_waiting(), 0);

        // Once drained, what was held goes as three messages: one recover
        // span from the oldest first frame to the newest last, then the
        // newest IDR ask and loss report.
        link.release_held();
        let read = carry_feedback(&mut link, &mut other, now);
        assert!(matches!(
            read.as_slice(),
            [
                Message::Recover {
                    share: 3,
                    first: 10,
                    last: 16
                },
                Message::Idr {
                    share: 3,
                    seen: Some(13)
                },
                Message::VideoLoss {
                    share: 3,
                    loss: Some(120)
                },
            ]
        ));

        // Nothing held, and room on the stream: straight on.
        link.queue_feedback(&recover(20, 20));
        assert_eq!(link.feedback_waiting(), 1);
    }

    // Pings go every few ms during a call, so a pong after a gap with three
    // or more unanswered ends a burst. One ping a second on an idle link
    // never does: there the backoff is what holds retransmissions down.
    #[test]
    fn a_pong_after_a_burst_brings_retransmissions_forward() {
        let ms = Duration::from_millis;
        let start = Instant::now();
        let clock = Clock::new(start);
        let mut link = Link::new(start);
        let ping = |link: &mut Link| match PingMessage::decode(&link.ping(clock)) {
            Ok(PingMessage::Ping { seq, t1 }) => (seq, t1),
            _ => panic!("ping() built something else"),
        };
        let pong = |link: &mut Link, (seq, t1): (u32, u64), at: Instant| {
            let t2 = clock.micros(at);
            assert!(link.pong(
                PingMessage::Pong {
                    seq,
                    t1,
                    t2,
                    t3: t2
                },
                at,
                clock
            ));
        };

        let first = ping(&mut link);
        let heard = Instant::now() + ms(20);
        pong(&mut link, first, heard);

        // A control message caught by the burst: sent and resent five times,
        // its timer doubling to the 2 s cap.
        link.queue(&Message::Idr {
            share: 3,
            seen: None,
        });
        let mut at = heard;
        assert!(link.reliable.poll_transmit(at, Some(ms(100))).is_some());
        for _ in 0..5 {
            at = link.next_timeout().unwrap();
            assert!(link.reliable.poll_transmit(at, Some(ms(100))).is_some());
        }
        let backed_off = link.next_timeout();
        assert_eq!(backed_off, Some(heard + ms(5100)));

        let idle = ping(&mut link);
        let later = Instant::now().max(heard) + ms(1000);
        pong(&mut link, idle, later);
        assert_eq!(link.next_timeout(), backed_off);

        let _ = ping(&mut link);
        let _ = ping(&mut link);
        let last = ping(&mut link);
        let after = Instant::now().max(later) + ms(400);
        pong(&mut link, last, after);
        assert_eq!(link.next_timeout(), Some(after + ms(100)));
    }
}
