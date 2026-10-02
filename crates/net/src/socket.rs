use std::fmt;
use std::io::{self, IoSlice};
use std::mem::offset_of;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6, UdpSocket};
use std::os::windows::io::{AsRawSocket, RawSocket};
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use socket2::{Domain, MsgHdr, Protocol, SockAddr, SockRef, Type};
use windows_sys::Win32::Networking::WinSock::{
    CMSGHDR, IN6_PKTINFO, IPPROTO_IPV6, IPV6_PKTINFO, SIO_UDP_CONNRESET, SIO_UDP_NETRESET,
    SO_EXCLUSIVEADDRUSE, SOCKET, SOCKET_ERROR, SOL_SOCKET, WSAEADDRNOTAVAIL, WSAEAFNOSUPPORT,
    WSAECONNRESET, WSAEMSGSIZE, WSAENETRESET, WSAGetLastError, WSAIoctl, setsockopt,
};

use crate::holder::{self, Holder};

// Video comes later and hands the socket a whole frame at once.
const BUFFER_BYTES: usize = 1 << 20;

// Our largest packet is 1400 bytes (video on a LAN). A buffer that holds a
// full Ethernet payload also holds every STUN answer.
pub const MIN_RECV_BUFFER: usize = 1500;

#[derive(Debug)]
pub struct Socket {
    udp: UdpSocket,
    port: u16,
    dual_stack: bool,
    shared: Arc<Shared>,
}

// What every clone of one socket sees: the receive thread and the senders
// hold different clones.
#[derive(Debug, Default)]
struct Shared {
    ipv6_source: Mutex<Option<Ipv6Addr>>,
    oversized: AtomicU64,
}

impl Socket {
    pub fn bind(port: u16) -> Result<Socket, BindError> {
        match open(true, port) {
            Err((step, err)) if no_ipv6(step, &err) => {
                open(false, port).map_err(|(step, err)| BindError::new(port, step, err))
            }
            other => other.map_err(|(step, err)| BindError::new(port, step, err)),
        }
    }

    pub fn local_port(&self) -> u16 {
        self.port
    }

    // False only on a PC without IPv6, where the fallback socket is IPv4 only.
    pub fn has_ipv6(&self) -> bool {
        self.dual_stack
    }

    pub fn try_clone(&self) -> io::Result<Socket> {
        Ok(Socket {
            udp: self.udp.try_clone()?,
            port: self.port,
            dual_stack: self.dual_stack,
            shared: Arc::clone(&self.shared),
        })
    }

    // Pass the stable address from addrs::local_addresses(). Left alone,
    // Windows sends from the daily temporary address, so the router opens its
    // pinhole for an address that is not in the invite, and a friend whose
    // router filters by address drops replies that come from a different
    // address than the one they sent to. Applies to every clone, and only to
    // global destinations: loopback, link-local, ULA (Tailscale's range) and
    // IPv4 keep Windows' own choice. When the prefix changes the pinned
    // address disappears and sends to global destinations fail, naming it,
    // until the new stable address is set.
    pub fn set_ipv6_source(&self, source: Option<Ipv6Addr>) {
        *self
            .shared
            .ipv6_source
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = source;
    }

