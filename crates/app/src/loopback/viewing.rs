// The viewer's thread: share::Screen on the loopback's link. Its answers go
// back through the network, its numbers once a second to the log, and a
// timed run ends after that many seconds' numbers.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Sender;
use std::time::Duration;

use share::{Line, Report, Screen, Second, ViewerNumbers, Watch, path_word, spread_text};

use super::link::Link;
use super::network::Network;

pub fn run(
    watch: &Watch,
    seconds: Option<Duration>,
    link: &Arc<Link>,
    network: &Network,
    lines: &Sender<Line>,
    ready: Sender<()>,
) -> Result<ViewerNumbers, String> {
    let mut say = |line| {
        let _ = lines.send(line);
    };
    let screen = Screen::open(watch, Arc::clone(&link.inbox), &mut say)?;
    // The viewer's own device has the last word for a share left to pick
    // its codec, as in a room.
    link.takes_hevc
        .store(screen.takes_hevc(), Ordering::Relaxed);
    // The sharer starts sending now.
    let _ = ready.send(());
    screen.run(&mut |report| match report {
        Report::Back(message) => network.back(message),
        Report::Line(line) => {
            let _ = lines.send(line);
        }
        Report::Second(second) => {
            let elapsed = second.elapsed;
            let parity = link.parity.load(Ordering::Relaxed);
            let _ = lines.send(Line::Log(second_line(second, parity)));
            if seconds.is_some_and(|seconds| elapsed >= seconds) {
                link.stop();
            }
        }
        // The stats panel opens from the strip in the panel window, and the
        // loopback has none.
        Report::StripClicked => {}
        Report::NoHevc => link.takes_hevc.store(false, Ordering::Relaxed),
    })
}

fn second_line(second: Second<'_>, parity: u32) -> String {
    let Second {
        elapsed,
        loss,
        mut window,
        numbers,
    } = second;
    let reassembly = &numbers.reassembly;
    format!(
        "{:.0} s: presented {} in {}, loss {} (parity {parity}%), encode {}, decode on the GPU {}, decode call {}, capture to display {} ms; so far repaired {}, dropped {}, skipped {}, not decoded {}, {}",
        elapsed.as_secs_f32(),
        window.presented,
        numbers
            .codec
            .map_or_else(|| String::from("no codec yet"), |codec| codec.to_string()),
        loss.map_or_else(|| String::from("not measured"), |pct| format!("{pct:.1}%")),
        spread_text(&mut window.encode_ms),
        spread_text(&mut window.decode_ms),
        spread_text(&mut window.decode_call_ms),
        spread_text(&mut window.end_to_end_ms),
        reassembly.repaired,
        reassembly.dropped(),
        reassembly.skipped + numbers.before_first_idr,
        numbers.decode_failed,
        path_word(numbers.path),
    )
}
