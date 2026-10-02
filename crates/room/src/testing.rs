// For unit tests that drive a Host or a Client by hand with a made-up clock.
// The far side is played with the session and channels crates directly, and
// packets cross real loopback sockets.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::time::Duration;

use channels::{Channel, PingMessage};
use session::Session;

// The far side's socket.
pub(crate) struct Wire {
    socket: UdpSocket,
}

impl Wire {
    pub(crate) fn new() -> Wire {
        Wire::at(Ipv4Addr::LOCALHOST)
    }

    // All of 127.0.0.0/8 is loopback on Windows, so a far side can have an
    // address of its own and a rate limit bucket of its own with it.
    pub(crate) fn at(ip: Ipv4Addr) -> Wire {
        let socket = UdpSocket::bind((ip, 0)).expect("bind a loopback socket");
        socket
            .set_read_timeout(Some(Duration::from_millis(20)))
            .expect("set a read timeout");
        Wire { socket }
    }

    pub(crate) fn addr(&self) -> SocketAddr {
        self.socket.local_addr().expect("local address")
    }

    // Everything that arrived so far. The room sends before these are read,
    // so the timeout only ends the wait once they are all in.
    pub(crate) fn packets(&self) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let mut buf = [0u8; 2048];
        while let Ok((len, _)) = self.socket.recv_from(&mut buf) {
            out.push(buf[..len].to_vec());
        }
        out
    }
}

// Two ends of one session, made by a real handshake between two made-up
// keys: the initiator's, which may send at once, and the responder's.
pub(crate) fn session_pair() -> (Session, Session) {
    let client = keys::Identity::generate();
    let host = keys::Identity::generate();
    let psk = [7u8; 32];
    let now = std::time::Instant::now();
    let (mut initiation, packet) = session::Initiation::start(
        &client.private_bytes(),
        client.public(),
        host.public(),
        &psk,
        session::InitKind::Known,
        session::Tai64N::now(),
        1,
    )
    .expect("start a handshake");
    let incoming = session::read_initiation(&host.private_bytes(), host.public(), &packet)
        .expect("read the initiation");
    let (responder, response) = incoming.accept(&psk, 2, now).expect("accept it");
    let initiator = initiation.finish(&response, now).expect("finish it");
    (initiator, responder)
}

pub(crate) fn seal(session: &mut Session, channel: Channel, payload: &[u8]) -> Vec<u8> {
    let mut plain = Vec::new();
    channels::frame(channel, payload, &mut plain);
    let mut packet = Vec::new();
    session.encrypt(&plain, &mut packet).expect("encrypt");
    packet
}

// A STUN server's answer to the request with `txid`: it saw the asker at
// `mapped`.
pub(crate) fn stun_answer(txid: &[u8; 12], mapped: SocketAddrV4) -> Vec<u8> {
    stun_answer_seeing(txid, SocketAddr::V4(mapped))
}

// The same in either family. An IPv6 address is masked with the cookie and
// the transaction id (RFC 5389 section 15.2).
pub(crate) fn stun_answer_seeing(txid: &[u8; 12], mapped: SocketAddr) -> Vec<u8> {
    let cookie = 0x2112_A442u32.to_be_bytes();
    let (family, octets): (u8, Vec<u8>) = match mapped {
        SocketAddr::V4(v4) => (0x01, v4.ip().octets().to_vec()),
        SocketAddr::V6(v6) => (0x02, v6.ip().octets().to_vec()),
    };
    let attribute_len = 4 + octets.len() as u8;
    let mut out = vec![0x01, 0x01, 0x00, 4 + attribute_len, 0x21, 0x12, 0xA4, 0x42];
    out.extend_from_slice(txid);
    out.extend_from_slice(&[0x00, 0x20, 0x00, attribute_len, 0x00, family]);
    out.extend_from_slice(&(mapped.port() ^ 0x2112).to_be_bytes());
    let mask = cookie.iter().chain(txid.iter());
    out.extend(octets.iter().zip(mask).map(|(b, m)| b ^ m));
    out
}

pub(crate) fn ping(session: &mut Session, seq: u32) -> Vec<u8> {
    let mut payload = Vec::new();
    PingMessage::Ping { seq, t1: 0 }.encode(&mut payload);
    seal(session, Channel::Ping, &payload)
}

// What a Host or a Client built by hand needs for voice when no audio thread
// runs: settings nobody acts on, and a mixer nobody reads the orders of.
pub(crate) fn quiet_voice(host: bool) -> std::sync::Arc<crate::talk::Shared> {
    crate::talk::Shared::new(&crate::talk::VoiceConfig::default(), None, host)
}

pub(crate) fn no_speaker() -> crossbeam_channel::Sender<crate::talk::ToSpeaker> {
    crossbeam_channel::unbounded().0
}

// Voice for a room whose threads run in a test: the fake devices, so no test
// opens a real microphone or plays a sound. 10 ms periods, since these rooms
// do not talk.
pub(crate) fn fake_voice() -> crate::talk::VoiceConfig {
    use voice::audio::fake::{Fake, Setup};
    let setup = Setup {
        period_frames: 480,
        buffer_frames: 960,
        ..Setup::default()
    };
    crate::talk::VoiceConfig {
        devices: crate::talk::Devices::Fake {
            microphone: Fake::new(setup, &[("mic", "Test microphone")], Some("mic")),
            speakers: Fake::new(setup, &[("out", "Test speakers")], Some("out")),
        },
        ..crate::talk::VoiceConfig::default()
    }
}

// Sharing for a Host or a Client built by hand: no socket, so an Outbox
// sends nothing, and events nobody waits on.
pub(crate) fn quiet_screen() -> crate::screen::ScreenSetup {
    crate::screen::ScreenSetup {
        socket: None,
        upload_kbps: crate::config::DEFAULT_VIDEO_UPLOAD_KBPS,
        wake: net::pace::Signal::new().expect("create an event"),
        answered: net::pace::Signal::new().expect("create an event"),
        threads: false,
        knob: None,
        hevc: true,
        injector: None,
        control_wake: net::pace::Signal::new().expect("create an event"),
    }
}

// A view with nothing in it, for tests of what is worked out from one.
pub(crate) fn empty_view(role: crate::view::Role) -> crate::view::View {
    crate::view::View {
        role,
        room_name: String::new(),
        strip: crate::view::Strip::default(),
        people: Vec::new(),
        invite: None,
        numbers: crate::view::Numbers::default(),
        chat: std::sync::Arc::default(),
        notice: None,
        reply: None,
        paste: None,
        address_changed: None,
        list_problem: None,
        share: crate::view::ShareView::default(),
        voice: crate::view::Voice::default(),
    }
}