    pub fn ipv6_source(&self) -> Option<Ipv6Addr> {
        *self
            .shared
            .ipv6_source
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    // Datagrams too big for the buffer given to recv_from, across all clones.
    // Not zero means a receive buffer is smaller than MIN_RECV_BUFFER or
    // someone is sending junk; either way the stats panel should say so.
    pub fn oversized_drops(&self) -> u64 {
        self.shared.oversized.load(Ordering::Relaxed)
    }

    pub fn send_to(&self, buf: &[u8], to: SocketAddr) -> io::Result<usize> {
        let to = match (self.dual_stack, to) {
            (true, SocketAddr::V4(v4)) => {
                SocketAddr::V6(SocketAddrV6::new(v4.ip().to_ipv6_mapped(), v4.port(), 0, 0))
            }
            (false, SocketAddr::V6(v6)) => match v6.ip().to_ipv4_mapped() {
                Some(v4) => SocketAddr::V4(SocketAddrV4::new(v4, v6.port())),
                None => {
                    return Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        format!(
                            "cannot send to {to}: this pc has no ipv6, the udp socket is ipv4 only"
                        ),
                    ));
                }
            },
            (_, to) => to,
        };
        match (to, self.ipv6_source()) {
            (SocketAddr::V6(v6), Some(from)) if is_global(v6.ip()) => {
                send_from(&self.udp, buf, v6, from)
            }
            _ => self.udp.send_to(buf, to),
        }
    }

    // A datagram longer than `buf` is dropped and counted in oversized_drops()
    // rather than returned as an error, so a stranger cannot make the receive
    // thread see an error just by sending a big packet.
    pub fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        debug_assert!(
            buf.len() >= MIN_RECV_BUFFER,
            "receive buffer of {} bytes is smaller than MIN_RECV_BUFFER, lan video packets would be dropped",
            buf.len()
        );
        loop {
            match self.udp.recv_from(buf) {
                Ok((n, from)) => return Ok((n, unmap(from))),
                Err(err) => match err.raw_os_error() {
                    Some(WSAEMSGSIZE) => {
                        self.shared.oversized.fetch_add(1, Ordering::Relaxed);
                    }
                    // The SIO_UDP ioctls in open() should stop these from
                    // arriving at all; this catches whatever still slips through.
                    Some(WSAECONNRESET | WSAENETRESET) => {}
                    _ => return Err(err),
                },
            }
        }
    }

    // Only makes a thread blocked in recv_from return. Anyone can send an
    // empty datagram, from anywhere, so what that recv_from returns means
    // nothing by itself: set your own stop flag before calling this and check
    // it after every return from recv_from, never the length or the source.
    pub fn wake(&self) -> io::Result<()> {
        let v4 = SocketAddr::from((Ipv4Addr::LOCALHOST, self.port));
        match self.send_to(&[], v4) {
            Ok(_) => Ok(()),
            Err(err) if self.dual_stack => {
                let v6 = SocketAddr::from((Ipv6Addr::LOCALHOST, self.port));
                self.send_to(&[], v6).map(|_| ()).map_err(|_| err)
            }
            Err(err) => Err(err),
        }
    }
}

// 2000::/3, where a home PC's stable and temporary addresses both live.
fn is_global(ip: &Ipv6Addr) -> bool {
    let [first, ..] = ip.segments();
    first & 0xE000 == 0x2000
}

// A socket bound to [::] takes its source address per send from an
// IPV6_PKTINFO control message. Interface index 0 leaves the adapter to
// Windows' routes; the address alone already names the one it lives on.
fn send_from(udp: &UdpSocket, buf: &[u8], to: SocketAddrV6, from: Ipv6Addr) -> io::Result<usize> {
    let control = PktInfo::new(from);
    let addr = SockAddr::from(to);
    let bufs = [IoSlice::new(buf)];
    let msg = MsgHdr::new()
        .with_addr(&addr)
        .with_buffers(&bufs)
        .with_control(&control.0);
    SockRef::from(udp).sendmsg(&msg, 0).map_err(|err| {
        io::Error::new(
            err.kind(),
            format!("could not send from {from} to {to}: {err}"),
        )
    })
}

// One control message laid out the way the WSA_CMSG_* macros in ws2def.h do
// it: the header, the data from the next natural-alignment boundary, and the
// whole thing rounded up to that boundary.
const CMSG_ALIGN: usize = align_of::<usize>();
const CMSG_DATA_AT: usize = size_of::<CMSGHDR>().next_multiple_of(CMSG_ALIGN);
const PKTINFO_LEN: usize = CMSG_DATA_AT + size_of::<IN6_PKTINFO>();
const PKTINFO_SPACE: usize = CMSG_DATA_AT + size_of::<IN6_PKTINFO>().next_multiple_of(CMSG_ALIGN);

// Windows reads the header's size_t straight out of this buffer.
#[repr(C, align(8))]
struct PktInfo([u8; PKTINFO_SPACE]);

