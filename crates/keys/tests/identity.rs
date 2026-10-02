mod common;

use std::fs;
use std::sync::Barrier;
use std::thread;

use common::TempDir;
use keys::{IDENTITY_FILE, Identity, KeyError, fingerprint, write_protected};
use x25519_dalek::{PublicKey, StaticSecret};

#[test]
fn second_load_gives_the_same_key() {
    let dir = TempDir::new("same-key");
    let first = Identity::load_or_create(dir.path()).expect("create");
    let second = Identity::load_or_create(dir.path()).expect("load");
    assert_eq!(first.public(), second.public());
    assert_eq!(*first.private_bytes(), *second.private_bytes());
    assert_eq!(dir.file_names(), [IDENTITY_FILE]);
}

#[test]
fn public_key_is_x25519_of_the_secret() {
    let dir = TempDir::new("x25519");
    let created = Identity::load_or_create(dir.path()).expect("create");
    let loaded = Identity::load_or_create(dir.path()).expect("load");
    for identity in [Identity::generate(), created, loaded] {
        let secret = StaticSecret::from(*identity.private_bytes());
        assert_eq!(PublicKey::from(&secret).as_bytes(), identity.public());
    }
}

#[test]
fn new_keys_differ() {
    assert_ne!(Identity::generate().public(), Identity::generate().public());
}

#[test]
fn key_file_is_encrypted_and_tagged() {
    let dir = TempDir::new("layout");
    let identity = Identity::load_or_create(dir.path()).expect("create");
    let bytes = fs::read(dir.path().join(IDENTITY_FILE)).expect("read key file");
    assert!(bytes.starts_with(b"BOOTHKEY\x01"));
    let secret = identity.private_bytes();
    assert!(!bytes.windows(32).any(|w| w == secret.as_slice()));
}

// Every broken-file case must fail and leave the file exactly as it was: a
// quiet new identity would lock this PC out of every host that knows it.
fn assert_rejected_and_untouched(test: &str, contents: &[u8]) -> KeyError {
    let dir = TempDir::new(test);
    let path = dir.path().join(IDENTITY_FILE);
    fs::write(&path, contents).expect("write test file");
    let err = Identity::load_or_create(dir.path()).expect_err("broken file must not load");
    assert_eq!(fs::read(&path).expect("read back"), contents);
    assert_eq!(dir.file_names(), [IDENTITY_FILE]);
    err
}

#[test]
fn corrupted_file() {
    let dir = TempDir::new("corrupt-source");
    Identity::load_or_create(dir.path()).expect("create");
    let mut bytes = fs::read(dir.path().join(IDENTITY_FILE)).expect("read key file");
    let last = bytes.last_mut().expect("file is not empty");
    *last ^= 0x01;

    let err = assert_rejected_and_untouched("corrupt", &bytes);
    assert!(matches!(err, KeyError::Decrypt { .. }), "{err:?}");
    let message = err.to_string();
    assert!(
        message.starts_with("could not read the identity key at "),
        "{message}"
    );
    assert!(message.contains(IDENTITY_FILE), "{message}");
    assert!(
        message.contains("Windows could not decrypt it"),
        "{message}"
    );
    assert!(
        message.contains("Delete the file to make a new identity"),
        "{message}"
    );
    assert!(message.contains("os error"), "{message}");
}

#[test]
fn foreign_file() {
    for (i, contents) in [&b""[..], b"hello", b"BOOTHKEY", b"BOOTHKEY\x01"]
        .into_iter()
        .enumerate()
    {
        let err = assert_rejected_and_untouched(&format!("foreign{i}"), contents);
        assert!(matches!(err, KeyError::NotAKeyFile { .. }), "{err:?}");
    }
}

#[test]
fn newer_format() {
    let err = assert_rejected_and_untouched("version", b"BOOTHKEY\x02some future blob");
    assert!(
        matches!(err, KeyError::UnknownVersion { version: 2, .. }),
        "{err:?}"
    );
    assert!(!err.to_string().contains("Delete the file"));
    assert!(
        err.to_string()
            .ends_with("Run the newest version of Booth."),
        "{err}"
    );
}

#[test]
fn wrong_secret_length() {
    let dir = TempDir::new("length");
    let path = dir.path().join(IDENTITY_FILE);
    write_protected(&path, &[7u8; 16], "test").expect("write");
    let before = fs::read(&path).expect("read");
    let err = Identity::load_or_create(dir.path()).expect_err("16 bytes is not a key");
    assert!(
        matches!(err, KeyError::WrongKeyLength { len: 16, .. }),
        "{err:?}"
    );
    assert_eq!(fs::read(&path).expect("read back"), before);
}

#[test]
fn unreadable_path() {
    let dir = TempDir::new("unreadable");
    let path = dir.path().join(IDENTITY_FILE);
    fs::create_dir(&path).expect("make a folder where the key should be");
    let err = Identity::load_or_create(dir.path()).expect_err("folder is not a key");
    assert!(matches!(err, KeyError::Read { .. }), "{err:?}");
    assert!(path.is_dir());
}

#[test]
fn debug_does_not_show_the_secret() {
    let identity = Identity::generate();
    let debug = format!("{identity:?}");
    let secret = identity.private_bytes();
    let hex: String = secret.iter().map(|b| format!("{b:02x}")).collect();
    assert!(!debug.contains(&hex));
    assert!(!debug.contains(&format!("{:?}", *secret)));
    assert!(debug.contains(&identity.fingerprint()));
}

#[test]
fn fingerprint_format() {
    let fp = Identity::generate().fingerprint();
    assert_eq!(fp.len(), 14);
    let groups: Vec<&str> = fp.split(' ').collect();
    assert_eq!(groups.len(), 3);
    for group in groups {
        assert_eq!(group.len(), 4);
        assert!(
            group
                .chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
        );
    }
}

#[test]
fn fingerprint_is_stable() {
    // Expected values from `openssl dgst -blake2s256` over the same 32 bytes.
    assert_eq!(fingerprint(&[0u8; 32]), "320b 5ea9 9e65");
    let counting: [u8; 32] = std::array::from_fn(|i| i as u8);
    assert_eq!(fingerprint(&counting), "0582 5607 d7fd");

    let identity = Identity::generate();
    assert_eq!(identity.fingerprint(), fingerprint(identity.public()));
}

// Two copies of Booth started together on a fresh profile must end up on the
// one key that is on disk. If either kept a key that lost the race, every
// host that pinned it would stop knowing this PC after the next start.
#[test]
fn concurrent_first_runs() {
    const THREADS: usize = 4;
    for round in 0..30 {
        let dir = TempDir::new(&format!("race{round}"));
        let barrier = Barrier::new(THREADS);
        let publics: Vec<[u8; 32]> = thread::scope(|s| {
            let handles: Vec<_> = (0..THREADS)
                .map(|_| {
                    s.spawn(|| {
                        barrier.wait();
                        *Identity::load_or_create(dir.path())
                            .expect("load or create")
                            .public()
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("thread"))
                .collect()
        });
        let on_disk = *Identity::load_or_create(dir.path()).expect("load").public();
        for public in &publics {
            assert_eq!(public, &on_disk, "round {round}");
        }
        assert_eq!(dir.file_names(), [IDENTITY_FILE], "round {round}");
    }
}
