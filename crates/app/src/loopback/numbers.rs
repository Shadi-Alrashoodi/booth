// How a run is summed up at the end, in one line; tests/loopback.rs reads it.

use std::fmt::Write;
use std::time::Duration;

use share::{ViewerNumbers, path_word, spread_text};

use super::network::NetworkNumbers;
use super::sharer::Shared;

pub struct Totals<'a> {
    pub shared: &'a mut Shared,
    pub viewer: &'a mut ViewerNumbers,
    pub knob_dropped: u64,
    pub overflow: u64,
    // None without a network between the two sides.
    pub network: Option<NetworkNumbers>,
}

// In the order the stages come.
pub fn summary(totals: Totals<'_>) -> String {
    let Totals {
        shared,
        viewer,
        knob_dropped,
        overflow,
        network,
    } = totals;
    let sharer = &mut shared.numbers;
    let reassembly = &viewer.reassembly;
    let sent_for = sharer.ran.as_secs_f64().max(0.001);
    let codec = shared
        .codec
        .map_or_else(|| String::from("no codec"), |codec| codec.to_string());
    let mut line = format!(
        "{:.1} s: captured {} (held {}, skipped by the cap {}), encoded {} in {codec} (IDRs {}, codec changes {}, {:.2} Mbit/s), recoveries {} ({} by invalidation, {} by IDR, {} already covered), IDRs the viewer asked for {}, packets sent {}, dropped by the knob {}",
        viewer.ran.as_secs_f64(),
        sharer.captured,
        sharer.held,
        sharer.skipped,
        sharer.encoded,
        sharer.idrs,
        sharer.codec_changes,
        sharer.bytes as f64 * 8.0 / sent_for / 1e6,
        sharer.recoveries,
        sharer.invalidated,
        sharer.idr_answers,
        sharer.covered,
        sharer.idr_asks,
        sharer.pace.packets,
        knob_dropped,
    );
    if sharer.pace.discarded > 0 || sharer.too_big > 0 || overflow > 0 {
        let _ = write!(
            line,
            " (discarded by the pacer {}, too big to send {}, dropped by a full inbox {})",
            sharer.pace.discarded, sharer.too_big, overflow
        );
    }
    let _ = write!(
        line,
        ", repaired by parity {}, dropped by the reassembler {}, skipped waiting for an IDR {}, decoded {}, not decoded {}, presented {}, {} fps, {}; encode ms {}, decode ms {} (on the GPU), decode call ms {} (FFmpeg call only), capture to display ms {} (to present returning, or to the picture done on the GPU when later)",
        reassembly.repaired,
        reassembly.dropped(),
        reassembly.skipped + viewer.before_first_idr,
        viewer.decoded,
        viewer.decode_failed,
        viewer.presented,
        viewer
            .fps()
            .map_or_else(|| String::from("no"), |fps| format!("{fps:.1}")),
        path_word(viewer.path),
        spread_text(&mut sharer.encode_ms),
        spread_text(&mut viewer.decode_ms),
        spread_text(&mut viewer.decode_call_ms),
        spread_text(&mut viewer.end_to_end_ms),
    );
    let _ = write!(
        line,
        "; rate at the end {} kbit/s of {} allowed, backoffs {}, steps down {}, steps up {}",
        shared.rate_kbps, shared.allowed_kbps, shared.backoffs, shared.steps_down, shared.steps_up
    );
    if let Some(network) = network {
        let ms = |value: Duration| value.as_secs_f64() * 1000.0;
        let average = if network.pings > 0 {
            ms(network.rtt_sum) / network.pings as f64
        } else {
            0.0
        };
        let _ = write!(
            line,
            "; network: round trip {:.1}/{average:.1}/{:.1} ms (min/avg/max) over {} pings, dropped by the full queue {}",
            ms(network.rtt_min),
            ms(network.rtt_max),
            network.pings,
            network.queue_dropped
        );
    }
    line
}

#[cfg(test)]
mod tests {
    use share::SharerNumbers;

    use super::*;

    // tests/loopback.rs finds its numbers by these words.
    #[test]
    fn the_summary_names_each_number() {
        let mut shared = Shared {
            numbers: SharerNumbers {
                encode_ms: vec![2.5],
                ..SharerNumbers::default()
            },
            codec: Some(share::Codec::Hevc),
            rate_kbps: 12_000,
            allowed_kbps: 15_000,
            backoffs: 1,
            ..Shared::default()
        };
        let mut viewer = ViewerNumbers {
            presented: 120,
            decode_ms: vec![1.7, 1.8],
            decode_call_ms: vec![0.1],
            ..ViewerNumbers::default()
        };
        let line = summary(Totals {
            shared: &mut shared,
            viewer: &mut viewer,
            knob_dropped: 3,
            overflow: 0,
            network: Some(NetworkNumbers {
                queue_dropped: 4,
                pings: 2,
                rtt_min: Duration::from_millis(4),
                rtt_max: Duration::from_millis(28),
                rtt_sum: Duration::from_millis(32),
            }),
        });
        for words in [
            " in HEVC (IDRs 0, codec changes 0,",
            " presented 120,",
            "dropped by the knob 3,",
            "not decoded 0,",
            "decode ms median 1.70 p95 1.80 (on the GPU)",
            "decode call ms median 0.10 p95 0.10 (FFmpeg call only)",
            "rate at the end 12000 kbit/s of 15000 allowed, backoffs 1, steps down 0, steps up 0",
            "round trip 4.0/16.0/28.0 ms (min/avg/max) over 2 pings, dropped by the full queue 4",
        ] {
            assert!(line.contains(words), "no {words:?} in {line}");
        }
        assert!(!line.contains("full inbox"), "{line}");
    }
}
