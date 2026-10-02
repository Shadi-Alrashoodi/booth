use crate::mac;

pub(crate) const KEY_LEN: usize = 32;
pub(crate) const TAG_LEN: usize = 16;
const MAC_LEN: usize = 16;
pub(crate) const XNONCE_LEN: usize = 24;
pub(crate) const COOKIE_LEN: usize = 16;
const HEADER_LEN: usize = 4;
const INDEX_LEN: usize = 4;
const COUNTER_LEN: usize = 8;

// Noise message 1 is e, then s and the payload, each encrypted with its own tag.
pub(crate) const INITIATION_NOISE_OVERHEAD: usize = KEY_LEN + KEY_LEN + TAG_LEN + TAG_LEN;
// Noise message 2 is e, then the tag of an empty payload.
pub(crate) const RESPONSE_NOISE_LEN: usize = KEY_LEN + TAG_LEN;

pub(crate) const PAYLOAD_LEN_KNOWN: usize = 1 + 12 + 1;
pub(crate) const PAYLOAD_LEN_INVITE: usize = PAYLOAD_LEN_KNOWN + 8;

const INITIATION_FIXED_LEN: usize =
    HEADER_LEN + INDEX_LEN + INITIATION_NOISE_OVERHEAD + MAC_LEN + MAC_LEN;
const INITIATION_LEN_KNOWN: usize = INITIATION_FIXED_LEN + PAYLOAD_LEN_KNOWN;
const INITIATION_LEN_INVITE: usize = INITIATION_FIXED_LEN + PAYLOAD_LEN_INVITE;
const RESPONSE_LEN: usize =
    HEADER_LEN + INDEX_LEN + INDEX_LEN + RESPONSE_NOISE_LEN + MAC_LEN + MAC_LEN;
pub const COOKIE_REPLY_LEN: usize = HEADER_LEN + INDEX_LEN + XNONCE_LEN + COOKIE_LEN + TAG_LEN;

// Before a handshake completes the host must never send more bytes than it received, or anyone
// holding the host's public key could use it to amplify a flood toward a spoofed address.
const _: () = assert!(RESPONSE_LEN < INITIATION_LEN_KNOWN);
const _: () = assert!(COOKIE_REPLY_LEN < INITIATION_LEN_KNOWN);

pub(crate) const DATA_HEADER_LEN: usize = HEADER_LEN + INDEX_LEN + COUNTER_LEN;
pub const DATA_OVERHEAD: usize = DATA_HEADER_LEN + TAG_LEN;

// The largest payload that still fits in one IPv4 UDP datagram with our overhead added.
pub const MAX_PLAINTEXT_LEN: usize = 65_507 - DATA_OVERHEAD;
const MAX_DATA_LEN: usize = MAX_PLAINTEXT_LEN + DATA_OVERHEAD;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PacketType {
    Initiation,
    Response,
    CookieReply,
    Punch,
    Data,
}

impl PacketType {
    // STUN shares the socket and its messages start with 0x00 to 0x03, so ours never do.
    pub const fn byte(self) -> u8 {
        match self {
            PacketType::Initiation => 0x11,
            PacketType::Response => 0x12,
            PacketType::CookieReply => 0x13,
            PacketType::Punch => 0x14,
            PacketType::Data => 0x15,
        }
    }
}

pub fn packet_type(buf: &[u8]) -> Option<PacketType> {
    match buf.first()? {
        0x11 => Some(PacketType::Initiation),
        0x12 => Some(PacketType::Response),
        0x13 => Some(PacketType::CookieReply),
        0x14 => Some(PacketType::Punch),
        0x15 => Some(PacketType::Data),
        _ => None,
    }
}

// The host sends these toward a client's outside address to open its own
// router for that client. They carry nothing: the random bytes only keep a
// router from treating them as one repeated packet.
pub const PUNCH_LEN: usize = 32;
pub const PUNCH_RANDOM_LEN: usize = PUNCH_LEN - HEADER_LEN;

