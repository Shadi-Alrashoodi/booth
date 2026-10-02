mod common;

use std::fs;
use std::io;
use std::sync::Barrier;
use std::thread;

use common::TempDir;
use keys::{KeyError, put_aside, read_protected, write_protected};

#[test]
fn dpapi_round_trip() {
    let dir = TempDir::new("round-trip");
    let path = dir.path().join("peer.key");
    let big: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
    for data in [&b"short secret"[..], &[0xa5; 32], &big] {
        write_protected(&path, data, "Booth test secret").expect("write");
        let bytes = fs::read(&path).expect("read raw");
        assert!(bytes.starts_with(b"BOOTHKEY\x01"));
        assert!(!bytes.windows(data.len()).any(|w| w == data));
        assert_eq!(read_protected(&path).expect("read").as_slice(), data);
    }
}

#[test]
fn empty_data_round_trips() {
    let dir = TempDir::new("empty");
    let path = dir.path().join("empty.key");
    write_protected(&path, &[], "Booth test secret").expect("write");
    assert!(read_protected(&path).expect("read").is_empty());
}

#[test]
fn rewrite_leaves_no_temp_file() {
    let dir = TempDir::new("rewrite");
    let path = dir.path().join("known.key");
    write_protected(&path, b"first", "Booth test").expect("first write");
    write_protected(&path, b"second", "Booth test").expect("second write");
    assert_eq!(read_protected(&path).expect("read").as_slice(), b"second");
    assert_eq!(dir.file_names(), ["known.key"]);
}

#[test]
fn stale_temp_file_does_not_block_a_write() {
    let dir = TempDir::new("stale-tmp");
    let path = dir.path().join("known.key");
    // Process id 0 is the idle process, so no live writer uses that name.
    // Another writer's temp file is not ours to delete, even an old one.
    for stale in ["known.key.tmp", "known.key.0-0.tmp"] {
        fs::write(dir.path().join(stale), b"left over from a crash").expect("write tmp");
    }
    write_protected(&path, b"fresh", "Booth test").expect("write");
    assert_eq!(read_protected(&path).expect("read").as_slice(), b"fresh");
    assert_eq!(
        dir.file_names(),
        ["known.key", "known.key.0-0.tmp", "known.key.tmp"]
    );
}

#[test]
fn missing_file_reports_not_found() {
    let dir = TempDir::new("missing");
    let path = dir.path().join("nothing.key");
    let err = read_protected(&path).expect_err("no file");
    assert!(
        matches!(&err, KeyError::Read { source, .. } if source.kind() == io::ErrorKind::NotFound),
        "{err:?}"
    );
    assert!(err.to_string().contains("nothing.key"));
}

#[test]
fn write_into_missing_folder_names_the_path() {
    let dir = TempDir::new("no-folder");
    let path = dir.path().join("not-here").join("peer.key");
    let err = write_protected(&path, b"x", "Booth test").expect_err("no folder");
    assert!(matches!(err, KeyError::Write { .. }), "{err:?}");
    assert!(err.to_string().starts_with("could not save "), "{err}");
    assert!(err.to_string().contains("not-here"), "{err}");
}

#[test]
fn wrong_magic_or_version_is_rejected() {
    let dir = TempDir::new("format");
    let path = dir.path().join("peer.key");
    write_protected(&path, b"secret", "Booth test").expect("write");
    let good = fs::read(&path).expect("read raw");

    let mut wrong_magic = good.clone();
    if let Some(first) = wrong_magic.first_mut() {
        *first = b'X';
    }
    fs::write(&path, &wrong_magic).expect("write");
    let err = read_protected(&path).expect_err("wrong magic");
    assert!(matches!(err, KeyError::NotAKeyFile { .. }), "{err:?}");
    assert!(err.to_string().contains("not a Booth key file"), "{err}");
    assert!(err.to_string().ends_with(NEXT_STEP), "{err}");

    let mut wrong_version = good.clone();
    if let Some(version) = wrong_version.get_mut(8) {
        *version = 9;
    }
    fs::write(&path, &wrong_version).expect("write");
    let err = read_protected(&path).expect_err("wrong version");
    assert!(
        matches!(err, KeyError::UnknownVersion { version: 9, .. }),
        "{err:?}"
    );
    assert!(
        err.to_string()
            .ends_with("Run the newest version of Booth."),
        "{err}"
    );
}

const NEXT_STEP: &str = "Delete the file. Any device it was for will need a new invite.";

#[test]
fn undecryptable_file_says_what_to_do() {
    let dir = TempDir::new("decrypt");
    let path = dir.path().join("peer.key");
    write_protected(&path, b"secret", "Booth test").expect("write");
    let mut bytes = fs::read(&path).expect("read raw");
    let last = bytes.last_mut().expect("file is not empty");
    *last ^= 0x01;
    fs::write(&path, &bytes).expect("write");

    let err = read_protected(&path).expect_err("tampered");
    assert!(matches!(err, KeyError::Decrypt { .. }), "{err:?}");
    let message = err.to_string();
    assert!(message.contains("peer.key"), "{message}");
    assert!(message.contains(NEXT_STEP), "{message}");
    assert!(message.contains("os error"), "{message}");
}

