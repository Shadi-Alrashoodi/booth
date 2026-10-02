// The strip while voice flows: pings ten times a second on a link that
// carries voice and once a second again 2 s after it stops, and jitter and
// loss taken from the voice of the person at the other end of the link while
// they talk.

mod common;

use std::thread;
use std::time::{Duration, Instant};

use common::voiced::{Voiced, alone, ms, settled, silence, talking, tone_440, tone_660, voiced};
use common::{Forwarder, Member, code_to_invite, host_invite, invite_to, loopback, timers};
use room::view::{Source, View};
use room::{TalkMode, Timers};

// A second between pings when idle, as outside the tests, so the two rates
// can be told apart; with the silence timers to match.
fn ping_timers() -> Timers {
    Timers {
        ping_idle: Duration::from_secs(1),
        ping_media: Duration::from_millis(100),
        reconnecting_after: Duration::from_secs(3),
        lost_after: Duration::from_secs(15),
        ..timers()
    }
}

// When each member's pings came back over `span`: every answered ping adds
// a point to its strip's trace, well under a millisecond after it left on
// loopback. The trace holds 120, more than any test here sends.
fn pings_during(members: &[&Member], span: Duration) -> Vec<Vec<Instant>> {
    let end = Instant::now() + span;
    let mut seen: Vec<usize> = members.iter().map(|m| m.view().strip.trace.len()).collect();
    let mut times = vec![Vec::new(); members.len()];
    while Instant::now() < end {
        for (i, member) in members.iter().enumerate() {
            let len = member.view().strip.trace.len();
            if len > seen[i] {
                assert!(len < 120, "the trace is full and counts no more");
                times[i].extend(std::iter::repeat_n(Instant::now(), len - seen[i]));
                seen[i] = len;
            }
        }
        thread::sleep(Duration::from_millis(1));
    }
    times
}

fn gaps(times: &[Instant]) -> Vec<f64> {
    times.windows(2).map(|pair| ms(pair[1] - pair[0])).collect()
}

fn average(values: &[f64]) -> f64 {
    values.iter().sum::<f64>() / values.len() as f64
}

#[test]
fn ping_rate_follows_voice() {
    let _alone = alone();
    let host = Voiced::host("Mara", ping_timers(), silence);
    let ana = Voiced::join(
        voiced("Ana", ping_timers(), tone_440, TalkMode::PushToTalk, true),
        host_invite(&host.member),
    );
    settled(&host, &[&ana]);
    let both = [&ana.member, &host.member];

    let idle = pings_during(&both, Duration::from_millis(2500));
    for (who, times) in ["Ana", "the host"].iter().zip(&idle) {
        let gaps = gaps(times);
        println!(
            "idle: {who} pinged {} times, {gaps:.0?} ms apart",
            times.len()
        );
        assert!(
            !gaps.is_empty() && gaps.iter().all(|gap| (900.0..1100.0).contains(gap)),
            "{who}: {gaps:?}"
        );
    }
    for member in both {
        assert_eq!(member.view().numbers.ping_interval, Duration::from_secs(1));
    }

    ana.room().talk(true);
    let started = Instant::now();
    let talk = pings_during(&both, Duration::from_millis(1500));
    for (who, times) in ["Ana", "the host"].iter().zip(&talk) {
        let first = times.first().expect("a ping while voice flows");
        let gaps = gaps(times);
        println!(
            "talking: {who}'s first ping {:.0} ms after the key went down, then {} more, {:.1} ms apart on average",
            ms(first.saturating_duration_since(started)),
            gaps.len(),
            average(&gaps)
        );
        // The frame fills, goes out, and the next ping comes at the media
        // rate from then: within one ping of the voice starting.
        assert!(
            first.saturating_duration_since(started) < Duration::from_millis(160),
            "{who}"
        );
        assert!((90.0..125.0).contains(&average(&gaps)), "{who}: {gaps:?}");
    }
    for member in both {
        assert_eq!(
            member.view().numbers.ping_interval,
            Duration::from_millis(100)
        );
    }

    ana.room().talk(false);
    let stopped = Instant::now();
    let after = pings_during(&both, Duration::from_millis(5000));
    for (who, times) in ["Ana", "the host"].iter().zip(&after) {
        let gaps = gaps(times);
        let slow = gaps
            .iter()
            .position(|gap| *gap > 500.0)
            .expect("the pings slow down");
        let fast_until = times[slow].saturating_duration_since(stopped);
        println!(
            "stopped: {who} pinged at the media rate for {:.0} ms more, then {:.0?} ms apart",
            ms(fast_until),
            &gaps[slow..]
        );
        assert!(
            fast_until >= Duration::from_millis(1900) && fast_until < Duration::from_millis(2300),
            "{who}: {fast_until:?}"
        );
        assert!(
            gaps[..slow].iter().all(|gap| *gap < 150.0),
            "{who}: {gaps:?}"
        );
        assert!(
            gaps[slow..].iter().all(|gap| (900.0..1100.0).contains(gap)),
            "{who}: {gaps:?}"
        );
    }
    for member in both {
        assert_eq!(member.view().numbers.ping_interval, Duration::from_secs(1));
    }
}