pub fn punch_packet(random: &[u8; PUNCH_RANDOM_LEN]) -> [u8; PUNCH_LEN] {
    let mut packet = [0u8; PUNCH_LEN];
    packet[..HEADER_LEN].copy_from_slice(&header(PacketType::Punch));
    packet[HEADER_LEN..].copy_from_slice(random);
    packet
}

pub fn is_punch(buf: &[u8]) -> bool {
    buf.len() == PUNCH_LEN && strip_header(buf, PacketType::Punch).is_some()
}

pub fn response_receiver_index(buf: &[u8]) -> Option<u32> {
    parse_response(buf).map(|response| response.receiver_index)
}

pub fn data_receiver_index(buf: &[u8]) -> Option<u32> {
    parse_data(buf).map(|data| data.receiver_index)
}

pub fn cookie_reply_receiver_index(buf: &[u8]) -> Option<u32> {
    parse_cookie_reply(buf).map(|reply| reply.receiver_index)
}

pub(crate) struct InitiationView<'a> {
    pub sender_index: u32,
    pub ephemeral: &'a [u8; KEY_LEN],
    pub noise: &'a [u8],
    pub covered: &'a [u8],
    pub mac1: &'a [u8; MAC_LEN],
    // Everything before mac2, mac1 included.
    pub mac2_covered: &'a [u8],
    pub mac2: &'a [u8; MAC_LEN],
}

pub(crate) fn parse_initiation(packet: &[u8]) -> Option<InitiationView<'_>> {
    if packet.len() != INITIATION_LEN_KNOWN && packet.len() != INITIATION_LEN_INVITE {
        return None;
    }
    let (mac2_covered, mac2) = packet.split_last_chunk::<MAC_LEN>()?;
    let (covered, mac1) = mac2_covered.split_last_chunk::<MAC_LEN>()?;
    let body = strip_header(covered, PacketType::Initiation)?;
    let (sender, noise) = body.split_first_chunk::<INDEX_LEN>()?;
    let ephemeral = noise.first_chunk::<KEY_LEN>()?;
    Some(InitiationView {
        sender_index: u32::from_le_bytes(*sender),
        ephemeral,
        noise,
        covered,
        mac1,
        mac2_covered,
        mac2,
    })
}

pub(crate) struct ResponseView<'a> {
    pub sender_index: u32,
    pub receiver_index: u32,
    pub ephemeral: &'a [u8; KEY_LEN],
    pub noise: &'a [u8],
    pub covered: &'a [u8],
    pub mac1: &'a [u8; MAC_LEN],
}

pub(crate) fn parse_response(packet: &[u8]) -> Option<ResponseView<'_>> {
    if packet.len() != RESPONSE_LEN {
        return None;
    }
    let (covered, mac1) = split_macs(packet)?;
    let body = strip_header(covered, PacketType::Response)?;
    let (sender, body) = body.split_first_chunk::<INDEX_LEN>()?;
    let (receiver, noise) = body.split_first_chunk::<INDEX_LEN>()?;
    let ephemeral = noise.first_chunk::<KEY_LEN>()?;
    Some(ResponseView {
        sender_index: u32::from_le_bytes(*sender),
        receiver_index: u32::from_le_bytes(*receiver),
        ephemeral,
        noise,
        covered,
        mac1,
    })
}

pub(crate) struct CookieReplyView<'a> {
    pub receiver_index: u32,
    pub nonce: &'a [u8; XNONCE_LEN],
    pub sealed_cookie: &'a [u8; COOKIE_LEN],
    pub tag: &'a [u8; TAG_LEN],
}

pub(crate) fn parse_cookie_reply(packet: &[u8]) -> Option<CookieReplyView<'_>> {
    if packet.len() != COOKIE_REPLY_LEN {
        return None;
    }
    let body = strip_header(packet, PacketType::CookieReply)?;
    let (receiver, body) = body.split_first_chunk::<INDEX_LEN>()?;
    let (nonce, body) = body.split_first_chunk::<XNONCE_LEN>()?;
    let (sealed_cookie, tag) = body.split_first_chunk::<COOKIE_LEN>()?;
    Some(CookieReplyView {
        receiver_index: u32::from_le_bytes(*receiver),
        nonce,
        sealed_cookie,
        tag: tag.try_into().ok()?,
    })
}