#[test]
fn oversized_file_is_rejected() {
    let dir = TempDir::new("oversized");
    let path = dir.path().join("huge.key");
    let mut bytes = b"BOOTHKEY\x01".to_vec();
    bytes.resize(2 << 20, 0);
    fs::write(&path, &bytes).expect("write");
    let err = read_protected(&path).expect_err("too large");
    assert!(matches!(err, KeyError::NotAKeyFile { .. }), "{err:?}");
}

// Every writer to one file must see its own write succeed, and the file must
// end up holding one complete value, never a mix and never an error that
// names a file the writer did not touch.
#[test]
fn concurrent_writers_leave_one_whole_value() {
    const THREADS: u8 = 4;
    let dir = TempDir::new("writers");
    let path = dir.path().join("known.key");
    for round in 0..30 {
        let barrier = Barrier::new(THREADS.into());
        thread::scope(|s| {
            for i in 0..THREADS {
                let (barrier, path) = (&barrier, &path);
                s.spawn(move || {
                    let data = vec![i; 100 + usize::from(i) * 50];
                    barrier.wait();
                    write_protected(path, &data, "Booth test").expect("concurrent write");
                });
            }
        });
        let back = read_protected(&path).expect("read");
        let first = *back.first().expect("not empty");
        assert!(first < THREADS, "round {round}");
        assert_eq!(back.len(), 100 + usize::from(first) * 50, "round {round}");
        assert!(back.iter().all(|&b| b == first), "round {round}");
        assert_eq!(dir.file_names(), ["known.key"], "round {round}");
    }
}

#[test]
fn replace_while_open() {
    let dir = TempDir::new("open-reader");
    let path = dir.path().join("known.key");
    write_protected(&path, b"old", "Booth test").expect("first write");
    let reader = fs::File::open(&path).expect("open for reading");
    write_protected(&path, b"new", "Booth test").expect("replace while open");
    drop(reader);
    assert_eq!(read_protected(&path).expect("read").as_slice(), b"new");
    assert_eq!(dir.file_names(), ["known.key"]);
}

// Whatever the writer accepts, the reader must accept too. Otherwise a list
// that grew past the reader's limit would be saved fine and then be reported
// as damaged on the next start, losing all of it.
#[test]
fn writer_and_reader_share_the_limit() {
    const LIMIT: usize = 1 << 20;
    let dir = TempDir::new("limit");
    let path = dir.path().join("known.key");
    write_protected(&path, b"good", "Booth test").expect("first write");

    let (mut accepted, mut refused) = (0, 0);
    for len in [LIMIT - 4096, LIMIT - 400, LIMIT - 300, LIMIT - 200, LIMIT] {
        match write_protected(&path, &vec![0x42; len], "Booth test") {
            Ok(()) => {
                let back = read_protected(&path).expect("what was written must read back");
                accepted += 1;
                assert_eq!(back.len(), len);
                write_protected(&path, b"good", "Booth test").expect("restore");
            }
            Err(err) => {
                assert!(matches!(err, KeyError::TooLarge { .. }), "{err:?}");
                let message = err.to_string();
                assert!(message.starts_with("could not save "), "{message}");
                assert!(message.contains(&LIMIT.to_string()), "{message}");
                assert_eq!(read_protected(&path).expect("old file").as_slice(), b"good");
                refused += 1;
            }
        }
    }
    // DPAPI adds a few hundred bytes, so the sizes above land on both sides.
    assert!(
        accepted >= 1 && refused >= 1,
        "{accepted} accepted, {refused} refused"
    );
    assert_eq!(dir.file_names(), ["known.key"]);
}

// A list Booth cannot read goes aside under a name of its own, and an older
// one already there is never written over.
#[test]
fn put_aside_keeps_older_copies() {
    let dir = TempDir::new("aside");
    let path = dir.path().join("hosts.bin");
    fs::write(&path, b"first").expect("write");
    let aside = put_aside(&path).expect("put aside");
    assert_eq!(aside, dir.path().join("hosts.bin.bad"));

    fs::write(&path, b"second").expect("write");
    assert_eq!(
        put_aside(&path).expect("put aside again"),
        dir.path().join("hosts.bin.bad2")
    );
    assert_eq!(fs::read(&aside).expect("the first copy"), b"first");
    assert_eq!(dir.file_names(), ["hosts.bin.bad", "hosts.bin.bad2"]);

    for n in 3..=9 {
        fs::write(dir.path().join(format!("hosts.bin.bad{n}")), b"older").expect("write");
    }
    fs::write(&path, b"third").expect("write");
    let err = put_aside(&path).expect_err("every name is taken");
    assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(err.to_string(), ".bad to .bad9 are all taken");
    assert_eq!(fs::read(&path).expect("left where it was"), b"third");

    let err = put_aside(&dir.path().join("nothing.bin")).expect_err("no file");
    assert_eq!(err.kind(), io::ErrorKind::NotFound);
}
