// The room's timer thread, which waits on the high-resolution timer because
// the default Windows one wakes up to 15 ms late: it sends the pings, the
// retransmits and the handshake retries, and the recover requests a viewer
// gathers for 20 ms. Here a host and a friend ping each other every 10 ms for
// three seconds, about 300 deadlines on each side, and each side's view says
// how late its timer thread woke for them.

mod common;

use std::thread;
use std::time::Duration;

use common::{Member, host_invite, timers};
use room::Timers;
use room::view::{LinkState, Spread};

const PING: Duration = Duration::from_millis(10);
const RUN: Duration = Duration::from_secs(3);
// Two thirds of the pings, so the 99th percentile is the fourth latest wake
// or so, not the latest of a few dozen.
const FEWEST: usize = 200;
// The claim, and what catches a timer back on the default 15.6 ms tick:
// forced onto the channel's own timed wait, the thread woke 2.5 to 5.8 ms
// late at the median. On my PC, 28 logical CPUs, it was 0.28 to
// 0.38 ms quiet and 0.05 to 0.09 ms with all 28 kept busy at normal
// priority, which keeps them from resting. Before the thread ran at the
// highest priority (net::pace::raise_priority), all 28 busy took it up to
// 6.7 ms, the thread waiting its turn for a CPU.
const MOST_MEDIAN_MS: f32 = 1.0;
// The tail: 0.5 to 0.8 ms quiet or with all 28 CPUs busy. At normal
// priority it followed the CPU more than the timer: up to 3.1 ms with 22
// busy and 6 to 24 ms with all 28 (the default tick's was 11 to 15 ms).
// Past 20 ms something holds the thread itself.
const MOST_P99_MS: f32 = 20.0;

fn late(spread: Option<Spread>, who: &str) -> Spread {
    let spread = spread.unwrap_or_else(|| panic!("{who}: no wakes measured"));
    println!(
        "{who}: the timer thread woke {:.2} ms late at the median, {:.2} ms at the 99th percentile, over {} deadlines",
        spread.median_ms, spread.p99_ms, spread.count
    );
    spread
}

#[test]
fn timer_thread_wakes_on_time() {
    let timers = Timers {
        ping_idle: PING,
        ..timers()
    };
    let host = Member::host("Mara", timers);
    let ana = Member::join("Ana", timers, host_invite(&host));
    ana.wait_for(Duration::from_secs(3), "Ana live", |v| {
        v.strip.state == LinkState::Live && v.people.len() == 2
    });
    thread::sleep(RUN);

    let spreads = [("the host", &host), ("Ana", &ana)]
        .map(|(who, member)| (who, late(member.view().numbers.timer_late, who)));
    for (who, spread) in spreads {
        assert!(spread.count >= FEWEST, "{who}: {spread:?}");
        assert!(spread.median_ms < MOST_MEDIAN_MS, "{who}: {spread:?}");
        assert!(spread.p99_ms < MOST_P99_MS, "{who}: {spread:?}");
    }
}