pub(crate) struct DataView<'a> {
    pub receiver_index: u32,
    pub counter: u64,
    pub ciphertext: &'a [u8],
}

pub(crate) fn parse_data(packet: &[u8]) -> Option<DataView<'_>> {
    // Longer than encrypt can make means a caller bug or a forgery, and decrypt would size its
    // output buffer from it before snow looked at it.
    if !(DATA_OVERHEAD..=MAX_DATA_LEN).contains(&packet.len()) {
        return None;
    }
    let body = strip_header(packet, PacketType::Data)?;
    let (receiver, body) = body.split_first_chunk::<INDEX_LEN>()?;
    let (counter, ciphertext) = body.split_first_chunk::<COUNTER_LEN>()?;
    Some(DataView {
        receiver_index: u32::from_le_bytes(*receiver),
        counter: u64::from_le_bytes(*counter),
        ciphertext,
    })
}

// Returns the packet and its mac1, which a cookie reply to it is sealed against.
pub(crate) fn initiation_packet(
    sender_index: u32,
    noise: &[u8],
    receiver_public: &[u8; KEY_LEN],
    cookie: Option<&[u8; COOKIE_LEN]>,
) -> (Vec<u8>, [u8; MAC_LEN]) {
    let mut packet = Vec::with_capacity(INITIATION_LEN_INVITE);
    packet.extend_from_slice(&header(PacketType::Initiation));
    packet.extend_from_slice(&sender_index.to_le_bytes());
    packet.extend_from_slice(noise);
    seal(packet, receiver_public, cookie)
}

pub(crate) fn response_packet(
    sender_index: u32,
    receiver_index: u32,
    noise: &[u8],
    receiver_public: &[u8; KEY_LEN],
) -> Vec<u8> {
    let mut packet = Vec::with_capacity(RESPONSE_LEN);
    packet.extend_from_slice(&header(PacketType::Response));
    packet.extend_from_slice(&sender_index.to_le_bytes());
    packet.extend_from_slice(&receiver_index.to_le_bytes());
    packet.extend_from_slice(noise);
    seal(packet, receiver_public, None).0
}

pub(crate) fn cookie_reply_packet(
    receiver_index: u32,
    nonce: &[u8; XNONCE_LEN],
    sealed_cookie: &[u8; COOKIE_LEN],
    tag: &[u8; TAG_LEN],
) -> [u8; COOKIE_REPLY_LEN] {
    const INDEX_AT: usize = HEADER_LEN;
    const NONCE_AT: usize = INDEX_AT + INDEX_LEN;
    const COOKIE_AT: usize = NONCE_AT + XNONCE_LEN;
    const TAG_AT: usize = COOKIE_AT + COOKIE_LEN;
    let mut packet = [0u8; COOKIE_REPLY_LEN];
    packet[..INDEX_AT].copy_from_slice(&header(PacketType::CookieReply));
    packet[INDEX_AT..NONCE_AT].copy_from_slice(&receiver_index.to_le_bytes());
    packet[NONCE_AT..COOKIE_AT].copy_from_slice(nonce);
    packet[COOKIE_AT..TAG_AT].copy_from_slice(sealed_cookie);
    packet[TAG_AT..].copy_from_slice(tag);
    packet
}

pub(crate) fn write_data_header(out: &mut Vec<u8>, receiver_index: u32, counter: u64) {
    out.extend_from_slice(&header(PacketType::Data));
    out.extend_from_slice(&receiver_index.to_le_bytes());
    out.extend_from_slice(&counter.to_le_bytes());
}

fn header(kind: PacketType) -> [u8; HEADER_LEN] {
    [kind.byte(), 0, 0, 0]
}

fn strip_header(packet: &[u8], kind: PacketType) -> Option<&[u8]> {
    let (found, rest) = packet.split_first_chunk::<HEADER_LEN>()?;
    (*found == header(kind)).then_some(rest)
}

