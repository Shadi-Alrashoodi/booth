use session::NOISE_PATTERN;
use snow::Builder;

// From the cacophony test vectors, vectors/cacophony.txt at
// https://github.com/haskell-cryptography/cacophony (the same file ships with snow 0.10.0 as
// tests/vectors/cacophony.txt; both copies were compared byte for byte). This is the entry
// "Noise_IKpsk2_25519_ChaChaPoly_BLAKE2s" and nothing else from that file.
const PROLOGUE: &str = "4a6f686e2047616c74";
const PSK: &str = "54686973206973206d7920417573747269616e20706572737065637469766521";
const INIT_STATIC: &str = "e61ef9919cde45dd5f82166404bd08e38bceb5dfdfded0a34c8df7ed542214d1";
const INIT_EPHEMERAL: &str = "893e28b9dc6ca8d611ab664754b8ceb7bac5117349a4439a6b0569da977c464a";
const INIT_REMOTE_STATIC: &str = "31e0303fd6418d2f8c0e78b91f22e8caed0fbe48656dcf4767e4834f701b8f62";
const RESP_STATIC: &str = "4a3acbfdb163dec651dfa3194dece676d437029c62a408b4c5ea9114246e4893";
const RESP_EPHEMERAL: &str = "bbdb4cdbd309f1a1f2e1456967fe288cadd6f712d65dc7b7793d5e63da6b375b";
const HANDSHAKE_HASH: &str = "f5191b875290abcd41347ac3622d9679688a7e980229cb937ef748336cfde0e5";

// (payload, ciphertext). The first two are the handshake, then transport messages alternate
// initiator, responder, starting from the initiator.
const MESSAGES: [(&str, &str); 6] = [
    (
        "4c756477696720766f6e204d69736573",
        "ca35def5ae56cec33dc2036731ab14896bc4c75dbb07a61f879f8e3afa4c7944001e21de9f98ddd8e2ad57527207feb56253c9c94a9e496782ecfcb2a75fbcaf1b52948cc48daefe660c62119ab5000980c84831215f2441eba616548e832985464cf17e51ee93109008399a21f7e13f",
    ),
    (
        "4d757272617920526f746862617264",
        "95ebc60d2b1fa672c1f46a8aa265ef51bfe38e7ccb39ec5be34069f144808843cb765f2caef0751b8f007572dab0322217755c0632f365717edbf34d33e87a",
    ),
    (
        "462e20412e20486179656b",
        "8153ca9833bc3c1b91a7e66e5f4d4f5b59bf9e64c2f20d15f0bba7",
    ),
    (
        "4361726c204d656e676572",
        "07af0c9c86e1b4e80f36b04ff7688d51141af3debd0332f0a705ef",
    ),
    (
        "4a65616e2d426170746973746520536179",
        "6ab1467c0448cc78394494abaaf23afce0e234315d6e2624dcbfa8a21c1c4d073d",
    ),
    (
        "457567656e2042f6686d20766f6e2042617765726b",
        "dfc346c0d2296ae6cf1acf6f12b8456a1dba228cf8d8b774aacf1c47fc53aa80ebc7a4c292",
    ),
];

fn hex(text: &str) -> Vec<u8> {
    assert!(text.len().is_multiple_of(2), "odd length hex");
    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&text[at..at + 2], 16).expect("hex digit"))
        .collect()
}

#[test]
fn ikpsk2_matches_the_cacophony_vector() {
    let prologue = hex(PROLOGUE);
    let psk: [u8; 32] = hex(PSK).try_into().expect("32 byte psk");
    let init_static = hex(INIT_STATIC);
    let init_ephemeral = hex(INIT_EPHEMERAL);
    let init_remote_static = hex(INIT_REMOTE_STATIC);
    let resp_static = hex(RESP_STATIC);
    let resp_ephemeral = hex(RESP_EPHEMERAL);

    let mut initiator = Builder::new(NOISE_PATTERN.parse().expect("pattern parses"))
        .local_private_key(&init_static)
        .and_then(|b| b.remote_public_key(&init_remote_static))
        .and_then(|b| b.prologue(&prologue))
        .and_then(|b| b.psk(2, &psk))
        .map(|b| b.fixed_ephemeral_key_for_testing_only(&init_ephemeral))
        .and_then(|b| b.build_initiator())
        .expect("initiator");

    // No psk yet: the host reads message 1 first and sets the psk afterwards, as accept does.
    let mut responder = Builder::new(NOISE_PATTERN.parse().expect("pattern parses"))
        .local_private_key(&resp_static)
        .and_then(|b| b.prologue(&prologue))
        .map(|b| b.fixed_ephemeral_key_for_testing_only(&resp_ephemeral))
        .and_then(|b| b.build_responder())
        .expect("responder");

    let mut wire = vec![0u8; 1024];
    let mut plain = vec![0u8; 1024];

    let (payload, ciphertext) = (hex(MESSAGES[0].0), hex(MESSAGES[0].1));
    let len = initiator
        .write_message(&payload, &mut wire)
        .expect("write 1");
    assert_eq!(wire[..len], ciphertext[..], "message 1");
    let len = responder
        .read_message(&ciphertext, &mut plain)
        .expect("read 1");
    assert_eq!(plain[..len], payload[..]);
    assert_eq!(
        responder.get_remote_static(),
        Some(&initiator_public(&init_static)[..])
    );

    responder.set_psk(2, &psk).expect("set psk");
    let (payload, ciphertext) = (hex(MESSAGES[1].0), hex(MESSAGES[1].1));
    let len = responder
        .write_message(&payload, &mut wire)
        .expect("write 2");
    assert_eq!(wire[..len], ciphertext[..], "message 2");
    let len = initiator
        .read_message(&ciphertext, &mut plain)
        .expect("read 2");
    assert_eq!(plain[..len], payload[..]);

    assert_eq!(initiator.get_handshake_hash(), &hex(HANDSHAKE_HASH)[..]);
    assert_eq!(responder.get_handshake_hash(), &hex(HANDSHAKE_HASH)[..]);

    let initiator = initiator
        .into_stateless_transport_mode()
        .expect("transport");
    let responder = responder
        .into_stateless_transport_mode()
        .expect("transport");
    for (at, (payload, ciphertext)) in MESSAGES.iter().enumerate().skip(2) {
        let (payload, ciphertext) = (hex(payload), hex(ciphertext));
        let nonce = (at as u64 - 2) / 2;
        let (sender, receiver) = if at % 2 == 0 {
            (&initiator, &responder)
        } else {
            (&responder, &initiator)
        };
        let len = sender
            .write_message(nonce, &payload, &mut wire)
            .expect("write transport");
        assert_eq!(wire[..len], ciphertext[..], "message {}", at + 1);
        let len = receiver
            .read_message(nonce, &ciphertext, &mut plain)
            .expect("read transport");
        assert_eq!(plain[..len], payload[..]);
    }
}

fn initiator_public(private: &[u8]) -> Vec<u8> {
    use snow::params::DHChoice;
    use snow::resolvers::{CryptoResolver, DefaultResolver};
    let mut dh = DefaultResolver
        .resolve_dh(&DHChoice::Curve25519)
        .expect("curve25519");
    dh.set(private);
    dh.pubkey().to_vec()
}
