use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use snow::StatelessTransportState;

use crate::SessionError;
use crate::packet::{self, DATA_HEADER_LEN, MAX_PLAINTEXT_LEN, TAG_LEN};
use crate::replay::ReplayWindow;

pub const REKEY_AFTER: Duration = Duration::from_secs(120);
pub const REJECT_AFTER: Duration = Duration::from_secs(180);
pub const REKEY_AFTER_MESSAGES: u64 = 1 << 60;
pub const REJECT_AFTER_MESSAGES: u64 = u64::MAX - (1 << 13);

// For tests that cannot wait minutes. Session::with_timers only ever shortens the defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timers {
    pub rekey_after: Duration,
    pub reject_after: Duration,
}

impl Default for Timers {
    fn default() -> Timers {
        Timers {
            rekey_after: REKEY_AFTER,
            reject_after: REJECT_AFTER,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Received {
    pub counter: u64,
    // The peer's address may follow a packet only when this is true.
    pub newest: bool,
}

pub struct Session {
    // Shared with every Sealer made from this session, so the audio threads
    // send under the same keys and the same counter without the lock the
    // session itself lives under.
    transport: Arc<StatelessTransportState>,
    local_index: u32,
    remote_index: u32,
    remote_public: [u8; 32],
    created: Instant,
    initiator: bool,
    confirmed: bool,
    send_counter: Arc<AtomicU64>,
    window: ReplayWindow,
    timers: Timers,
}

// Encrypts for one session from any thread: what a capture thread holds so
// that sending a voice frame costs the encryption and nothing more. Every
// packet takes its counter from the one the session uses, so no two packets
// ever share a nonce, whichever of them sends. A rekey makes a new session
// and with it a new Sealer; one made before it seals under keys the peer
// still keeps as its previous session until the room hands the new one over.
#[derive(Clone)]
pub struct Sealer {
    transport: Arc<StatelessTransportState>,
    remote_index: u32,
    counter: Arc<AtomicU64>,
}

impl Sealer {
    pub fn remote_index(&self) -> u32 {
        self.remote_index
    }

    // As Session::encrypt. `out` keeps its capacity; a caller that reuses it
    // sends without allocating.
    pub fn seal(&self, plaintext: &[u8], out: &mut Vec<u8>) -> Result<(), SessionError> {
        seal(
            &self.transport,
            self.remote_index,
            &self.counter,
            plaintext,
            out,
        )
    }
}

impl fmt::Debug for Sealer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sealer")
            .field("remote_index", &self.remote_index)
            .field("counter", &self.counter.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

// The counter is spent before use: a failure after it can never lead to the
// same nonce twice. It never moves past the reject limit either, and a
// session at the limit stays there however many threads send on it.
fn seal(
    transport: &StatelessTransportState,
    remote_index: u32,
    counter: &AtomicU64,
    plaintext: &[u8],
    out: &mut Vec<u8>,
) -> Result<(), SessionError> {
    out.clear();
    if plaintext.len() > MAX_PLAINTEXT_LEN {
        return Err(SessionError::TooLarge);
    }
    let counter = counter
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |next| {
            (next < REJECT_AFTER_MESSAGES).then_some(next + 1)
        })
        .map_err(|_| SessionError::Expired)?;

    packet::write_data_header(out, remote_index, counter);
    out.resize(DATA_HEADER_LEN + plaintext.len() + TAG_LEN, 0);
    let sealed = out
        .get_mut(DATA_HEADER_LEN..)
        .ok_or(SessionError::Encrypt)
        .and_then(|body| {
            transport
                .write_message(counter, plaintext, body)
                .map_err(|_| SessionError::Encrypt)
        });
    if let Err(err) = sealed {
        out.clear();
        return Err(err);
    }
    Ok(())
}

impl Session {
    pub(crate) fn new(
        transport: StatelessTransportState,
        local_index: u32,
        remote_index: u32,
        remote_public: [u8; 32],
        now: Instant,
    ) -> Session {
        let initiator = transport.is_initiator();
        Session {
            transport: Arc::new(transport),
            local_index,
            remote_index,
            remote_public,
            created: now,
            initiator,
            // The host's response already proved that the host holds the keys.
            confirmed: initiator,
            send_counter: Arc::new(AtomicU64::new(0)),
            window: ReplayWindow::new(),
            timers: Timers::default(),
        }
    }

    // Longer values are cut back to the defaults: a longer life would stretch how much traffic one
    // stolen key opens. A rekey never waits past the point the session is dropped.
    pub fn with_timers(mut self, timers: Timers) -> Session {
        let reject_after = timers.reject_after.min(REJECT_AFTER);
        self.timers = Timers {
            rekey_after: timers.rekey_after.min(REKEY_AFTER).min(reject_after),
            reject_after,
        };
        self
    }

    pub fn local_index(&self) -> u32 {
        self.local_index
    }

    pub fn remote_index(&self) -> u32 {
        self.remote_index
    }

    pub fn remote_public(&self) -> [u8; 32] {
        self.remote_public
    }

    pub fn created(&self) -> Instant {
        self.created
    }

    pub fn is_initiator(&self) -> bool {
        self.initiator
    }

    pub fn is_confirmed(&self) -> bool {
        self.confirmed
    }

    pub fn encrypt(&mut self, plaintext: &[u8], out: &mut Vec<u8>) -> Result<(), SessionError> {
        out.clear();
        // WireGuard's rule: until the initiator has sent something under these keys, the host
        // cannot know the initiator finished the handshake, so the host never speaks first.
        if !self.confirmed {
            return Err(SessionError::NotConfirmed);
        }
        seal(
            &self.transport,
            self.remote_index,
            &self.send_counter,
            plaintext,
            out,
        )
    }

    // None until the session may send, by the same rule as encrypt.
    pub fn sealer(&self) -> Option<Sealer> {
        self.confirmed.then(|| Sealer {
            transport: Arc::clone(&self.transport),
            remote_index: self.remote_index,
            counter: Arc::clone(&self.send_counter),
        })
    }

    pub fn decrypt(&mut self, packet: &[u8], out: &mut Vec<u8>) -> Result<Received, SessionError> {
        out.clear();
        let data = packet::parse_data(packet).ok_or(SessionError::Malformed)?;
        if data.receiver_index != self.local_index {
            return Err(SessionError::WrongIndex);
        }
        if data.counter >= REJECT_AFTER_MESSAGES {
            return Err(SessionError::CounterLimit);
        }
        if !self.window.check(data.counter) {
            return Err(SessionError::Replayed);
        }

        out.resize(data.ciphertext.len().saturating_sub(TAG_LEN), 0);
        if self
            .transport
            .read_message(data.counter, data.ciphertext, out)
            .is_err()
        {
            out.clear();
            return Err(SessionError::Decrypt);
        }

        let newest = self
            .window
            .greatest()
            .is_none_or(|greatest| data.counter > greatest);
        if !self.window.update(data.counter) {
            out.clear();
            return Err(SessionError::Replayed);
        }
        self.confirmed = true;
        Ok(Received {
            counter: data.counter,
            newest,
        })
    }

    // Only the initiator starts a rekey, as in WireGuard; the host just answers the new handshake.
    pub fn needs_rekey(&self, now: Instant) -> bool {
        self.initiator
            && (self.age(now) >= self.timers.rekey_after || self.sent() >= REKEY_AFTER_MESSAGES)
    }

    // The caller must drop an expired session: encrypt and decrypt do not look at the clock.
    pub fn is_expired(&self, now: Instant) -> bool {
        self.age(now) >= self.timers.reject_after || self.sent() >= REJECT_AFTER_MESSAGES
    }

    fn sent(&self) -> u64 {
        self.send_counter.load(Ordering::Relaxed)
    }

    fn age(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.created)
    }
}

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session")
            .field("local_index", &self.local_index)
            .field("remote_index", &self.remote_index)
            .field("initiator", &self.initiator)
            .field("confirmed", &self.confirmed)
            .field("send_counter", &self.sent())
            .field("window", &self.window)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{InitKind, Initiation, Tai64N, read_initiation};
    use snow::Builder;

    fn keypair() -> ([u8; 32], [u8; 32]) {
        let pair = Builder::new(crate::NOISE_PATTERN.parse().expect("pattern parses"))
            .generate_keypair()
            .expect("keypair");
        (
            pair.private.try_into().expect("32 byte key"),
            pair.public.try_into().expect("32 byte key"),
        )
    }

    fn pair() -> (Session, Session) {
        let (client_private, client_public) = keypair();
        let (host_private, host_public) = keypair();
        let psk = [7u8; 32];
        let now = Instant::now();
        let (mut initiation, packet) = Initiation::start(
            &client_private,
            &client_public,
            &host_public,
            &psk,
            InitKind::Known,
            Tai64N::now(),
            1,
        )
        .expect("start");
        let incoming = read_initiation(&host_private, &host_public, &packet).expect("read");
        let (host, response) = incoming.accept(&psk, 2, now).expect("accept");
        let client = initiation.finish(&response, now).expect("finish");
        (client, host)
    }

    #[test]
    fn send_stops_at_the_reject_limit() {
        let (mut client, mut host) = pair();
        let mut packet = Vec::new();
        let mut plain = Vec::new();

        client
            .send_counter
            .store(REJECT_AFTER_MESSAGES - 1, Ordering::Relaxed);
        client
            .encrypt(b"last", &mut packet)
            .expect("last counter is usable");
        let received = host.decrypt(&packet, &mut plain).expect("decrypts");
        assert_eq!(received.counter, REJECT_AFTER_MESSAGES - 1);
        assert_eq!(plain, b"last");

        assert_eq!(
            client.encrypt(b"one too many", &mut packet),
            Err(SessionError::Expired)
        );
        assert!(packet.is_empty());
        assert!(client.is_expired(client.created()));

        // A sealer made from it is held to the same limit, and trying does
        // not move the counter past it.
        let sealer = client.sealer().expect("the initiator may send");
        assert_eq!(
            sealer.seal(b"from another thread", &mut packet),
            Err(SessionError::Expired)
        );
        assert!(packet.is_empty());
        assert_eq!(client.sent(), REJECT_AFTER_MESSAGES);
    }

    #[test]
    fn rekey_after_message_count() {
        let (client, host) = pair();
        let now = client.created();
        assert!(!client.needs_rekey(now));
        client
            .send_counter
            .store(REKEY_AFTER_MESSAGES, Ordering::Relaxed);
        assert!(client.needs_rekey(now));
        assert!(!client.is_expired(now));

        host.send_counter
            .store(REKEY_AFTER_MESSAGES, Ordering::Relaxed);
        assert!(!host.needs_rekey(now));
    }

    #[test]
    fn counters_count_up_from_zero() {
        let (mut client, mut host) = pair();
        let mut packet = Vec::new();
        let mut plain = Vec::new();
        for expected in 0..3 {
            client.encrypt(b"x", &mut packet).expect("encrypt");
            assert_eq!(
                packet::parse_data(&packet).map(|d| d.counter),
                Some(expected)
            );
            assert_eq!(
                host.decrypt(&packet, &mut plain).map(|r| r.counter),
                Ok(expected)
            );
        }
    }
}