// Without a cookie mac2 is sent as zeros, as in WireGuard.
fn seal(
    mut packet: Vec<u8>,
    receiver_public: &[u8; KEY_LEN],
    cookie: Option<&[u8; COOKIE_LEN]>,
) -> (Vec<u8>, [u8; MAC_LEN]) {
    let mac1 = mac::mac1(receiver_public, &packet);
    packet.extend_from_slice(&mac1);
    let mac2 = cookie.map_or([0; MAC_LEN], |cookie| mac::mac2(cookie, &packet));
    packet.extend_from_slice(&mac2);
    (packet, mac1)
}

// mac2 is skipped, not required to be zero, so a peer that starts filling it in still gets through.
fn split_macs(packet: &[u8]) -> Option<(&[u8], &[u8; MAC_LEN])> {
    let (rest, _mac2) = packet.split_last_chunk::<MAC_LEN>()?;
    rest.split_last_chunk::<MAC_LEN>()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stun_first_bytes_are_never_ours() {
        for first in 0x00..=0x03u8 {
            assert_eq!(packet_type(&[first, 0, 0, 0]), None);
        }
        assert_eq!(packet_type(&[]), None);
    }

    #[test]
    fn type_bytes_round_trip() {
        for kind in [
            PacketType::Initiation,
            PacketType::Response,
            PacketType::CookieReply,
            PacketType::Punch,
            PacketType::Data,
        ] {
            assert_eq!(packet_type(&[kind.byte()]), Some(kind));
        }
    }

    #[test]
    fn oversized_data_packet_is_refused() {
        let mut packet = Vec::new();
        write_data_header(&mut packet, 7, 0);
        packet.resize(MAX_DATA_LEN, 0);
        assert_eq!(data_receiver_index(&packet), Some(7));
        packet.push(0);
        assert_eq!(data_receiver_index(&packet), None);
    }

    #[test]
    fn punch_packet_layout() {
        let random: [u8; PUNCH_RANDOM_LEN] = std::array::from_fn(|i| i as u8 + 1);
        let packet = punch_packet(&random);
        assert_eq!(packet.len(), 32);
        assert_eq!(packet[..4], [0x14, 0, 0, 0]);
        assert_eq!(packet[4..], random);
        assert_eq!(packet_type(&packet), Some(PacketType::Punch));
        assert!(is_punch(&packet));

        assert!(!is_punch(&packet[..31]));
        let mut longer = packet.to_vec();
        longer.push(0);
        assert!(!is_punch(&longer));
        let mut reserved = packet;
        reserved[2] = 1;
        assert!(!is_punch(&reserved));
        let mut data = packet;
        data[0] = PacketType::Data.byte();
        assert!(!is_punch(&data));
    }

    #[test]
    fn packet_lengths() {
        assert_eq!(DATA_OVERHEAD, 32);
        assert_eq!(INITIATION_LEN_KNOWN, 150);
        assert_eq!(INITIATION_LEN_INVITE, 158);
        assert_eq!(RESPONSE_LEN, 92);
        assert_eq!(COOKIE_REPLY_LEN, 64);
    }

    #[test]
    fn cookie_reply_round_trip() {
        let nonce: [u8; XNONCE_LEN] = std::array::from_fn(|i| i as u8);
        let sealed = [0xc0; COOKIE_LEN];
        let tag = [0x7a; TAG_LEN];
        let packet = cookie_reply_packet(0x0102_0304, &nonce, &sealed, &tag);
        assert_eq!(packet[..8], [0x13, 0, 0, 0, 4, 3, 2, 1]);
        assert_eq!(packet_type(&packet), Some(PacketType::CookieReply));
        assert_eq!(cookie_reply_receiver_index(&packet), Some(0x0102_0304));

        let view = parse_cookie_reply(&packet).expect("parses");
        assert_eq!(*view.nonce, nonce);
        assert_eq!(*view.sealed_cookie, sealed);
        assert_eq!(*view.tag, tag);

        assert!(parse_cookie_reply(&packet[..63]).is_none());
        let mut longer = packet.to_vec();
        longer.push(0);
        assert!(parse_cookie_reply(&longer).is_none());
        let mut reserved = packet;
        reserved[3] = 1;
        assert!(parse_cookie_reply(&reserved).is_none());
    }
}