fn from_voice(view: &View) -> bool {
    let (strip, numbers) = (&view.strip, &view.numbers);
    strip.jitter_from == Source::Voice
        && strip.loss_from == Source::Voice
        && numbers.jitter_from == Source::Voice
        && numbers.loss_from == Source::Voice
}

fn from_pings(view: &View) -> bool {
    let (strip, numbers) = (&view.strip, &view.numbers);
    strip.jitter_from == Source::Pings
        && strip.loss_from == Source::Pings
        && numbers.jitter_from == Source::Pings
        && numbers.loss_from == Source::Pings
}

// Ana talks a steady tone through a forwarder. On the host, which hears her,
// the strip's jitter comes from her voice: near zero, the capture side's
// 2.7 ms periods being all that moves the frames about, until the forwarder
// holds each packet up a different time. Ana hears no voice, so hers stays
// with the pings.
#[test]
fn jitter_from_voice() {
    let _alone = alone();
    let host = Voiced::host("Mara", timers(), silence);
    let forwarder = Forwarder::new(loopback(host.member.port()));
    let ana = Voiced::join(
        voiced("Ana", timers(), tone_440, TalkMode::PushToTalk, true),
        invite_to(&host.member, forwarder.addr),
    );
    settled(&host, &[&ana]);
    assert!(from_pings(&host.view()));

    ana.room().talk(true);
    host.member
        .wait_for(Duration::from_secs(1), "the strip on voice", from_voice);
    thread::sleep(Duration::from_millis(1500));
    let steady = host.view();
    assert!(from_voice(&steady), "{:?}", steady.strip);
    assert!(from_pings(&ana.view()), "Ana hears no voice");
    let steady_ms = steady.strip.jitter_ms.expect("jitter from voice");
    assert_eq!(steady.numbers.jitter_ms, Some(steady_ms));
    assert_eq!(steady.strip.loss_pct, Some(0.0));

    forwarder.hold(Duration::ZERO, Duration::from_millis(30));
    thread::sleep(Duration::from_millis(1500));
    let uneven = host.view();
    forwarder.hold(Duration::ZERO, Duration::ZERO);
    let uneven_ms = uneven.strip.jitter_ms.expect("jitter from voice");
    println!(
        "jitter from voice on the host: {steady_ms:.2} ms from a steady talker, {uneven_ms:.2} ms with each packet held 0 to 30 ms; Ana, who hears none, shows {:?} ms from the host's pings, which the forwarder leaves alone",
        ana.view().strip.jitter_ms
    );
    assert!(from_voice(&uneven));
    assert!(steady_ms < 1.5, "{steady_ms}");
    assert!(
        uneven_ms > 3.0 && uneven_ms > 3.0 * steady_ms,
        "{uneven_ms}"
    );

    ana.room().talk(false);
    let stopped = Instant::now();
    host.member.wait_for(
        Duration::from_secs(3),
        "the strip back on pings",
        from_pings,
    );
    let back = stopped.elapsed();
    println!(
        "the host's strip back on pings {:.0} ms after the release",
        ms(back)
    );
    assert!(back >= Duration::from_millis(1900), "{back:?}");
}

