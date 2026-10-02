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
}

impl MediaPath {
    // A handshake answered from `to` is a round trip there too.
    pub(crate) fn new(to: SocketAddr) -> MediaPath {
        MediaPath { to, checking: None }
    }

    pub(crate) fn to(&self) -> SocketAddr {
        self.to
    }

    // The peer's packets now come from `addr`. True when a ping should go
    // there now, so media follows a real move within a round trip.
    pub(crate) fn moved(&mut self, addr: SocketAddr, at_us: u64) -> bool {
        if addr == self.to {
            self.checking = None;
            return false;
        }
        self.checking = Some((addr, at_us));
        true
    }

    // A pong the link took, from `from`, for the ping sent at `t1`. True
    // when media moved.
    pub(crate) fn answered(&mut self, from: SocketAddr, t1: u64) -> bool {
        match self.checking {
            Some((addr, since)) if addr == from && t1 >= since => {
                self.to = addr;
                self.checking = None;
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
}

impl Link {
    pub(crate) fn new(now: Instant) -> Link {
        Link {
            reliable: Reliable::new(),
            chat: Reliable::new(),
            stats: LinkStats::new(),
            offset: OffsetEstimator::new(),
            next_ping: now,
            next_seq: 0,
            rtt: RttEstimator::new(),
            minute: Minute::new(now),
            last_media: None,
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

    // Both streams: a message that waited out an outage goes one normal
    // timeout from now, not at the end of its backoff.
    pub(crate) fn path_recovered(&mut self, now: Instant) {
        self.reliable.path_recovered(now);
        self.chat.path_recovered(now);
    }

    pub(crate) fn next_timeout(&self) -> Option<Instant> {
        let mut soonest = crate::Soonest(self.reliable.next_timeout());
        soonest.add(self.chat.next_timeout());
        soonest.0
    }

    pub(crate) fn retransmissions(&self) -> u64 {
        self.reliable.counters().retransmissions + self.chat.counters().retransmissions
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

    // Stamped when built, not when the timer pass began, so waiting for the
    // lock and the rest of the pass do not count as round trip.
    pub(crate) fn ping(&mut self, clock: Clock) -> Vec<u8> {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        let sent = Instant::now();
        self.stats.ping_sent(seq, sent);
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
        assert!(!media.answered(home, 2_000));
        assert!(!media.answered(copier, 999));
        assert_eq!(media.to(), home);
        // The friend's next packet takes the address back, and nothing is
        // left to check.
        assert!(!media.moved(home, 1_500));
        assert!(!media.answered(copier, 2_000));
        assert_eq!(media.to(), home);

        // A real move: the ping sent after it is answered from there.
        let new_home: SocketAddr = "198.51.100.7:41000".parse().unwrap();
        assert!(media.moved(new_home, 3_000));
        assert!(media.answered(new_home, 3_000));
        assert_eq!(media.to(), new_home);
        assert!(!media.answered(new_home, 4_000), "once");
    }
}