impl PktInfo {
    fn new(source: Ipv6Addr) -> PktInfo {
        fn put(bytes: &mut [u8], at: usize, value: &[u8]) {
            if let Some(dst) = bytes.get_mut(at..at + value.len()) {
                dst.copy_from_slice(value);
            }
        }
        let mut bytes = [0u8; PKTINFO_SPACE];
        put(
            &mut bytes,
            offset_of!(CMSGHDR, cmsg_len),
            &PKTINFO_LEN.to_ne_bytes(),
        );
        put(
            &mut bytes,
            offset_of!(CMSGHDR, cmsg_level),
            &IPPROTO_IPV6.to_ne_bytes(),
        );
        put(
            &mut bytes,
            offset_of!(CMSGHDR, cmsg_type),
            &IPV6_PKTINFO.to_ne_bytes(),
        );
        let data = CMSG_DATA_AT + offset_of!(IN6_PKTINFO, ipi6_addr);
        put(&mut bytes, data, &source.octets());
        PktInfo(bytes)
    }
}

pub(crate) fn unmap(addr: SocketAddr) -> SocketAddr {
    match addr {
        SocketAddr::V6(v6) => match v6.ip().to_ipv4_mapped() {
            Some(v4) => SocketAddr::V4(SocketAddrV4::new(v4, v6.port())),
            None => addr,
        },
        SocketAddr::V4(_) => addr,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    Create,
    DualStack,
    Exclusive,
    Bind,
    Buffers,
    Resets,
}

impl Step {
    fn what(self) -> &'static str {
        match self {
            Step::Create => "creating the socket",
            Step::DualStack => "turning on dual-stack",
            Step::Exclusive => "claiming the port for this program only",
            Step::Bind => "binding",
            Step::Buffers => "setting 1 MB socket buffers",
            Step::Resets => "turning off udp connection reset",
        }
    }
}

fn no_ipv6(step: Step, err: &io::Error) -> bool {
    match step {
        Step::Create | Step::DualStack => true,
        Step::Bind => matches!(err.raw_os_error(), Some(WSAEAFNOSUPPORT | WSAEADDRNOTAVAIL)),
        _ => false,
    }
}

fn open(ipv6: bool, port: u16) -> Result<Socket, (Step, io::Error)> {
    let domain = if ipv6 { Domain::IPV6 } else { Domain::IPV4 };
    let sock = socket2::Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))
        .map_err(|e| (Step::Create, e))?;
    if ipv6 {
        sock.set_only_v6(false).map_err(|e| (Step::DualStack, e))?;
    }
    // Without this another program could bind the same port with
    // SO_REUSEADDR and receive part of our traffic.
    set_exclusive(sock.as_raw_socket()).map_err(|e| (Step::Exclusive, e))?;

    let any = if ipv6 {
        SocketAddr::from((Ipv6Addr::UNSPECIFIED, port))
    } else {
        SocketAddr::from((Ipv4Addr::UNSPECIFIED, port))
    };
    sock.bind(&any.into()).map_err(|e| (Step::Bind, e))?;

    sock.set_recv_buffer_size(BUFFER_BYTES)
        .and_then(|()| sock.set_send_buffer_size(BUFFER_BYTES))
        .map_err(|e| (Step::Buffers, e))?;

    // One ICMP port-unreachable, from a friend who just quit, would otherwise
    // make the next receive fail with WSAECONNRESET, which looks like the host
    // itself dying. NETRESET is the same for ICMP time-exceeded.
    report_udp_resets(sock.as_raw_socket(), SIO_UDP_CONNRESET).map_err(|e| (Step::Resets, e))?;
    report_udp_resets(sock.as_raw_socket(), SIO_UDP_NETRESET).map_err(|e| (Step::Resets, e))?;

    let udp: UdpSocket = sock.into();
    let port = udp.local_addr().map_err(|e| (Step::Bind, e))?.port();
    Ok(Socket {
        udp,
        port,
        dual_stack: ipv6,
        shared: Arc::default(),
    })
}

