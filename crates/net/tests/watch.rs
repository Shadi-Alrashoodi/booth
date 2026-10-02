use std::sync::Arc;
use std::thread;

use net::watch::{self, AddressWatch};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessHandleCount};

// The token's count tells whether the box holding the callback was freed.
fn start_holding(token: &Arc<()>) -> AddressWatch {
    let held = Arc::clone(token);
    watch::start(move || {
        let _ = &held;
    })
    .unwrap()
}

fn handle_count() -> u32 {
    let mut count = 0;
    // SAFETY: GetCurrentProcess returns a pseudo handle that needs no closing,
    // and `count` is a live u32 for the call to fill.
    let ok = unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut count) };
    assert_ne!(ok, 0, "could not read this process's handle count");
    count
}

// A live registration holds about eight handles on Windows 11, so a missed
// cancel shows up in the thousands here. The slack is for the thread pool
// threads Windows starts and stops on its own.
#[test]
fn start_and_drop_many_times() {
    let token = Arc::new(());
    // Whatever Windows opens once for the whole process is opened here.
    for _ in 0..10 {
        drop(start_holding(&token));
    }
    let before = handle_count();

    for round in 0..1000 {
        let watch = start_holding(&token);
        assert_eq!(Arc::strong_count(&token), 2, "round {round}");
        drop(watch);
        assert_eq!(Arc::strong_count(&token), 1, "round {round}");
    }

    let watches: Vec<_> = (0..100).map(|_| start_holding(&token)).collect();
    assert_eq!(Arc::strong_count(&token), 101);
    thread::spawn(move || drop(watches)).join().unwrap();
    assert_eq!(Arc::strong_count(&token), 1);

    let after = handle_count();
    assert!(
        after < before + 500,
        "handles went from {before} to {after} over 1100 watches"
    );
}
