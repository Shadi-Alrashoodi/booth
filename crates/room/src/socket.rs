// The room's one socket: net's, plus a line in the log when a send fails.
// The room itself goes on ignoring failed sends; silence from the peer is
// what it acts on. The log is where a dead route or a firewall shows up.

use std::collections::HashMap;
use std::io;
use std::net::{Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use net::BindError;

use crate::log::{Log, log};

// One line per destination per minute: a ladder aimed at a dead address
// fails five times a second.
const QUIET_FOR: Duration = Duration::from_secs(60);
const MAX_FAILING: usize = 64;
// An IPv4 header and a UDP header, and the same over IPv6.
const IPV4_UDP: usize = 20 + 8;
const IPV6_UDP: usize = 40 + 8;

pub(crate) struct Socket {
    inner: net::Socket,
    log: Log,
    // When a failed send to each address was last written down.
    failing: Mutex<HashMap<SocketAddr, Instant>>,
    // Every byte sent, with the IP and UDP headers each datagram carries,
    // for the stats panel's total upload.
    sent_bytes: AtomicU64,
}

impl Socket {
    pub(crate) fn bind(port: u16, log: Log) -> Result<Socket, BindError> {
        Ok(Socket {
            inner: net::Socket::bind(port)?,
            log,
            failing: Mutex::default(),
            sent_bytes: AtomicU64::new(0),
        })
    }

    pub(crate) fn local_port(&self) -> u16 {
        self.inner.local_port()
    }

    pub(crate) fn has_ipv6(&self) -> bool {
        self.inner.has_ipv6()
    }

    pub(crate) fn set_ipv6_source(&self, source: Option<Ipv6Addr>) {
        self.inner.set_ipv6_source(source);
    }

    pub(crate) fn ipv6_source(&self) -> Option<Ipv6Addr> {
        self.inner.ipv6_source()
    }

    pub(crate) fn oversized_drops(&self) -> u64 {
        self.inner.oversized_drops()
    }

    pub(crate) fn sent_bytes(&self) -> u64 {
        self.sent_bytes.load(Ordering::Relaxed)
    }

    pub(crate) fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        self.inner.recv_from(buf)
    }

    pub(crate) fn wake(&self) -> io::Result<()> {
        self.inner.wake()
    }

    pub(crate) fn send_to(&self, buf: &[u8], to: SocketAddr) -> io::Result<usize> {
        let sent = self.inner.send_to(buf, to);
        if sent.is_ok() {
            let headers = match to {
                SocketAddr::V6(v6) if v6.ip().to_ipv4_mapped().is_none() => IPV6_UDP,
                _ => IPV4_UDP,
            };
            self.sent_bytes
                .fetch_add((buf.len() + headers) as u64, Ordering::Relaxed);
        }
        if let Err(err) = &sent
            && self.log.is_on()
        {
            self.failed(to, err);
        }
        sent
    }

    fn failed(&self, to: SocketAddr, err: &io::Error) {
        let now = Instant::now();
        let recent = |at: &Instant| now.saturating_duration_since(*at) < QUIET_FOR;
        {
            let mut failing = self.failing.lock().unwrap_or_else(PoisonError::into_inner);
            if failing.get(&to).is_some_and(recent) {
                return;
            }
            if failing.len() >= MAX_FAILING && !failing.contains_key(&to) {
                failing.retain(|_, at| recent(at));
                let oldest = failing.iter().min_by_key(|(_, at)| **at).map(|(a, _)| *a);
                if let Some(oldest) = oldest.filter(|_| failing.len() >= MAX_FAILING) {
                    failing.remove(&oldest);
                }
            }
            failing.insert(to, now);
        }
        log!(
            self.log,
            "could not send to {to}: {err}; further failures to {to} in the next minute are not written down"
        );
    }
}