// Ana talks through a forwarder that drops every tenth voice packet on its
// way to the host. The host, whose link to Ana that is, reads the loss on its
// strip from the gaps in her numbering. Bo hears her through the host, but
// his own link is clean: his strip stays with the pings, and only his voice
// loss for Ana shows the drops. Once the host talks, Bo's strip takes the
// host's voice, which comes over his link alone.
#[test]
fn loss_from_voice_on_that_link_only() {
    let _alone = alone();
    let host = Voiced::host("Mara", timers(), tone_660);
    host.room().new_invite(true);
    let multi = host
        .member
        .wait_for(Duration::from_secs(1), "multi-use invite", |v| {
            v.invite
                .as_ref()
                .is_some_and(|i| i.multi_use && !i.code.is_empty())
        });
    let code = multi.invite.expect("an invite").code;
    let forwarder = Forwarder::new(loopback(host.member.port()));
    // A steady 5 ms on Ana's way makes hers the host's slowest link, which
    // the host's strip follows.
    let five = Duration::from_millis(5);
    forwarder.hold(five, five);
    let ana = Voiced::join(
        voiced("Ana", timers(), tone_440, TalkMode::PushToTalk, true),
        code_to_invite(&code, forwarder.addr),
    );
    let bo = Voiced::join(
        voiced("Bo", timers(), silence, TalkMode::PushToTalk, true),
        code_to_invite(&code, loopback(host.member.port())),
    );
    settled(&host, &[&ana, &bo]);

    ana.room().talk(true);
    bo.member
        .wait_for(Duration::from_secs(2), "Bo hears Ana", |v| {
            talking(v, "Ana")
        });
    thread::sleep(Duration::from_millis(500));
    let view = host.view();
    assert!(from_voice(&view), "{:?}", view.strip);
    assert_eq!(view.strip.loss_pct, Some(0.0));
    let view = bo.view();
    assert!(from_pings(&view), "{:?}", view.strip);

    forwarder.drop_voice_every(10);
    thread::sleep(Duration::from_millis(2500));
    let (dropped, passed) = forwarder.voice_counts();
    let (on_host, on_bo) = (host.view(), bo.view());
    ana.room().talk(false);
    let dropped_pct = dropped as f32 * 100.0 / (dropped + passed) as f32;
    let heard = |view: &View| {
        view.numbers
            .voice_loss
            .iter()
            .find(|(name, _)| name == "Ana")
            .map(|(_, loss)| *loss)
            .expect("Ana's voice loss")
    };
    println!(
        "the forwarder dropped {dropped} of {} voice packets ({dropped_pct:.1} percent); from the gaps in Ana's numbering the host's strip reads {:?} percent; Bo's strip reads {:?} from his own pings; their buffers counted {:?} and {:?}",
        dropped + passed,
        on_host.strip.loss_pct,
        on_bo.strip.loss_pct,
        heard(&on_host),
        heard(&on_bo)
    );
    assert_eq!(on_host.numbers.link_name.as_deref(), Some("Ana"));
    assert!(from_voice(&on_host), "{:?}", on_host.strip);
    let loss = on_host.strip.loss_pct.expect("loss from voice");
    assert!(
        (loss - dropped_pct).abs() <= 1.0,
        "{loss} against {dropped_pct}"
    );
    assert_eq!(on_host.numbers.loss_pct, Some(loss));
    assert!(from_pings(&on_bo), "{:?}", on_bo.strip);
    assert_eq!(on_bo.strip.loss_pct, Some(0.0));
    let bo_heard = heard(&on_bo).all_pct;
    assert!(
        (bo_heard - dropped_pct).abs() <= 1.0,
        "{bo_heard} against {dropped_pct}"
    );

    host.room().talk(true);
    let on_bo = bo.member.wait_for(
        Duration::from_secs(2),
        "Bo's strip on the host's voice",
        from_voice,
    );
    host.room().talk(false);
    println!(
        "with the host talking Bo's strip reads {:?} percent and {:?} ms from the host's voice",
        on_bo.strip.loss_pct, on_bo.strip.jitter_ms
    );
    assert_eq!(on_bo.strip.loss_pct, Some(0.0));
}
