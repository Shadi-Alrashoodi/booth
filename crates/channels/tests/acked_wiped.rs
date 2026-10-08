// A PeerSecret goes to a friend on the host's control stream, and the stream
// keeps a copy until the friend acks it. No buffer of the stream may be
// freed with the secret still in it: not once the message is acked, and not
// when the stream is dropped with it in flight, queued or resent. This file
// is its own test binary, so its allocator can look into every block freed
// while a test watches. Watched per thread, since the tests in this file run
// side by side.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::hint::black_box;
use std::time::{Duration, Instant};

use channels::Reliable;

const SECRET: usize = 32;
// The byte an encoded PeerSecret starts with, before the secret itself.
const PEER_SECRET: u8 = 2;

thread_local! {
    // The secret to look for, while this thread watches.
    static LOOKING_FOR: Cell<Option<[u8; SECRET]>> = const { Cell::new(None) };
    static FOUND: Cell<usize> = const { Cell::new(0) };
}

struct Watcher;

// SAFETY: every call is passed straight to the system allocator with the
// same arguments. dealloc first reads the block it is about to free, which
// is still allocated, and touches only thread-local cells.
unsafe impl GlobalAlloc for Watcher {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's contract for alloc, passed on unchanged.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let _ = LOOKING_FOR.try_with(|looking_for| {
            if let Some(secret) = looking_for.get()
                // SAFETY: the block stays allocated until System.dealloc below.
                && unsafe { holds(ptr, layout.size(), &secret) }
            {
                let _ = FOUND.try_with(|found| found.set(found.get() + 1));
            }
        });
        // SAFETY: as above.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static WATCHER: Watcher = Watcher;

// Whether the secret is anywhere in the block. Read with volatile reads,
// since parts of a block may never have been written.
//
// SAFETY: the caller passes an allocated block of `len` bytes.
unsafe fn holds(ptr: *const u8, len: usize, secret: &[u8; SECRET]) -> bool {
    (0..len.saturating_sub(SECRET - 1)).any(|start| {
        secret.iter().enumerate().all(|(i, &byte)| {
            // SAFETY: start + i < len, so the byte is inside the block.
            unsafe { ptr.add(start + i).read_volatile() == byte }
        })
    })
}

// How many blocks freed while `f` runs still held the secret.
fn freed_holding(secret: &[u8; SECRET], f: impl FnOnce()) -> usize {
    LOOKING_FOR.set(Some(*secret));
    FOUND.set(0);
    // A block of our own first, so a zero below means nothing was found,
    // not that nothing was looked at.
    drop(black_box(secret.to_vec()));
    assert_eq!(FOUND.get(), 1, "the allocator did not see a freed secret");
    FOUND.set(0);
    f();
    LOOKING_FOR.set(None);
    FOUND.get()
}

// Made at run time, so it is in no buffer the test did not put it in. No
// zero bytes, so a wiped buffer never matches.
fn secret() -> [u8; SECRET] {
    let mut state = black_box(0x9E37_79B9_7F4A_7C15u64);
    let mut secret = [0; SECRET];
    for byte in &mut secret {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        *byte = (state >> 56) as u8 | 1;
    }
    secret
}

// What a PeerSecret encodes to, 33 bytes on the stack.
fn peer_secret(secret: &[u8; SECRET]) -> [u8; 1 + SECRET] {
    let mut message = [PEER_SECRET; 1 + SECRET];
    message[1..].copy_from_slice(secret);
    message
}

// Frames and delivered messages are the caller's to wipe, as the room does.
fn wipe(mut bytes: Vec<u8>) {
    bytes.fill(0);
    black_box(&bytes);
}

#[test]
fn acked_message_is_wiped() {
    let secret = secret();
    let now = Instant::now();
    let mut host = Reliable::new();
    let mut friend = Reliable::new();
    host.send(&peer_secret(&secret)).unwrap();
    let frame = host.poll_transmit(now, None).unwrap();
    friend.receive(&frame, now).unwrap();
    wipe(frame);
    wipe(friend.next_delivered().unwrap());
    let ack = friend.poll_transmit(now, None).unwrap();

    let freed = freed_holding(&secret, || host.receive(&ack, now).unwrap());
    assert_eq!(host.counters().in_flight, 0, "the ack was not taken");
    assert_eq!(freed, 0, "the acked message was freed unwiped");
}

#[test]
fn message_in_flight_is_wiped_when_dropped() {
    let secret = secret();
    let now = Instant::now();
    let mut host = Reliable::new();
    host.send(&peer_secret(&secret)).unwrap();
    wipe(host.poll_transmit(now, None).unwrap());
    assert_eq!(host.counters().in_flight, 1);

    let freed = freed_holding(&secret, || drop(host));
    assert_eq!(freed, 0, "a message in flight was freed unwiped");
}

#[test]
fn queued_message_is_wiped_when_dropped() {
    let secret = secret();
    let mut host = Reliable::new();
    host.send(&peer_secret(&secret)).unwrap();
    assert_eq!(host.counters().queued, 1);

    let freed = freed_holding(&secret, || drop(host));
    assert_eq!(freed, 0, "a queued message was freed unwiped");
}

// A resend builds its frame from the kept message and leaves no copy of its
// own behind.
#[test]
fn resent_message_is_wiped_when_dropped() {
    let secret = secret();
    let now = Instant::now();
    let mut host = Reliable::new();
    host.send(&peer_secret(&secret)).unwrap();
    wipe(host.poll_transmit(now, None).unwrap());
    let later = now + Duration::from_secs(1);
    wipe(host.poll_transmit(later, None).unwrap());
    assert_eq!(host.counters().retransmissions, 1);

    let freed = freed_holding(&secret, || drop(host));
    assert_eq!(freed, 0, "a resent message was freed unwiped");
}