#[allow(unsafe_code)]
fn set_exclusive(sock: RawSocket) -> io::Result<()> {
    let on: i32 = 1;
    // SAFETY: `sock` is an open socket owned by the caller for the whole call,
    // and optval points at a live i32 whose size is passed as optlen.
    let rc = unsafe {
        setsockopt(
            sock as SOCKET,
            SOL_SOCKET,
            SO_EXCLUSIVEADDRUSE,
            (&raw const on).cast(),
            size_of::<i32>() as i32,
        )
    };
    if rc == SOCKET_ERROR {
        return Err(last_wsa_error());
    }
    Ok(())
}

#[allow(unsafe_code)]
fn report_udp_resets(sock: RawSocket, code: u32) -> io::Result<()> {
    let off: i32 = 0;
    let mut returned: u32 = 0;
    // SAFETY: `sock` is an open socket owned by the caller. The input buffer is
    // a live BOOL with its size passed alongside, there is no output buffer,
    // and the call is synchronous (no OVERLAPPED, no completion routine).
    let rc = unsafe {
        WSAIoctl(
            sock as SOCKET,
            code,
            (&raw const off).cast(),
            size_of::<i32>() as u32,
            ptr::null_mut(),
            0,
            &mut returned,
            ptr::null_mut(),
            None,
        )
    };
    if rc == SOCKET_ERROR {
        return Err(last_wsa_error());
    }
    Ok(())
}

#[allow(unsafe_code)]
pub(crate) fn last_wsa_error() -> io::Error {
    // SAFETY: WSAGetLastError only reads this thread's last socket error.
    io::Error::from_raw_os_error(unsafe { WSAGetLastError() })
}

#[derive(Debug)]
pub struct BindError {
    pub port: u16,
    pub kind: io::ErrorKind,
    step: Step,
    source: io::Error,
    holder: Option<Holder>,
}

impl BindError {
    fn new(port: u16, step: Step, source: io::Error) -> BindError {
        let kind = source.kind();
        // Asked at once, while the holder still has it.
        let holder = (step == Step::Bind && kind == io::ErrorKind::AddrInUse)
            .then(|| holder::udp_port(port))
            .flatten();
        BindError {
            port,
            kind,
            step,
            source,
            holder,
        }
    }

    pub fn is_in_use(&self) -> bool {
        self.kind == io::ErrorKind::AddrInUse
    }

    // Who had the port when the bind found it in use, if Windows said.
    pub fn holder(&self) -> Option<&Holder> {
        self.holder.as_ref()
    }
}

impl fmt::Display for BindError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let port = self.port;
        match (self.step, self.kind) {
            (Step::Bind, io::ErrorKind::AddrInUse) => {
                write!(f, "could not bind udp port {port}: address in use")?;
                match &self.holder {
                    None => Ok(()),
                    Some(holder) if holder.is_this_process() => {
                        write!(f, ", held by this process, pid {}", holder.pid)
                    }
                    Some(Holder {
                        pid,
                        exe: Some(exe),
                    }) => write!(f, ", held by pid {pid}, {}", exe.display()),
                    Some(Holder { pid, exe: None }) => {
                        write!(f, ", held by pid {pid}, which windows does not name")
                    }
                }
            }
            // Hyper-V and WSL reserve port ranges at boot, and binding inside one
            // fails with an access error that reads like a permissions problem.
            (Step::Bind, io::ErrorKind::PermissionDenied) => write!(
                f,
                "could not bind udp port {port}: windows refused it, the port may be reserved by hyper-v or wsl; pick another port"
            ),
            (Step::Bind, _) => write!(f, "could not bind udp port {port}: {}", self.source),
            (step, _) => write!(
                f,
                "could not open udp port {port}: {} failed: {}",
                step.what(),
                self.source
            ),
        }
    }
}

impl std::error::Error for BindError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    // Goes around the retry loop in recv_from on purpose: this checks that the
    // ioctl itself took effect, not that the loop hides the error.
    #[test]
    fn port_unreachable_does_not_reach_the_socket() {
        let sock = Socket::bind(0).unwrap();
        sock.udp
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let closed = {
            let s = UdpSocket::bind("127.0.0.1:0").unwrap();
            s.local_addr().unwrap()
        };
        sock.send_to(b"anyone there", closed).unwrap();
        std::thread::sleep(Duration::from_millis(50));

        let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
        sender
            .send_to(b"real", ("127.0.0.1", sock.local_port()))
            .unwrap();

        let mut buf = [0u8; 16];
        let (n, _) = sock.udp.recv_from(&mut buf).unwrap();
        assert_eq!(buf.get(..n), Some(&b"real"[..]));
    }

    #[test]
    fn ipv4_only_socket_refuses_ipv6_targets() {
        let sock = open(false, 0).unwrap();
        let err = sock
            .send_to(b"x", "[2001:db8::1]:41000".parse().unwrap())
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);
        assert!(err.to_string().contains("ipv4 only"), "{err}");

        let mapped = sock.send_to(
            b"x",
            SocketAddr::from((Ipv4Addr::LOCALHOST.to_ipv6_mapped(), sock.local_port())),
        );
        assert!(mapped.is_ok(), "{mapped:?}");
    }

    // x64 values of WSA_CMSG_LEN and WSA_CMSG_SPACE for a 20-byte IN6_PKTINFO.
    #[cfg(target_pointer_width = "64")]
    #[test]
    fn pktinfo_is_laid_out_like_ws2def() {
        assert_eq!((PKTINFO_LEN, PKTINFO_SPACE), (36, 40));
        let source: Ipv6Addr = "2001:db8::7".parse().unwrap();
        let control = PktInfo::new(source);
        let mut want = [0u8; 40];
        want[..8].copy_from_slice(&36u64.to_le_bytes());
        want[8..12].copy_from_slice(&41i32.to_le_bytes());
        want[12..16].copy_from_slice(&19i32.to_le_bytes());
        want[16..32].copy_from_slice(&source.octets());
        assert_eq!(control.0, want);
    }

    // Loopback stands in for a global address here: send_from is what
    // send_to calls when a source is pinned and the target is global.
    #[test]
    fn pinned_source_is_used_and_checked() {
        if UdpSocket::bind("[::1]:0").is_err() {
            eprintln!("skipped: this pc has no ipv6 loopback");
            return;
        }
        let a = Socket::bind(0).unwrap();
        let b = Socket::bind(0).unwrap();
        if !a.has_ipv6() {
            eprintln!("skipped: the socket fell back to ipv4 only");
            return;
        }
        b.udp
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let to_b = SocketAddrV6::new(Ipv6Addr::LOCALHOST, b.local_port(), 0, 0);

        let sent = send_from(&a.udp, b"from ::1", to_b, Ipv6Addr::LOCALHOST).unwrap();
        assert_eq!(sent, 8);
        let mut buf = [0u8; MIN_RECV_BUFFER];
        let (n, from) = b.recv_from(&mut buf).unwrap();
        assert_eq!(buf.get(..n), Some(&b"from ::1"[..]));
        assert_eq!(
            from,
            SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::LOCALHOST, a.local_port(), 0, 0))
        );

        // If Windows ignored the control message this would go out from ::1.
        let not_ours: Ipv6Addr = "2001:db8::1".parse().unwrap();
        let err = send_from(&a.udp, b"x", to_b, not_ours).unwrap_err();
        assert!(
            err.to_string().contains("could not send from 2001:db8::1"),
            "{err}"
        );
    }

    #[test]
    fn only_global_targets_take_the_pinned_source() {
        for global in ["2001:db8::1", "2a02:8108::10", "3fff::1"] {
            assert!(is_global(&global.parse().unwrap()), "{global}");
        }
        for other in [
            "::1",
            "fe80::1",
            "fd7a:115c:a1e0::1",
            "::ffff:192.168.1.20",
            "ff02::1",
            "::",
        ] {
            assert!(!is_global(&other.parse().unwrap()), "{other}");
        }
    }
}
