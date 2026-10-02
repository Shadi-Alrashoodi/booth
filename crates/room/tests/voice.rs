// Voice through the room, on loopback, with the fake audio devices: no test
// here opens a real microphone or plays a sound. The fakes run on the real
// clock at the 128-frame (2.7 ms) period of a good driver, so what the numbers
// say is what a good PC would do, less the network.

mod common;

use std::f64::consts::TAU;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use common::voiced::{
    Cue, PERIOD, RATE, Voiced, alone, cues, frames, heard_cues, ms, settled, silence, sine,
    slow_to_close, talking, tone_440, tone_660, voiced, voiced_at, you_talk,
};
use common::{Forwarder, Hand, Member, host_invite, invite_to, loopback, poll, timers};
use room::view::{LinkState, OwnShare, RunningShare, View, VoiceLoss};
use room::{Devices, Show, TalkMode, Timers, VideoConfig, VideoSource, VoiceConfig};
use voice::audio::fake::{Fake, Setup};
use voice::audio::{AudioError, Choice, Direction};
use voice::codec::{Encoder, Mode};

// voice/tests/jitter.rs: no step between two output samples may be larger
// than this, joins included, for a 330 Hz tone at 0.4 of full scale.
const MAX_JOIN_STEP: f32 = 0.04;

// The amplitude of a sine at `hz` in `samples`, which should hold a whole
// number of its cycles.
fn amplitude(samples: &[f32], hz: f64) -> f64 {
    let (mut re, mut im) = (0.0, 0.0);
    for (n, &sample) in samples.iter().enumerate() {
        let phase = TAU * hz * n as f64 / RATE as f64;
        re += f64::from(sample) * phase.cos();
        im += f64::from(sample) * phase.sin();
    }
    2.0 * (re * re + im * im).sqrt() / samples.len() as f64
}

// The last 200 ms, which hold whole cycles of both test tones.
fn last_200_ms(samples: &[f32]) -> &[f32] {
    &samples[samples.len().saturating_sub(9600)..]
}

// The time of the frame played last before the view was stored. The view
// is stored again only when something it shows changed, and a voice packet
// changes nothing it shows, so a new time comes with the pings, about every
// 100 ms, not with every frame.
fn mouth_to_ear_now(member: &Member) -> Option<f32> {
    member.view().numbers.mouth_to_ear.map(|m2e| m2e.last_ms)
}

// The median of the next `count` new times.
fn next_mouth_to_ear(member: &Member, count: usize) -> f32 {
    let mut last = mouth_to_ear_now(member);
    let mut times = Vec::with_capacity(count);
    while times.len() < count {
        let time = poll(
            Duration::from_millis(300),
            "a new mouth-to-ear time",
            || mouth_to_ear_now(member).filter(|&time| Some(time) != last),
        );
        last = Some(time);
        times.push(time);
    }
    times.sort_unstable_by(f32::total_cmp);
    times[count / 2]
}

// For a tone just found on the speakers: the time in the view as it stands,
// stored at most a ping earlier, and in the first view stored after. The
// frame the tone started in played between the two, unless a view was
// stored in the few ms between the tone playing and it being found. If the
// buffer grew a frame between the two, only the time from the tone's side
// of the growth agrees with the speakers.
fn mouth_to_ear_around(member: &Member) -> [f32; 2] {
    let before = mouth_to_ear_now(member).expect("a mouth-to-ear time at the tone");
    [before, next_mouth_to_ear(member, 1)]
}

fn largest_step(samples: &[f32]) -> (f32, usize) {
    samples
        .windows(2)
        .enumerate()
        .map(|(at, pair)| ((pair[1] - pair[0]).abs(), at))
        .fold(
            (0.0, 0),
            |most, step| if step.0 > most.0 { step } else { most },
        )
}

// 1.5 s into the microphone's stream a 1 kHz tone starts, after the talk key
// has gone down, so its first sample on the host's speakers times the path.
const TONE_AT: u64 = 72_000;

fn tone_from_1500_ms(frame: u64, _: u16) -> f32 {
    if frame >= TONE_AT {
        sine(frame, 1000.0, 0.3)
    } else {
        0.0
    }
}

// Expected on these fake devices: 5 ms for the frame to fill, up to 2.7 ms
// more for the capture period it ends in, 2.5 ms of Opus lookahead, one or
// two 5 ms frames in the jitter buffer, two 2.7 ms periods queued in the
// speakers and the 1.5 ms the fake says Windows adds: about 15 to 25 ms,
// under the 30 ms LAN target. Two frames, because a frame leaves on the
// capture side's 2.7 ms grid and is pulled on the render side's, and that
// is 5.3 ms of spread, which one 5 ms frame cannot hold; the outage test
// shows one frame is enough when the periods line up with the frames.
#[test]
fn held_talk_reaches_the_host() {
    let _alone = alone();
    let host = Voiced::host("Mara", timers(), silence);
    let ana = Voiced::join(
        voiced(
            "Ana",
            timers(),
            tone_from_1500_ms,
            TalkMode::PushToTalk,
            true,
        ),
        host_invite(&host.member),
    );
    settled(&host, &[&ana]);
    let tone_at = ana.first_captured() + frames(TONE_AT);
    assert!(
        Instant::now() + Duration::from_millis(100) < tone_at,
        "settled too late"
    );
    ana.room().talk(true);
    host.member
        .wait_for(Duration::from_secs(2), "Ana talking on the host", |v| {
            talking(v, "Ana")
        });
    assert!(you_talk(&ana.view()));

    let arrived = poll(
        Duration::from_secs(4),
        "the tone on the host's speakers",
        || {
            let (samples, first) = host.heard();
            let at = samples.iter().position(|sample| sample.abs() > 0.05)?;
            Some(first + frames(at as u64))
        },
    );
    let delay = arrived.saturating_duration_since(tone_at);
    let at_tone = mouth_to_ear_around(&host.member);
    thread::sleep(Duration::from_millis(500));
    let before = host.view().numbers;

    ana.room().talk(false);
    let released = Instant::now();
    host.member
        .wait_for(Duration::from_secs(1), "Ana stops on the host", |v| {
            !talking(v, "Ana")
        });
    let stopped = released.elapsed();
    thread::sleep(Duration::from_millis(100));
    let after_release = host.heard().0.len();
    thread::sleep(Duration::from_millis(400));
    let (samples, _) = host.heard();
    let later = &samples[after_release..];
    assert!(later.len() > 9000, "the speakers kept running");
    assert!(
        later.iter().all(|sample| sample.abs() < 1e-4),
        "the tone goes on after the release"
    );

    let m2e = before.mouth_to_ear.expect("mouth to ear on the host");
    let buffer = before.buffer.expect("a buffer for Ana");
    println!(
        "tone first on the host's speakers {:.1} ms after it was captured, {:.1} and {:.1} ms by the stats either side of it; mouth to ear on the host last {:.1} ms, 10 s average {:.1} ms, p95 {:.1} ms{}; buffer {} ms ({} frames); talking ring off {:.1} ms after the release",
        ms(delay),
        at_tone[0],
        at_tone[1],
        m2e.last_ms,
        m2e.avg_ms,
        m2e.p95_ms,
        if m2e.about { ", about" } else { "" },
        buffer.ms,
        buffer.frames,
        ms(stopped)
    );
    println!(
        "audio periods on the host: in {:?} ms, out {:?} ms, render latency {:?} ms; Ana's as the host heard them: in {:?}, out {:?}, latency {:?}",
        before.audio_in_ms,
        before.audio_out_ms,
        before.render_latency_ms,
        before.far_audio_in_ms,
        before.far_audio_out_ms,
        before.far_render_latency_ms
    );
    assert_eq!((m2e.name.as_str(), buffer.name.as_str()), ("Ana", "Ana"));
    assert!(ms(delay) < 30.0, "{:.1} ms", ms(delay));
    assert!(m2e.avg_ms < 30.0 && m2e.p95_ms < 30.0, "{m2e:?}");
    // The room counts a render period and the 1.5 ms the fake reports as
    // Windows' own latency, which the fake's playback clock leaves out. The
    // stats' times for the frames either side of the tone are what the tone
    // is checked against, not the 10 s average: that also holds the frames
    // played before the buffer grew to its steady depth, a frame sooner, so
    // it reads lower the later in the spell the buffer grew.
    let modelled = f64::from(PERIOD) * 1000.0 / RATE as f64 + 1.5;
    let apart = at_tone.map(|m2e| f64::from(m2e) - ms(delay));
    assert!(
        apart.iter().any(|apart| (apart - modelled).abs() < 2.0),
        "the two clocks are {:.1} and {:.1} ms apart",
        apart[0],
        apart[1]
    );
    assert!(buffer.frames <= 2, "{buffer:?}");
    assert!(before.far_audio_in_ms.is_some() && before.far_render_latency_ms.is_some());
    assert!((before.audio_out_ms.unwrap() - 2.667).abs() < 0.01);
    // The fake microphone runs at 48 kHz on no Bluetooth at all, so the stats
    // panel has no "Microphone" line for it.
    let mic = before.microphone.expect("the host's microphone is open");
    assert_eq!(
        (mic.rate, mic.hands_free, mic.warns()),
        (48_000, false, false)
    );
}

#[test]
fn three_hear_each_other_not_themselves() {
    let _alone = alone();
    let host = Voiced::host("Mara", timers(), silence);
    let invite = host_invite(&host.member);
    host.room().new_invite(true);
    let multi = host
        .member
        .wait_for(Duration::from_secs(1), "multi-use invite", |v| {
            v.invite
                .as_ref()
                .is_some_and(|i| i.multi_use && !i.code.is_empty())
        });
    let code = multi.invite.expect("an invite").code;
    let to_host = loopback(host.member.port());
    let ana = Voiced::join(
        voiced("Ana", timers(), tone_440, TalkMode::PushToTalk, true),
        common::code_to_invite(&code, to_host),
    );
    let bo = Voiced::join(
        voiced("Bo", timers(), tone_660, TalkMode::PushToTalk, true),
        common::code_to_invite(&code, to_host),
    );
    drop(invite);
    settled(&host, &[&ana, &bo]);

    ana.room().talk(true);
    host.member
        .wait_for(Duration::from_secs(2), "the host hears Ana", |v| {
            talking(v, "Ana")
        });
    bo.member
        .wait_for(Duration::from_secs(2), "Bo hears Ana", |v| {
            talking(v, "Ana")
        });
    thread::sleep(Duration::from_millis(400));
    let on = |who: &Voiced, hz| amplitude(last_200_ms(&who.heard().0), hz);
    let (host_440, host_660) = (on(&host, 440.0), on(&host, 660.0));
    let (bo_440, ana_440) = (on(&bo, 440.0), on(&ana, 440.0));
    println!(
        "Ana alone: host 440 Hz {host_440:.3}, 660 Hz {host_660:.3}; Bo 440 Hz {bo_440:.3}; Ana 440 Hz {ana_440:.4}"
    );
    assert!(host_440 > 0.12 && host_660 < 0.01);
    assert!(bo_440 > 0.12, "Bo hears Ana through the host");
    assert!(ana_440 < 0.005, "Ana hears herself");

    bo.room().talk(true);
    host.member
        .wait_for(Duration::from_secs(2), "the host hears both", |v| {
            talking(v, "Ana") && talking(v, "Bo")
        });
    ana.member
        .wait_for(Duration::from_secs(2), "Ana hears Bo", |v| talking(v, "Bo"));
    thread::sleep(Duration::from_millis(400));
    let (host_440, host_660) = (on(&host, 440.0), on(&host, 660.0));
    let (ana_440, ana_660) = (on(&ana, 440.0), on(&ana, 660.0));
    let (bo_440, bo_660) = (on(&bo, 440.0), on(&bo, 660.0));
    println!(
        "both: host 440 Hz {host_440:.3}, 660 Hz {host_660:.3}; Ana 440 {ana_440:.4}, 660 {ana_660:.3}; Bo 440 {bo_440:.3}, 660 {bo_660:.4}"
    );
    assert!(
        host_440 > 0.12 && host_660 > 0.12,
        "the host's mixer has both"
    );
    assert!(ana_660 > 0.12 && ana_440 < 0.005);
    assert!(bo_440 > 0.12 && bo_660 < 0.005);
    let views = [host.view(), ana.view(), bo.view()];
    assert!(talking(&views[1], "Bo") && you_talk(&views[1]) && !talking(&views[1], "Mara"));
    assert!(talking(&views[2], "Ana") && you_talk(&views[2]));
    ana.room().talk(false);
    bo.room().talk(false);
}

fn loss_timers() -> Timers {
    Timers {
        redundancy_off_after: Duration::from_secs(2),
        repair_off_after: Duration::from_secs(2),
        ..timers()
    }
}

#[test]
fn loss_turns_on_redundancy_then_10_ms_frames() {
    let _alone = alone();
    let host = Voiced::host("Mara", loss_timers(), silence);
    let forwarder = Forwarder::new(loopback(host.member.port()));
    let log = common::fresh_log("voice", "loss", "client");
    let (mut config, microphone, speakers) =
        voiced("Ana", loss_timers(), tone_440, TalkMode::PushToTalk, true);
    config.log = Some(log.clone());
    let mut ana = Voiced::join(
        (config, microphone, speakers),
        invite_to(&host.member, forwarder.addr),
    );
    settled(&host, &[&ana]);
    ana.room().talk(true);
    ana.member
        .holds_for(Duration::from_millis(1500), "clean: 5 ms, no copy", |v| {
            v.numbers.send_frame_ms == 5 && !v.numbers.send_repair_copy
        });

    forwarder.lose(10);
    let lossy = Instant::now();
    ana.member
        .wait_for(Duration::from_secs(3), "redundancy on", |v| {
            v.numbers.send_repair_copy || v.numbers.send_frame_ms == 10
        });
    let copy_on = lossy.elapsed();
    let view = ana
        .member
        .wait_for(Duration::from_secs(6), "10 ms frames", |v| {
            v.numbers.send_frame_ms == 10
        });
    let tens = lossy.elapsed();
    let worst = view.numbers.own_voice_loss;
    let host_numbers = host.view().numbers;

    forwarder.lose(0);
    let clean = Instant::now();
    ana.member
        .wait_for(Duration::from_secs(10), "back to 5 ms with no copy", |v| {
            v.numbers.send_frame_ms == 5 && !v.numbers.send_repair_copy
        });
    let back = clean.elapsed();
    println!(
        "10 percent loss: redundancy on after {:.0} ms, 10 ms frames after {:.0} ms (worst reported {:?}; the host saw {:?}, buffer {:?}); loss off: back to 5 ms with no copy after {:.0} ms ({} packets lost on the way)",
        ms(copy_on),
        ms(tens),
        worst,
        host_numbers.voice_loss,
        host_numbers.buffer,
        ms(back),
        forwarder.lost()
    );
    assert!(copy_on <= Duration::from_secs(2), "{copy_on:?}");
    assert!(
        tens >= Duration::from_secs(2),
        "10 ms frames before 2 s of loss"
    );
    // Random loss of one packet in ten is nearly all one or two in a row.
    assert!(
        worst.is_some_and(|loss| loss.scattered_pct >= 5.0),
        "{worst:?}"
    );
    // Reports cover the last 2 s, so they read clean 2 s after the loss
    // stops, and the shortened quiet time is 2 s more.
    assert!(
        back >= Duration::from_secs(2) && back < Duration::from_secs(8),
        "{back:?}"
    );
    ana.room().talk(false);
    ana.member.leave();
    let text = common::read_log(&log);
    for line in [
        "voice: push to talk, constant rate on, input windows default, output windows default",
        "voice: microphone opened: \"Test microphone\", period 2.67 ms (128 frames)",
        "engine 48000 Hz, resampled by windows no, bluetooth hands-free no, small period yes",
        "voice: speakers opened: \"Test speakers\"",
        "voice: redundancy on, a listener lost ",
        " percent one or two frames at a time over the last 2 s",
        "voice: 10 ms frames with opus repair data, a listener lost ",
        "voice: back to 5 ms frames, 0.0 percent scattered loss, under 1 for 2 s",
        "voice: redundancy off, no scattered loss reported for 2 s",
        "voice: microphone closed, the room closed",
    ] {
        assert!(text.contains(line), "no {line:?} in:\n{text}");
    }
}

fn steady_330(frame: u64, _: u16) -> f32 {
    sine(frame, 330.0, 0.4)
}

struct Outage {
    longest_gap_ms: f64,
    // Both from the tone's first sample on the host's speakers.
    gap_at_ms: f64,
    step: f32,
    step_at_ms: f64,
    // The samples on either side of the largest step.
    around_step: Vec<f32>,
    depth_before: u32,
    depth_after: u32,
    // Each the median of three new times, about 300 ms of them.
    m2e_before: f32,
    m2e_after: f32,
    // What the host told Ana of her voice after the outage, and whether she
    // turned the repair copy or the 10 ms mode on at any look.
    reported: VoiceLoss,
    switched: bool,
}

// A talker's 5 ms frames leave at the end of the device period each one is
// completed in, so how long a frame waits for that follows a pattern that
// repeats every this many frames: 8 at 128-frame periods, 1 at 240.
fn frames_per_pattern(period: u32) -> u64 {
    let (mut a, mut b) = (period, 240);
    while b != 0 {
        (a, b) = (b, a % b);
    }
    u64::from(period / a)
}

// Ana talks a steady tone through a forwarder, which goes dark for 200 ms,
// and at 128-frame periods up to 7 frames (35 ms) longer, until the packet
// it ends on (below). The loss reports go once a second as they always do:
// an outage is loss in a long run, which neither the repair copy nor the
// 10 ms mode could have brought back, so neither comes on.
//
// The host's buffer times out in the outage and starts again from the first
// packet back, as it started the spell from the first packet of it. The
// outage ends on a packet at the same place in Ana's pattern as that one:
// at 128-frame periods the place moves a packet by up to 7/8 of a period
// (2.3 ms), which would move where playing starts again by as much, and is
// no part of what the outage leaves behind.
fn outage(period: u32) -> Outage {
    let timers = timers();
    let host = Voiced::host_at("Mara", timers, silence, period);
    let forwarder = Forwarder::new(loopback(host.member.port()));
    let ana = Voiced::join(
        voiced_at(
            "Ana",
            timers,
            steady_330,
            TalkMode::PushToTalk,
            true,
            period,
        ),
        invite_to(&host.member, forwarder.addr),
    );
    settled(&host, &[&ana]);
    forwarder.number_voice_from_now();
    ana.room().talk(true);
    host.member
        .wait_for(Duration::from_secs(2), "the host hears Ana", |v| {
            talking(v, "Ana")
        });
    thread::sleep(Duration::from_millis(500));
    let m2e_before = next_mouth_to_ear(&host.member, 3);
    let before = host.view().numbers;

    forwarder.block(true);
    thread::sleep(Duration::from_millis(200));
    forwarder.unblock_at_voice(frames_per_pattern(period));
    let unblocked = poll(
        Duration::from_millis(100),
        "the outage ends on a voice packet",
        || (!forwarder.blocked()).then(Instant::now),
    );
    let mut switched = false;
    let mut reported = None;
    while unblocked.elapsed() < Duration::from_millis(2500) {
        let view = ana.view();
        switched |= view.numbers.send_repair_copy || view.numbers.send_frame_ms == 10;
        if let Some(loss) = view
            .numbers
            .own_voice_loss
            .filter(|loss| loss.all_pct > 0.0)
        {
            reported.get_or_insert(loss);
        }
        if reported.is_some() && unblocked.elapsed() >= Duration::from_millis(1000) {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let m2e_after = next_mouth_to_ear(&host.member, 3);
    let after = host.view().numbers;
    ana.room().talk(false);
    thread::sleep(Duration::from_millis(200));

    let record = host.speakers.record();
    let samples: Vec<f32> = record.written.iter().step_by(2).copied().collect();
    let start = samples
        .iter()
        .position(|s| s.abs() > 0.01)
        .expect("the tone arrived");
    // Up to where Ana let go, less the fade after it.
    let heard = &samples[start..samples.len().saturating_sub(24_000)];
    let mut longest_gap = 0;
    let mut gap_at = 0;
    let mut gap = 0;
    for (at, sample) in heard.iter().enumerate() {
        if sample.abs() < 1e-4 {
            gap += 1;
            if gap > longest_gap {
                longest_gap = gap;
                gap_at = at + 1 - gap;
            }
        } else {
            gap = 0;
        }
    }
    let (step, step_at) = largest_step(&samples);
    let ms = |samples: f64| samples * 1000.0 / RATE as f64;
    Outage {
        longest_gap_ms: ms(longest_gap as f64),
        gap_at_ms: ms(gap_at as f64),
        step,
        step_at_ms: ms(step_at as f64 - start as f64),
        around_step: samples[step_at.saturating_sub(4)..(step_at + 5).min(samples.len())].to_vec(),
        depth_before: before.buffer.expect("a buffer before").frames,
        depth_after: after.buffer.expect("a buffer after").frames,
        m2e_before,
        m2e_after,
        reported: reported.expect("the outage reported to Ana within 2.5 s"),
        switched,
    }
}

// End to end: a 200 ms outage gives a gap, then audio again at one frame of
// buffer, with no click and no lasting delay. The buffer adds and takes away
// delay in whole 5 ms frames, so a delay the outage left behind is a frame
// more of mouth to ear. After the outage the buffer starts playing at the
// first render period after the first packet back, as it did for the first
// packet of the spell. With device periods that line up with the frames that
// is the same place: one frame of buffer before and after. At 2.7 ms periods
// it is two frames before (held_talk_reaches_the_host says why), and a
// first packet can come close enough to the end of a render period that a
// little lateness decides which period it makes. When only one of the two
// first packets misses the period it would have made, playing starts a
// period (2.7 ms) later or earlier than it did for the spell. That is less
// than a frame, but the depth counts whole frames behind the fastest packet
// and can then read one more or less, so it may differ by one, in the
// direction the mouth-to-ear time moved, when that moved by more than half
// a period.
//
// So the mouth-to-ear time may move by a render period where the periods
// do not line up with the frames, and by under half a millisecond at any
// period from the clock offset estimate and the render thread's wake-ups.
// A frame left in the buffer moves it by 5 ms, which this catches, unless
// playing also started a period earlier: 2.3 ms against a late start's
// 2.7, which nothing the test reads can tell apart.
#[test]
fn outage_of_200_ms() {
    let _alone = alone();
    for period in [240, PERIOD] {
        let got = outage(period);
        let moved = got.m2e_after - got.m2e_before;
        println!(
            "200 ms outage at {period}-frame periods: longest gap {:.0} ms from {:.0} ms into the tone, largest step {:.3} at {:.1} ms (the tone's own 0.017), buffer {} then {} frames, mouth to ear {:.1} ms before and {:.1} ms after ({moved:+.2} ms); Ana was told {:.1} percent lost, {:.1} scattered",
            got.longest_gap_ms,
            got.gap_at_ms,
            got.step,
            got.step_at_ms,
            got.depth_before,
            got.depth_after,
            got.m2e_before,
            got.m2e_after,
            got.reported.all_pct,
            got.reported.scattered_pct
        );
        assert_eq!(got.reported.scattered_pct, 0.0, "{:?}", got.reported);
        assert!(!got.switched, "the outage turned the repair copy on");
        assert!(got.longest_gap_ms > 120.0 && got.longest_gap_ms < 260.0);
        assert!(
            got.step <= MAX_JOIN_STEP,
            "a click of {} at {:.1} ms: {:?}",
            got.step,
            got.step_at_ms,
            got.around_step
        );
        let period_ms = period as f32 * 1000.0 / RATE as f32;
        let start_may_move = if frames_per_pattern(period) > 1 {
            period_ms
        } else {
            0.0
        };
        assert!(
            moved.abs() < start_may_move + 1.0,
            "a lasting delay: mouth to ear moved {moved:.1} ms"
        );
        let half_period = period_ms / 2.0;
        let may_move = if moved > half_period {
            0..=1
        } else if moved < -half_period {
            -1..=0
        } else {
            0..=0
        };
        let depth_moved = i64::from(got.depth_after) - i64::from(got.depth_before);
        assert!(
            may_move.contains(&depth_moved),
            "a lasting delay: buffer {} then {} frames, mouth to ear moved {moved:.1} ms",
            got.depth_before,
            got.depth_after
        );
        if period == 240 {
            assert_eq!((got.depth_before, got.depth_after), (1, 1));
        }
    }
}

#[test]
fn mute_deafen_and_release() {
    let _alone = alone();
    // Long enough that only the last-packet flag can end talking quickly.
    let timers = Timers {
        talking_for: Duration::from_secs(3),
        ..timers()
    };
    let host_log = common::fresh_log("voice", "mute", "host");
    let (mut config, microphone, speakers) =
        voiced("Mara", timers, silence, TalkMode::PushToTalk, true);
    config.log = Some(host_log.clone());
    let mut host = Voiced {
        member: Member::host_with(config),
        microphone,
        speakers,
    };
    let ana_log = common::fresh_log("voice", "mute", "client");
    let (mut config, microphone, speakers) =
        voiced("Ana", timers, tone_440, TalkMode::PushToTalk, true);
    config.log = Some(ana_log.clone());
    let mut ana = Voiced::join((config, microphone, speakers), host_invite(&host.member));
    settled(&host, &[&ana]);

    ana.room().mute(true);
    poll(Duration::from_secs(2), "Ana's microphone closes", || {
        let record = ana.microphone.record();
        (record.at_once == 0 && record.stops == 1).then_some(())
    });
    let view = ana
        .member
        .wait_for(Duration::from_secs(1), "muted in the view", |v| {
            v.voice.muted
        });
    let sent = view.numbers.voice_sent;
    ana.room().talk(true);
    host.member
        .holds_for(Duration::from_millis(500), "muted, nothing is heard", |v| {
            !talking(v, "Ana")
        });
    assert_eq!(ana.view().numbers.voice_sent, sent);
    ana.room().mute(false);
    host.member
        .wait_for(Duration::from_secs(2), "unmuted and still held", |v| {
            talking(v, "Ana")
        });
    assert_eq!(ana.microphone.record().opens.len(), 2);

    // Muted in the middle of a spell, the microphone closes between two
    // frames, and nothing goes on saying Ana sends.
    ana.room().mute(true);
    ana.member
        .wait_for(Duration::from_secs(1), "muted mid-spell", |v| {
            !v.voice.sending && !you_talk(v)
        });
    ana.room().mute(false);
    ana.member
        .wait_for(Duration::from_secs(2), "unmuted, still held", |v| {
            v.voice.sending && you_talk(v)
        });

    // Deafened, the host's speakers go quiet within one fade, and its own
    // microphone closes too. The row reads Unmute and Undeafen.
    host.room().deafen(true);
    host.member
        .wait_for(Duration::from_secs(1), "deafened", |v| {
            v.voice.deafened && v.voice.muted
        });
    thread::sleep(Duration::from_millis(50));
    let from = host.heard().0.len();
    thread::sleep(Duration::from_millis(400));
    let (samples, _) = host.heard();
    assert!(samples.len() - from > 9000);
    assert!(
        samples[from..].iter().all(|&s| s == 0.0),
        "deafened, and still playing"
    );
    poll(
        Duration::from_secs(2),
        "the host's microphone closes",
        || (host.microphone.record().at_once == 0).then_some(()),
    );
    host.room().deafen(false);
    host.member
        .wait_for(Duration::from_secs(1), "undeafened", |v| {
            !v.voice.deafened && !v.voice.muted
        });
    thread::sleep(Duration::from_millis(300));
    assert!(amplitude(last_200_ms(&host.heard().0), 440.0) > 0.12);

    ana.room().talk(false);
    let released = Instant::now();
    host.member.wait_for(
        Duration::from_secs(1),
        "the last packet ends talking",
        |v| !talking(v, "Ana"),
    );
    let ended = released.elapsed();
    println!(
        "talking ring off {:.1} ms after the release, with 3 s to wait without the flag",
        ms(ended)
    );
    assert!(ended < Duration::from_millis(300));
    ana.member.leave();
    host.member.leave();
    let (ana_text, host_text) = (common::read_log(&ana_log), common::read_log(&host_log));
    for line in [
        "voice: muted, the microphone closes",
        "voice: microphone closed\r\n",
        "voice: unmuted, the microphone opens",
    ] {
        assert!(ana_text.contains(line), "no {line:?} in:\n{ana_text}");
    }
    for line in [
        "voice: deafened, the microphone closes",
        "voice: undeafened, the microphone opens",
    ] {
        assert!(host_text.contains(line), "no {line:?} in:\n{host_text}");
    }
}

// 250 ms of tone and 750 ms of quiet, over and over. At a variable rate Opus
// fills the 20 bytes for the tone and needs less in the quiet; the cap is
// the constant size, so a variable rate only ever goes under it.
fn tone_and_silence(frame: u64, _: u16) -> f32 {
    if frame % 48_000 < 12_000 {
        sine(frame, 440.0, 0.3)
    } else {
        0.0
    }
}

// The talker's packets on the wire during a spell of about 1 s: how many
// voice frames went out, and the sizes of everything that went to the host.
fn one_spell(talker: &Voiced, forwarder: &Forwarder) -> (u64, Vec<usize>) {
    let sent = talker.view().numbers.voice_sent;
    forwarder.keep_sizes();
    talker.room().talk(true);
    thread::sleep(Duration::from_secs(1));
    talker.room().talk(false);
    thread::sleep(Duration::from_millis(100));
    let sizes = forwarder.take_sizes();
    let frames = talker.view().numbers.voice_sent - sent;
    (frames, sizes)
}

fn most_common(sizes: &[usize]) -> (usize, usize) {
    let mut counts = std::collections::BTreeMap::new();
    for &size in sizes {
        *counts.entry(size).or_insert(0usize) += 1;
    }
    counts
        .into_iter()
        .max_by_key(|&(_, count)| count)
        .unwrap_or_default()
}

#[test]
fn constant_rate_packets_are_one_size() {
    let _alone = alone();
    let host = Voiced::host("Mara", timers(), silence);
    host.room().new_invite(true);
    let multi = host
        .member
        .wait_for(Duration::from_secs(1), "multi-use invite", |v| {
            v.invite
                .as_ref()
                .is_some_and(|i| i.multi_use && !i.code.is_empty())
        });
    let code = multi.invite.expect("an invite").code;
    let to_ana = Forwarder::new(loopback(host.member.port()));
    let to_bo = Forwarder::new(loopback(host.member.port()));
    let ana = Voiced::join(
        voiced(
            "Ana",
            timers(),
            tone_and_silence,
            TalkMode::PushToTalk,
            true,
        ),
        common::code_to_invite(&code, to_ana.addr),
    );
    let bo = Voiced::join(
        voiced(
            "Bo",
            timers(),
            tone_and_silence,
            TalkMode::PushToTalk,
            false,
        ),
        common::code_to_invite(&code, to_bo.addr),
    );
    settled(&host, &[&ana, &bo]);

    let (frames, sizes) = one_spell(&ana, &to_ana);
    let (size, count) = most_common(&sizes);
    println!(
        "constant rate on: {frames} voice frames, {count} of {} packets on the wire {size} bytes",
        sizes.len()
    );
    // 32 bytes of framing and tag, the channel byte, 13 of voice header, and
    // a length byte with 20 bytes of Opus.
    assert_eq!(size, 32 + 1 + 13 + 1 + 20);
    assert!(frames > 150);
    assert!(
        count as u64 >= frames,
        "some voice packets were another size"
    );

    let (frames, sizes) = one_spell(&bo, &to_bo);
    let (size, count) = most_common(&sizes);
    let distinct = sizes
        .iter()
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    println!(
        "constant rate off: {frames} voice frames, {} packets on the wire in {distinct} sizes, the most common {size} bytes ({count})",
        sizes.len()
    );
    assert!(frames > 150);
    assert!(
        (count as u64) < frames * 8 / 10,
        "sizes do not follow the sound"
    );
    assert!(distinct >= 3);
}

// Open mic: -60 dBFS of room noise, and three bursts of a voice, 400 ms each
// from 1.2 s, 3.2 s and 5.2 s into the microphone's stream.
const BURSTS: [u64; 3] = [57_600, 153_600, 249_600];
const BURST: u64 = 19_200;

fn bursts(frame: u64, _: u16) -> f32 {
    let mut hashed = frame.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    hashed ^= hashed >> 29;
    let noise = ((hashed >> 40) as f32 / (1u64 << 24) as f32 - 0.5) * 0.002;
    let voice = BURSTS.iter().any(|&at| (at..at + BURST).contains(&frame));
    if voice {
        noise + sine(frame, 220.0, 0.2)
    } else {
        noise
    }
}

// Ana's own view says she stopped once her capture thread has sent her last
// packet and her timer thread has woken for it. That send readies the
// host's receive thread, which can run first and change the host's view
// before Ana's threads get back to hers, so hers can be a moment behind:
// 9.4 ms at worst seen, with every CPU busy. A wake the room lost would
// leave her view saying she talks until the next packet from the host
// rebuilt it. Here that is the host's ping or its answer to Ana's, each
// every 100 ms on a phase of its own. Both are more than 25 ms off, so a
// lost wake is caught, in a bit over half the bursts: 21 of 36 with the
// wake at the end of a spell taken out, and in each of 12 runs.
const OWN_VIEW_WITHIN: Duration = Duration::from_millis(25);

#[test]
fn open_mic_tail() {
    let _alone = alone();
    let host = Voiced::host("Mara", timers(), silence);
    let ana = Voiced::join(
        voiced("Ana", timers(), bursts, TalkMode::OpenMic, true),
        host_invite(&host.member),
    );
    settled(&host, &[&ana]);
    let first = ana.first_captured();
    assert!(Instant::now() + Duration::from_millis(100) < first + frames(BURSTS[0]));
    let mut tails = Vec::new();
    for at in BURSTS {
        let voice_at = first + frames(at);
        let voice_end = first + frames(at + BURST);
        host.member
            .wait_for(Duration::from_secs(3), "the voice is heard", |v| {
                talking(v, "Ana")
            });
        let started = Instant::now().saturating_duration_since(voice_at);
        host.member
            .wait_for(Duration::from_secs(3), "the tail ends", |v| {
                !talking(v, "Ana")
            });
        let stopped = Instant::now();
        let tail = stopped.saturating_duration_since(voice_end);
        ana.member
            .wait_for(OWN_VIEW_WITHIN, "Ana's own view says she stopped", |v| {
                !you_talk(v)
            });
        let behind = stopped.elapsed();
        println!(
            "open mic: heard {:.0} ms after the voice began, sent for {:.0} ms after it ended, Ana's own view {:.2} ms behind the host's",
            ms(started),
            ms(tail),
            ms(behind)
        );
        assert!(started < Duration::from_millis(60), "{started:?}");
        tails.push(tail);
    }
    for tail in &tails {
        // The tail, less a frame or two of path, and the delivery of the
        // last packet on top.
        assert!(
            *tail >= Duration::from_millis(190) && *tail <= Duration::from_millis(840),
            "{tails:?}"
        );
    }
}

// Opus frames Booth would send, from the voice crate's own encoder.
fn opus_frames(count: usize) -> Vec<Vec<u8>> {
    let mut encoder = Encoder::new(Mode::LowDelay, true).unwrap();
    (0..count)
        .map(|i| {
            let pcm: Vec<f32> = (0..240)
                .map(|n| sine((i * 240 + n) as u64, 440.0, 0.2))
                .collect();
            let mut out = [0u8; 20];
            let len = encoder.encode(&pcm, &mut out).unwrap();
            out[..len].to_vec()
        })
        .collect()
}

// The voice packet a talker sends the host, by hand: kind 1, seq, capture
// time, frame ms, flags, and the frame with its length.
fn spoken(seq: u16, frame: &[u8]) -> Vec<u8> {
    let mut packet = vec![1];
    packet.extend_from_slice(&seq.to_le_bytes());
    packet.extend_from_slice(&1_790_284_323_456_789u64.to_le_bytes());
    packet.extend_from_slice(&[5, 0, frame.len() as u8]);
    packet.extend_from_slice(frame);
    packet
}

#[test]
fn forged_and_bad_voice_goes_nowhere() {
    let _alone = alone();
    // Longer than the whole test, so a forged frame played under the host's
    // name would still show at the last look.
    let timers = Timers {
        talking_for: Duration::from_secs(5),
        ..timers()
    };
    let host = Voiced::host("Mara", timers, silence);
    host.room().new_invite(true);
    let multi = host
        .member
        .wait_for(Duration::from_secs(1), "multi-use invite", |v| {
            v.invite
                .as_ref()
                .is_some_and(|i| i.multi_use && !i.code.is_empty())
        });
    let code = multi.invite.expect("an invite").code;
    let to_host = loopback(host.member.port());
    let ana = Voiced::join(
        voiced("Ana", timers, silence, TalkMode::PushToTalk, true),
        common::code_to_invite(&code, to_host),
    );
    ana.member
        .wait_for(Duration::from_secs(3), "Ana live", |v| {
            v.strip.state == LinkState::Live && v.people.len() == 2
        });
    let mut eve = Hand::join(&common::code_to_invite(&code, to_host), to_host, "Eve");
    ana.member
        .wait_for(Duration::from_secs(2), "Eve in the roster", |v| {
            v.people.iter().any(|p| p.name == "Eve")
        });

    // Eve's voice goes to Ana under Eve's own slot, whatever Eve would like.
    let frames = opus_frames(200);
    let start = Instant::now();
    let mut seq = 0u16;
    let mut next = start;
    let mut say = |eve: &mut Hand, packet: &[u8]| {
        next += Duration::from_millis(5);
        thread::sleep(next.saturating_duration_since(Instant::now()));
        eve.send(channels::Channel::Voice, packet);
    };
    for frame in &frames[..100] {
        say(&mut eve, &spoken(seq, frame));
        seq = seq.wrapping_add(1);
    }
    let heard = ana.view();
    assert!(talking(&heard, "Eve"), "{:?}", heard.people);
    assert!(!talking(&heard, "Mara"));

    // What only a host sends, naming the host's slot and then Ana's. The
    // host takes Spoken alone, so neither goes anywhere.
    let dropped_before = host.view().numbers.voice_dropped;
    let mut claims = Vec::new();
    for slot in [0u8, 1] {
        let mut relayed = vec![2, slot];
        relayed.extend_from_slice(&spoken(seq, &frames[0])[1..]);
        claims.push(relayed);
    }
    // A frame length past the limit, a frame no Opus decoder should see, one
    // that says 10 ms and holds 5, and a packet cut short.
    let mut huge = vec![1, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8, 5, 0, 255];
    huge.extend(std::iter::repeat_n(0xE8, 1000));
    let mut garbage = vec![1, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8, 5, 0, 20];
    garbage.extend(std::iter::repeat_n(0xFF, 20));
    let mut mismatch = spoken(seq, &frames[1]);
    mismatch[11] = 10;
    let cut = spoken(seq, &frames[2])[..20].to_vec();
    let bad: Vec<Vec<u8>> = claims
        .into_iter()
        .chain([huge, garbage, mismatch, cut])
        .collect();
    for packet in &bad {
        say(&mut eve, packet);
    }
    for frame in &frames[100..] {
        say(&mut eve, &spoken(seq, frame));
        seq = seq.wrapping_add(1);
    }
    let still = ana.view();
    assert!(
        talking(&still, "Eve"),
        "Eve's good voice stopped with the bad"
    );
    let host_view = host
        .member
        .wait_for(Duration::from_secs(1), "the bad ones counted", |v| {
            v.numbers.voice_dropped >= dropped_before + bad.len() as u64
        });
    let ana_view = ana.view();
    println!(
        "{} bad voice packets from Eve: the host dropped {}, Ana dropped {}, and Eve's good voice kept going",
        bad.len(),
        host_view.numbers.voice_dropped - dropped_before,
        ana_view.numbers.voice_dropped
    );
    assert_eq!(
        host_view.numbers.voice_dropped - dropped_before,
        bad.len() as u64
    );
    assert_eq!(ana_view.numbers.voice_dropped, 0);
    assert!(
        !talking(&ana_view, "Mara"),
        "the host was named and Ana believed it"
    );
    drop(eve);
}

#[test]
fn callbacks_take_a_fraction_of_a_period() {
    let _alone = alone();
    let host = Voiced::host("Mara", timers(), silence);
    let ana = Voiced::join(
        voiced("Ana", timers(), tone_440, TalkMode::PushToTalk, true),
        host_invite(&host.member),
    );
    settled(&host, &[&ana]);
    ana.room().talk(true);
    thread::sleep(Duration::from_secs(3));
    let talker = ana.view().numbers;
    let listener = host.view().numbers;
    ana.room().talk(false);
    let capture = talker.capture_callback.expect("capture timed");
    let render = listener.render_callback.expect("render timed");
    let build = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    println!(
        "{build} build, 128-frame periods: the talker's capture callback median {:.3} ms, p99 {:.3} ms over {}; the listener's render callback median {:.3} ms, p99 {:.3} ms over {}",
        capture.median_ms,
        capture.p99_ms,
        capture.count,
        render.median_ms,
        render.p99_ms,
        render.count
    );
    // A period is 2.7 ms; a callback that took most of it would starve the
    // device.
    assert!(capture.p99_ms < 1.5 && render.p99_ms < 1.5);
}

// A microphone chosen in settings that is not plugged in: the view says why,
// nothing is sent, and the room still plays everyone else.
#[test]
fn unplugged_microphone_is_said() {
    let _alone = alone();
    let host = Voiced::host("Mara", timers(), tone_440);
    let (mut config, microphone, speakers) =
        voiced("Ana", timers(), silence, TalkMode::PushToTalk, true);
    config.voice.input = voice::audio::Choice::Device(String::from("unplugged"));
    let ana = Voiced::join((config, microphone, speakers), host_invite(&host.member));
    settled(&host, &[&ana]);
    let view = ana
        .member
        .wait_for(Duration::from_secs(2), "the microphone's error", |v| {
            v.voice.microphone.is_some()
        });
    let err = view.voice.microphone.expect("an error");
    println!("Ana's row says: {err}");
    assert!(
        matches!(err, voice::audio::AudioError::NotConnected { .. }),
        "{err:?}"
    );
    ana.room().talk(true);
    host.room().talk(true);
    ana.member
        .wait_for(Duration::from_secs(2), "Ana hears the host", |v| {
            talking(v, "Mara")
        });
    thread::sleep(Duration::from_millis(300));
    assert!(amplitude(last_200_ms(&ana.heard().0), 440.0) > 0.12);
    assert_eq!(ana.view().numbers.voice_sent, 0);
    assert!(!talking(&host.view(), "Ana"));
    host.room().talk(false);
    ana.room().talk(false);
}

// 2.5 s into the microphone's stream a 1 kHz tone starts, late enough for
// three people to settle first.
const LATER_TONE_AT: u64 = 120_000;

fn tone_from_2500_ms(frame: u64, _: u16) -> f32 {
    if frame >= LATER_TONE_AT {
        sine(frame, 1000.0, 0.3)
    } else {
        0.0
    }
}

// Bo hears Ana through the host, so Ana's capture time reaches Bo's clock
// through two offsets: Ana's to the host, and the host's to Bo. What Bo's
// stats say must agree with when the tone really left Bo's speakers.
#[test]
fn mouth_to_ear_through_the_host() {
    let _alone = alone();
    let host = Voiced::host("Mara", timers(), silence);
    host.room().new_invite(true);
    let multi = host
        .member
        .wait_for(Duration::from_secs(1), "multi-use invite", |v| {
            v.invite
                .as_ref()
                .is_some_and(|i| i.multi_use && !i.code.is_empty())
        });
    let code = multi.invite.expect("an invite").code;
    let to_host = loopback(host.member.port());
    let ana = Voiced::join(
        voiced(
            "Ana",
            timers(),
            tone_from_2500_ms,
            TalkMode::PushToTalk,
            true,
        ),
        common::code_to_invite(&code, to_host),
    );
    let bo = Voiced::join(
        voiced("Bo", timers(), silence, TalkMode::PushToTalk, true),
        common::code_to_invite(&code, to_host),
    );
    settled(&host, &[&ana, &bo]);
    let tone_at = ana.first_captured() + frames(LATER_TONE_AT);
    assert!(
        Instant::now() + Duration::from_millis(100) < tone_at,
        "settled too late"
    );
    ana.room().talk(true);
    bo.member
        .wait_for(Duration::from_secs(2), "Bo hears Ana", |v| {
            talking(v, "Ana")
        });
    let arrived = poll(Duration::from_secs(4), "the tone on Bo's speakers", || {
        let (samples, first) = bo.heard();
        let at = samples.iter().position(|sample| sample.abs() > 0.05)?;
        Some(first + frames(at as u64))
    });
    let delay = arrived.saturating_duration_since(tone_at);
    let at_tone = mouth_to_ear_around(&bo.member);
    thread::sleep(Duration::from_millis(500));
    let numbers = bo.view().numbers;
    ana.room().talk(false);

    let m2e = numbers.mouth_to_ear.expect("mouth to ear on Bo");
    println!(
        "Ana to Bo through the host: tone on Bo's speakers {:.1} ms after it was captured, {:.1} and {:.1} ms by Bo's stats either side of it; Bo's stats say last {:.1} ms, 10 s average {:.1} ms, p95 {:.1} ms{}; buffer {:?}",
        ms(delay),
        at_tone[0],
        at_tone[1],
        m2e.last_ms,
        m2e.avg_ms,
        m2e.p95_ms,
        if m2e.about { ", about" } else { "" },
        numbers.buffer
    );
    assert_eq!(m2e.name, "Ana");
    assert!(ms(delay) < 30.0, "{:.1} ms", ms(delay));
    // As on the host: the room counts a render period and the fake's 1.5 ms
    // that its playback clock leaves out, and the tone is checked against
    // the stats' times for the frames either side of it. Traced on
    // loopback, both clock offsets were within 0.2 ms of the true ones; what
    // moved the 10 s average was when in the spell Bo's buffer grew its
    // second frame, 0.2 ms lower for every 100 ms later.
    let modelled = f64::from(PERIOD) * 1000.0 / RATE as f64 + 1.5;
    let apart = at_tone.map(|m2e| f64::from(m2e) - ms(delay));
    assert!(
        apart.iter().any(|apart| (apart - modelled).abs() < 2.0),
        "the clocks are {:.1} and {:.1} ms apart through the host",
        apart[0],
        apart[1]
    );
}

// Loss on the link of someone who only listens: Bo reports it, and both
// people he hears, a friend and the host, turn their redundancy on.
#[test]
fn listeners_loss_reaches_both_talkers() {
    let _alone = alone();
    let host = Voiced::host("Mara", loss_timers(), tone_660);
    host.room().new_invite(true);
    let multi = host
        .member
        .wait_for(Duration::from_secs(1), "multi-use invite", |v| {
            v.invite
                .as_ref()
                .is_some_and(|i| i.multi_use && !i.code.is_empty())
        });
    let code = multi.invite.expect("an invite").code;
    let to_host = loopback(host.member.port());
    let to_bo = Forwarder::new(to_host);
    let ana = Voiced::join(
        voiced("Ana", loss_timers(), tone_440, TalkMode::PushToTalk, true),
        common::code_to_invite(&code, to_host),
    );
    let bo = Voiced::join(
        voiced("Bo", loss_timers(), silence, TalkMode::PushToTalk, true),
        common::code_to_invite(&code, to_bo.addr),
    );
    settled(&host, &[&ana, &bo]);
    ana.room().talk(true);
    host.room().talk(true);
    bo.member
        .wait_for(Duration::from_secs(2), "Bo hears both", |v| {
            talking(v, "Ana") && talking(v, "Mara")
        });
    let clean = |v: &View| v.numbers.send_frame_ms == 5 && !v.numbers.send_repair_copy;
    assert!(clean(&ana.view()) && clean(&host.view()));

    to_bo.lose(10);
    let lossy = Instant::now();
    let copy = |v: &View| v.numbers.send_repair_copy || v.numbers.send_frame_ms == 10;
    ana.member
        .wait_for(Duration::from_secs(3), "Ana's redundancy on", copy);
    let ana_on = lossy.elapsed();
    let host_view = host
        .member
        .wait_for(Duration::from_secs(3), "the host's redundancy on", copy);
    let host_on = lossy.elapsed();
    let bo_lost = bo.view().numbers.voice_loss;
    println!(
        "10 percent loss on Bo's link: Ana's redundancy on after {:.0} ms, the host's after {:.0} ms; the host says of its talkers {:?}, of its own voice {:?}; Bo lost {:?}",
        ms(ana_on),
        ms(host_on),
        host_view.numbers.voice_loss,
        host_view.numbers.own_voice_loss,
        bo_lost
    );
    assert!(ana_on <= Duration::from_secs(2), "{ana_on:?}");
    assert!(host_on <= Duration::from_secs(2), "{host_on:?}");
    // The host hears Ana clean itself; the number it shows is Bo's.
    let worst = host_view
        .numbers
        .voice_loss
        .iter()
        .find(|(name, _)| name == "Ana")
        .map(|(_, loss)| *loss)
        .expect("the host shows Ana's loss");
    assert!(worst.scattered_pct > 2.0, "{worst:?}");
    assert!(
        host_view
            .numbers
            .own_voice_loss
            .is_some_and(|loss| loss.scattered_pct > 2.0)
    );
    ana.room().talk(false);
    host.room().talk(false);
}

// A rekey every second while Ana talks: the capture thread moves to each new
// session between two frames, and nothing is lost or dropped on the way.
#[test]
fn talking_through_rekeys_loses_nothing() {
    let _alone = alone();
    let timers = Timers {
        rekey_after: Duration::from_secs(1),
        ..timers()
    };
    let host = Voiced::host("Mara", timers, silence);
    let ana = Voiced::join(
        voiced("Ana", timers, steady_330, TalkMode::PushToTalk, true),
        host_invite(&host.member),
    );
    settled(&host, &[&ana]);
    ana.room().talk(true);
    host.member
        .wait_for(Duration::from_secs(2), "the host hears Ana", |v| {
            talking(v, "Ana")
        });
    thread::sleep(Duration::from_millis(100));
    let rekeys = host.view().numbers.rekeys;
    let from = host.heard().0.len();
    thread::sleep(Duration::from_millis(3500));
    let host_view = host.view();
    let ana_view = ana.view();
    ana.room().talk(false);

    let (samples, _) = host.heard();
    let during = &samples[from..];
    let (step, _) = largest_step(during);
    let quiet = during.iter().filter(|s| s.abs() < 1e-4).count();
    let lost = host_view
        .numbers
        .voice_loss
        .iter()
        .find(|(name, _)| name == "Ana")
        .map(|(_, loss)| *loss);
    println!(
        "3.5 s of talk over {} rekeys: largest step {step:.3}, {quiet} near-zero samples of {}, loss {lost:?}; dropped on the host {} bad, {} replayed, voice {}; on Ana {} bad, voice {}",
        host_view.numbers.rekeys - rekeys,
        during.len(),
        host_view.numbers.dropped_bad,
        host_view.numbers.dropped_replay,
        host_view.numbers.voice_dropped,
        ana_view.numbers.dropped_bad,
        ana_view.numbers.voice_dropped
    );
    assert!(host_view.numbers.rekeys - rekeys >= 2);
    assert!(step <= MAX_JOIN_STEP, "a click of {step}");
    // A 330 Hz tone at 0.4 is that near zero for a sample at a crossing,
    // and no more.
    assert!(quiet < during.len() / 100, "{quiet} near-zero samples");
    assert_eq!(lost, Some(VoiceLoss::default()));
    for numbers in [&host_view.numbers, &ana_view.numbers] {
        assert_eq!(
            (
                numbers.dropped_bad,
                numbers.dropped_replay,
                numbers.voice_dropped
            ),
            (0, 0, 0)
        );
    }
}

// The host closed the room: nobody is left to hear, so Ana's microphone
// closes, and Windows stops showing it in use, before she presses Leave.
#[test]
fn microphone_closes_with_the_room() {
    let _alone = alone();
    let mut host = Voiced::host("Mara", timers(), silence);
    let log = common::fresh_log("voice", "closed", "client");
    let (mut config, microphone, speakers) =
        voiced("Ana", timers(), tone_440, TalkMode::PushToTalk, true);
    config.log = Some(log.clone());
    let mut ana = Voiced::join((config, microphone, speakers), host_invite(&host.member));
    settled(&host, &[&ana]);
    ana.room().talk(true);
    host.member
        .wait_for(Duration::from_secs(2), "the host hears Ana", |v| {
            talking(v, "Ana")
        });

    host.member.leave();
    ana.member
        .wait_for(Duration::from_secs(2), "the room closed", |v| {
            v.strip.state == LinkState::Closed
        });
    poll(Duration::from_secs(2), "Ana's microphone closes", || {
        (ana.microphone.record().at_once == 0).then_some(())
    });
    let view = ana
        .member
        .wait_for(Duration::from_secs(1), "Ana no longer sends", |v| {
            !v.voice.sending && !you_talk(v)
        });
    assert_eq!(view.voice.microphone, None, "no sentence under the row");
    ana.room().talk(false);
    ana.member.leave();
    let text = common::read_log(&log);
    let line = "voice: microphone closed, nobody is left to hear it";
    assert!(text.contains(line), "no {line:?} in:\n{text}");
}

// The host let Ana go after a long silence, and she came back with a new
// handshake. The host starts her over with nothing known of her audio, so
// she tells it her periods again.
#[test]
fn returning_friend_sends_her_periods_again() {
    let _alone = alone();
    let host = Voiced::host("Mara", timers(), silence);
    let router = Forwarder::new(loopback(host.member.port()));
    let ana = Voiced::join(
        voiced("Ana", timers(), silence, TalkMode::PushToTalk, true),
        invite_to(&host.member, router.addr),
    );
    settled(&host, &[&ana]);
    host.member
        .wait_for(Duration::from_secs(3), "Ana's audio on the host", |v| {
            v.numbers.far_audio_in_ms.is_some()
        });

    router.block(true);
    ana.member
        .wait_for(Duration::from_secs(5), "Ana lost", |v| {
            v.strip.state == LinkState::Lost
        });
    host.member
        .wait_for(Duration::from_secs(2), "the host let Ana go", |v| {
            v.people.len() == 1
        });
    router.block(false);
    ana.member
        .wait_for(Duration::from_secs(3), "Ana back", |v| {
            v.strip.state == LinkState::Live && v.people.len() == 2
        });
    let view = host
        .member
        .wait_for(Duration::from_secs(3), "Ana's audio again", |v| {
            v.people.len() == 2 && v.numbers.far_audio_in_ms.is_some()
        });
    println!(
        "Ana's periods on the host after she came back: in {:?} ms, out {:?} ms, render latency {:?} ms",
        view.numbers.far_audio_in_ms,
        view.numbers.far_audio_out_ms,
        view.numbers.far_render_latency_ms
    );
}

fn own_share(view: &View) -> Option<u32> {
    match view.share.own {
        OwnShare::Sharing { number, .. } => Some(number),
        _ => None,
    }
}

// The share's thread, played by hand: the capture and the encoder opened.
fn opened(member: &Member, share: u32) {
    member.room().sharing().opened(
        share,
        RunningShare {
            software: false,
            paused: None,
        },
    );
}

// The two notes, 80 ms each, less the quiet ends of the first and the last,
// no louder than a voice from a sensible microphone, which the mixer's knee
// is set above, and far below one at full scale.
fn check_cue(cue: &Cue, rising: bool, who: &str) {
    println!("{who}: {cue:?}");
    assert_eq!(cue.rising, rising, "{who}: {cue:?}");
    assert!((7400..=7680).contains(&cue.len), "{who}: {cue:?}");
    assert!(
        cue.peak > 0.1 && cue.peak <= voice::mix::KNEE,
        "{who}: {cue:?}"
    );
}

// The share cue rises in the sharer's own speakers when its share starts
// and falls when it stops, once each, and is never sent. The share's
// thread is played by hand here, saying when the capture and the encoder
// are open, which is when a share has started: a grant alone plays
// nothing. Whoever is sharing holds the talk key with a silent microphone,
// so their voice goes out while the cue plays, and the other's speakers
// show none of the cue reached them.
#[test]
fn share_cue_plays_for_the_sharer_only() {
    let _alone = alone();
    let host = Voiced::host("Mara", timers(), silence);
    let ana = Voiced::join(
        voiced("Ana", timers(), silence, TalkMode::PushToTalk, true),
        host_invite(&host.member),
    );
    settled(&host, &[&ana]);
    let wait = Duration::from_secs(3);

    host.room().talk(true);
    host.room().share(60, None);
    let view = host
        .member
        .wait_for(wait, "the host's share", |v| own_share(v).is_some());
    let share = own_share(&view).expect("granted");
    thread::sleep(Duration::from_millis(300));
    assert!(cues(&host.speakers).is_empty(), "a grant plays nothing");
    opened(&host.member, share);
    let heard = heard_cues(&host.speakers, 1);
    assert_eq!(heard.len(), 1, "{heard:?}");
    check_cue(&heard[0], true, "the host's start");
    host.room().stop_sharing();
    // The share's thread would notice the end at its next frame and say it
    // closed; here that comes after the falling cue has played.
    let heard = heard_cues(&host.speakers, 2);
    host.room().sharing().closed();
    assert_eq!(heard.len(), 2, "{heard:?}");
    check_cue(&heard[1], false, "the host's stop");
    let sent = host.view().numbers.voice_sent;
    host.room().talk(false);
    assert!(sent > 0, "the host's voice went out meanwhile");

    ana.room().talk(true);
    ana.room().share(60, None);
    let view = ana
        .member
        .wait_for(wait, "Ana's share", |v| own_share(v).is_some());
    let share = own_share(&view).expect("granted");
    opened(&ana.member, share);
    let heard = heard_cues(&ana.speakers, 1);
    assert_eq!(heard.len(), 1, "{heard:?}");
    check_cue(&heard[0], true, "Ana's start");
    ana.room().stop_sharing();
    let heard = heard_cues(&ana.speakers, 2);
    ana.room().sharing().closed();
    assert_eq!(heard.len(), 2, "{heard:?}");
    check_cue(&heard[1], false, "Ana's stop");
    let sent = ana.view().numbers.voice_sent;
    ana.room().talk(false);
    assert!(sent > 0, "Ana's voice went out meanwhile");

    // Long enough for a cue played twice, or one sent to the other, to be
    // heard whole.
    thread::sleep(Duration::from_millis(500));
    for (who, member) in [("the host", &host), ("Ana", &ana)] {
        let own = cues(&member.speakers);
        assert_eq!(own.len(), 2, "{who}: {own:?}");
        // Past the quiet first and last samples of each note of its own.
        let near = |at: usize| {
            own.iter()
                .any(|cue| (cue.at.saturating_sub(100)..cue.at + cue.len + 100).contains(&at))
        };
        let (heard, _) = member.heard();
        let rest = heard
            .iter()
            .enumerate()
            .filter(|&(at, _)| !near(at))
            .fold(0.0f32, |most, (_, s)| most.max(s.abs()));
        println!("{who}: the loudest sample outside their own cues {rest}");
        assert!(rest < 1e-4, "{who}: {rest}");
    }
}

// A share that cannot start plays nothing. The room's own share thread runs
// here, on the test pattern at a size the room refuses before anything
// opens: nothing is captured and the GPU is not touched.
#[test]
fn failed_share_plays_no_cue() {
    let _alone = alone();
    let (mut config, microphone, speakers) =
        voiced("Mara", timers(), silence, TalkMode::PushToTalk, true);
    config.video = VideoConfig {
        source: VideoSource::Pattern {
            width: 0,
            height: 0,
            busy: false,
        },
        show: Show::NoActivate,
        ..VideoConfig::default()
    };
    let host = Voiced {
        member: Member::host_with(config),
        microphone,
        speakers,
    };
    poll(Duration::from_secs(3), "the speakers open", || {
        host.speakers.record().opens.first().map(|_| ())
    });
    host.room().share(60, None);
    let view = host
        .member
        .wait_for(Duration::from_secs(3), "the share ended", |v| {
            v.share.problem.is_some() && v.share.own == OwnShare::Off
        });
    println!("{:?}", view.share.problem);
    thread::sleep(Duration::from_millis(500));
    let (heard, _) = host.heard();
    assert!(!heard.is_empty(), "the speakers played");
    assert!(
        cues(&host.speakers).is_empty(),
        "{:?}",
        cues(&host.speakers)
    );
    assert!(heard.iter().all(|s| s.abs() < 0.001));
}

// Leave with a microphone and speakers that close at once, as nearly all
// do: they are closed when leave returns, the log says so and says nothing
// of leaving them behind, and the next room on the same devices and port
// opens them at once.
#[test]
fn leave_closes_quick_devices() {
    let _alone = alone();
    let log = common::fresh_log("voice", "quick-close", "host");
    let (mut config, microphone, speakers) =
        voiced("Mara", timers(), tone_440, TalkMode::PushToTalk, true);
    config.log = Some(log.clone());
    let devices = config.voice.clone();
    let mut host = Voiced::host_with((config, microphone.clone(), speakers.clone()));
    let ana = Voiced::join(
        voiced("Ana", timers(), silence, TalkMode::PushToTalk, true),
        host_invite(&host.member),
    );
    settled(&host, &[&ana]);
    host.room().talk(true);
    ana.member
        .wait_for(Duration::from_secs(2), "Ana hears the host", |v| {
            talking(v, "Mara")
        });
    let port = host.member.port();
    let took = host.member.leave();
    let (mic, out) = (microphone.record(), speakers.record());
    println!(
        "leave took {:.1} ms; open after it: microphone {}, speakers {}",
        ms(took),
        mic.at_once,
        out.at_once
    );
    assert!(took < Duration::from_millis(200), "{took:?}");
    assert_eq!((mic.at_once, out.at_once), (0, 0));
    assert_eq!((mic.stops, out.stops), (1, 1));
    let text = common::read_log(&log);
    for line in [
        "voice: microphone closed, the room closed",
        "voice: speakers closed",
    ] {
        assert!(text.contains(line), "no {line:?} in:\n{text}");
    }
    assert!(!text.contains("leave waits no longer"), "{text}");

    let mut config = common::config("Mara", timers());
    config.voice = devices;
    config.port = port;
    let started = Instant::now();
    let next = Voiced::host_with((config, microphone.clone(), speakers.clone()));
    let opened = poll(Duration::from_secs(1), "the next room opens both", || {
        let (mic, out) = (microphone.record(), speakers.record());
        Some(mic.opens.get(1)?.0.max(out.opens.get(1)?.0))
    });
    let reopened = opened.saturating_duration_since(started);
    println!(
        "the next room on the same port had both open {:.1} ms after it started",
        ms(reopened)
    );
    assert!(reopened < Duration::from_millis(200), "{reopened:?}");
    drop(next);
}

// A Bluetooth headset that takes seconds to let go, as AirPods once took
// over 30: leave still returns at once, the port is free as it does, and a
// room hosted on that port at once talks while the headset is still
// closing. The new room has devices of its own; the old ones close by
// themselves afterwards, side by side.
#[test]
fn slow_headset_holds_up_nothing() {
    let _alone = alone();
    const CLOSE: Duration = Duration::from_secs(5);
    let (config, old_microphone, old_speakers) = slow_to_close("Mara", timers(), tone_440, CLOSE);
    let mut host = Voiced::host_with((config, old_microphone.clone(), old_speakers.clone()));
    let ana = Voiced::join(
        voiced("Ana", timers(), silence, TalkMode::PushToTalk, true),
        host_invite(&host.member),
    );
    settled(&host, &[&ana]);
    host.room().talk(true);
    ana.member
        .wait_for(Duration::from_secs(2), "Ana hears the host", |v| {
            talking(v, "Mara")
        });
    let port = host.member.port();
    let took = host.member.leave();
    let left = Instant::now();
    assert!(took < Duration::from_millis(200), "leave took {took:?}");

    let (mut config, microphone, speakers) =
        voiced("Mara", timers(), tone_660, TalkMode::PushToTalk, true);
    config.port = port;
    let next = Voiced::host_with((config, microphone, speakers));
    let bound = left.elapsed();
    let bea = Voiced::join(
        voiced("Bea", timers(), silence, TalkMode::PushToTalk, true),
        host_invite(&next.member),
    );
    settled(&next, &[&bea]);
    next.room().talk(true);
    bea.member.wait_for(
        Duration::from_secs(2),
        "Bea hears the next room's host",
        |v| talking(v, "Mara"),
    );
    let level = poll(Duration::from_secs(2), "the tone at full level", || {
        let level = amplitude(last_200_ms(&bea.heard().0), 660.0);
        (level > 0.12).then_some(level)
    });
    let talked = left.elapsed();
    let still = (
        old_microphone.record().at_once,
        old_speakers.record().at_once,
    );
    println!(
        "leave took {:.1} ms; port {port} bound again {:.1} ms after it, voice heard in the next room {:.0} ms after it at level {level:.2}; the old microphone and speakers still open: {still:?}",
        ms(took),
        ms(bound),
        ms(talked)
    );
    assert_eq!(
        still,
        (1, 1),
        "the old headset closed before the next room talked"
    );
    next.room().talk(false);

    let closed = poll(Duration::from_secs(10), "the old headset closes", || {
        let done = old_microphone.record().at_once == 0 && old_speakers.record().at_once == 0;
        done.then(|| left.elapsed())
    });
    println!("the old headset let go {:.0} ms after leave", ms(closed));
    assert!(closed < CLOSE + CLOSE / 2, "{closed:?}");
}

// The next room on the same headset while it is still closing. The room
// starts at once and so does its network; its microphone and speakers open
// once the old ones have let go, so the headset never has two of this PC's
// streams on it at once. A room left while it waits leaves at once and
// opens nothing.
#[test]
fn next_room_waits_for_a_closing_headset() {
    let _alone = alone();
    const CLOSE: Duration = Duration::from_millis(1500);
    let (config, microphone, speakers) = slow_to_close("Mara", timers(), tone_440, CLOSE);
    let devices = config.voice.clone();
    let mut first = Voiced::host_with((config, microphone.clone(), speakers.clone()));
    poll(Duration::from_secs(3), "the first room opens both", || {
        (microphone.record().starts == 1 && speakers.record().starts == 1).then_some(())
    });
    let port = first.member.port();
    let leaving = Instant::now();
    let took = first.member.leave();
    assert!(took < Duration::from_millis(200), "leave took {took:?}");

    let room = |log: Option<PathBuf>| {
        let mut config = common::config("Mara", timers());
        config.voice = devices.clone();
        config.port = port;
        config.log = log;
        config
    };
    let mut waiting = Member::host_with(room(None));
    thread::sleep(Duration::from_millis(100));
    let left_waiting = waiting.leave();
    assert!(
        left_waiting < Duration::from_millis(200),
        "{left_waiting:?}"
    );

    let log = common::fresh_log("voice", "same-headset", "host");
    let started = Instant::now();
    let second = Voiced::host_with((
        room(Some(log.clone())),
        microphone.clone(),
        speakers.clone(),
    ));
    let start_took = started.elapsed();
    let ana = Voiced::join(
        voiced("Ana", timers(), silence, TalkMode::PushToTalk, true),
        host_invite(&second.member),
    );
    settled(&second, &[&ana]);
    second.room().talk(true);
    ana.member
        .wait_for(Duration::from_secs(4), "Ana hears the host", |v| {
            talking(v, "Mara")
        });
    let level = poll(Duration::from_secs(2), "the tone at full level", || {
        let level = amplitude(last_200_ms(&ana.heard().0), 440.0);
        (level > 0.12).then_some(level)
    });
    second.room().talk(false);

    let (mic, out) = (microphone.record(), speakers.record());
    let reopened = |opens: &[(Instant, String)]| {
        opens
            .get(1)
            .map(|open| open.0.saturating_duration_since(leaving))
    };
    println!(
        "first leave {:.1} ms, a room left while it waited {:.1} ms, the next room started in {:.1} ms and opened the microphone {:?} and the speakers {:?} after the first leave, the old ones taking {CLOSE:?} to close; level {level:.2}",
        ms(took),
        ms(left_waiting),
        ms(start_took),
        reopened(&mic.opens),
        reopened(&out.opens)
    );
    assert!(start_took < Duration::from_millis(200), "{start_took:?}");
    assert_eq!(
        (mic.opens.len(), out.opens.len()),
        (2, 2),
        "{mic:?} {out:?}"
    );
    assert_eq!((mic.most_at_once, out.most_at_once), (1, 1));
    for (what, opens) in [("microphone", &mic.opens), ("speakers", &out.opens)] {
        let after = reopened(opens).expect("opened again");
        assert!(after >= CLOSE, "the {what} opened {after:?} after leave");
    }
    drop(ana);
    drop(second);
    let text = common::read_log(&log);
    for line in [
        "voice: the last room's speakers are still closing, they open here once they have let go",
        " ms for the last room's speakers",
        "voice: speakers opened",
    ] {
        assert!(text.contains(line), "no {line:?} in:\n{text}");
    }
}

// A headset slow to let go, as a Bluetooth one, that also has a second
// device beside it, as a PC's own microphone and speakers.
fn headset_and(signal: fn(u64, u16) -> f32, other: (&str, &str), close: Duration) -> Fake {
    let setup = Setup {
        period_frames: PERIOD,
        buffer_frames: 2 * PERIOD,
        signal,
        close_time: close,
        ..Setup::default()
    };
    Fake::new(setup, &[("pods", "Headset"), other], Some("pods"))
}

fn on_headset(close: Duration) -> (VoiceConfig, Fake, Fake) {
    let microphone = headset_and(tone_440, ("int", "Built in microphone"), close);
    let speakers = headset_and(silence, ("spk", "Built in speakers"), close);
    let mut voice = common::quiet_voice();
    voice.devices = Devices::Fake {
        microphone: microphone.clone(),
        speakers: speakers.clone(),
    };
    (voice, microphone, speakers)
}

fn host_on(voice: &VoiceConfig, port: u16) -> Member {
    let mut config = common::config("Mara", timers());
    config.voice = voice.clone();
    config.port = port;
    Member::host_with(config)
}

fn both_started(microphone: &Fake, speakers: &Fake, starts: usize) -> Option<()> {
    (microphone.record().starts == starts && speakers.record().starts == starts).then_some(())
}

// Windows' default moves off a headset still closing while the next room
// waits for it: the room sees the move within a quarter second and opens the
// new default beside the old close, which it never needed to wait for.
#[test]
fn new_default_opens_without_waiting() {
    let _alone = alone();
    const CLOSE: Duration = Duration::from_secs(2);
    let (voice, microphone, speakers) = on_headset(CLOSE);
    let mut first = host_on(&voice, 0);
    poll(Duration::from_secs(3), "the first room opens both", || {
        both_started(&microphone, &speakers, 1)
    });
    let port = first.port();
    let took = first.leave();
    assert!(took < Duration::from_millis(200), "leave took {took:?}");
    let left = Instant::now();
    let second = host_on(&voice, port);
    thread::sleep(Duration::from_millis(300));
    let (mic, out) = (microphone.record(), speakers.record());
    assert_eq!(
        (mic.opens.len(), out.opens.len()),
        (1, 1),
        "the next room opened the headset beside its close"
    );
    microphone.set_default(Some("int"));
    speakers.set_default(Some("spk"));
    let moved = Instant::now();
    let opened = poll(Duration::from_secs(2), "the new default opens", || {
        both_started(&microphone, &speakers, 2).map(|()| moved.elapsed())
    });
    let (mic, out) = (microphone.record(), speakers.record());
    println!(
        "Windows' default moved {:.0} ms after leave; the next room opened {:?} and {:?} {:.0} ms after the move, the old headset still closing: {}",
        ms(moved - left),
        mic.opens.last().map(|open| &open.1),
        out.opens.last().map(|open| &open.1),
        ms(opened),
        mic.at_once + out.at_once
    );
    assert_eq!(mic.opens[1].1, "int");
    assert_eq!(out.opens[1].1, "spk");
    assert_eq!(
        (mic.at_once, out.at_once),
        (2, 2),
        "the old headset closed before the new default opened"
    );
    assert!(opened < Duration::from_secs(1), "{opened:?}");
    drop(second);
}

// The headset a room opened as Windows' default, and the same headset
// chosen by name in settings for the next room, are one device: the next
// room waits for it to let go.
#[test]
fn headset_by_name_waits_for_itself_as_default() {
    let _alone = alone();
    const CLOSE: Duration = Duration::from_millis(1500);
    let (voice, microphone, speakers) = on_headset(CLOSE);
    let mut first = host_on(&voice, 0);
    poll(Duration::from_secs(3), "the first room opens both", || {
        both_started(&microphone, &speakers, 1)
    });
    let port = first.port();
    let leaving = Instant::now();
    let took = first.leave();
    assert!(took < Duration::from_millis(200), "leave took {took:?}");
    let mut named = voice.clone();
    named.input = Choice::Device(String::from("pods"));
    named.output = Choice::Device(String::from("pods"));
    let second = host_on(&named, port);
    second.wait_for(
        Duration::from_secs(1),
        "the speakers' line, with no way out through Windows",
        |v| v.voice.speakers == Some(closing(Direction::Output, false)),
    );
    let opened = poll(Duration::from_secs(4), "the next room opens both", || {
        both_started(&microphone, &speakers, 2).map(|()| leaving.elapsed())
    });
    let (mic, out) = (microphone.record(), speakers.record());
    println!(
        "the next room opened the headset by name {:.0} ms after leave began, the old one taking {CLOSE:?} to close",
        ms(opened)
    );
    assert_eq!(
        (mic.opens[1].1.as_str(), out.opens[1].1.as_str()),
        ("pods", "pods")
    );
    assert_eq!((mic.most_at_once, out.most_at_once), (1, 1));
    assert!(opened >= CLOSE, "{opened:?}");
    drop(second);
}

fn closing(direction: Direction, default: bool) -> AudioError {
    AudioError::StillClosing { direction, default }
}

fn says_closing(view: &View) -> bool {
    [&view.voice.microphone, &view.voice.speakers]
        .into_iter()
        .any(|err| matches!(err, Some(AudioError::StillClosing { .. })))
}

// What the panel says under your row while the next room waits for the
// last room's headset: the speakers' line first, since they open first,
// then the microphone's, which here takes longer to let go. Each goes the
// moment its device opens, and a room with nothing to wait for never says
// it.
#[test]
fn waiting_for_a_headset_says_so() {
    let _alone = alone();
    const SPEAKERS_CLOSE: Duration = Duration::from_millis(1000);
    const MICROPHONE_CLOSE: Duration = Duration::from_millis(2000);
    let microphone = headset_and(tone_440, ("int", "Built in microphone"), MICROPHONE_CLOSE);
    let speakers = headset_and(silence, ("spk", "Built in speakers"), SPEAKERS_CLOSE);
    let mut voice = common::quiet_voice();
    voice.devices = Devices::Fake {
        microphone: microphone.clone(),
        speakers: speakers.clone(),
    };
    let mut first = host_on(&voice, 0);
    poll(Duration::from_secs(3), "the first room opens both", || {
        assert!(!says_closing(&first.view()), "nothing was closing");
        both_started(&microphone, &speakers, 1)
    });
    first.holds_for(Duration::from_millis(200), "nothing was closing", |v| {
        !says_closing(v)
    });
    let port = first.port();
    let took = first.leave();
    assert!(took < Duration::from_millis(200), "leave took {took:?}");
    let left = Instant::now();
    let second = host_on(&voice, port);

    let mut found = Vec::new();
    for (what, device, direction) in [
        ("speakers", &speakers, Direction::Output),
        ("microphone", &microphone, Direction::Input),
    ] {
        let line = Some(closing(direction, true));
        let side = |v: &View| match direction {
            Direction::Input => v.voice.microphone.clone(),
            Direction::Output => v.voice.speakers.clone(),
        };
        second.wait_for(
            SPEAKERS_CLOSE,
            &format!("the line for the {what} while it waits"),
            |v| side(v) == line,
        );
        let shown = left.elapsed();
        assert_eq!(
            device.record().opens.len(),
            1,
            "the {what} opened before the line for it showed"
        );
        second.wait_for(
            MICROPHONE_CLOSE * 2,
            &format!("the line for the {what} goes"),
            |v| side(v) != line,
        );
        let gone = Instant::now();
        let view = second.view();
        let record = device.record();
        assert_eq!(side(&view), None, "the {what} did not open");
        assert_eq!(
            record.opens.len(),
            2,
            "the line for the {what} went before it opened"
        );
        let after_open = gone.saturating_duration_since(record.opens[1].0);
        found.push((what, shown, gone - left, after_open));
        assert!(
            after_open < Duration::from_millis(250),
            "the line for the {what} stayed {after_open:?} after it opened"
        );
    }
    second.holds_for(
        Duration::from_millis(200),
        "nothing is closing any more",
        |v| !says_closing(v),
    );
    for (what, shown, gone, after_open) in found {
        println!(
            "{what}: the line showed {:.0} ms after leave and went {:.0} ms after it, {:.1} ms after the open",
            ms(shown),
            ms(gone),
            ms(after_open)
        );
    }
    drop(second);
}

// Muted while the microphone waits for the last room's headset, the wait's
// line goes with it, so an unmute after the wait says nothing while a
// headset slow to open opens.
#[test]
fn mute_while_waiting_leaves_no_line() {
    let _alone = alone();
    const CLOSE: Duration = Duration::from_millis(1000);
    const OPEN: Duration = Duration::from_millis(400);
    let setup = Setup {
        period_frames: PERIOD,
        buffer_frames: 2 * PERIOD,
        signal: tone_440,
        open_time: OPEN,
        close_time: CLOSE,
        ..Setup::default()
    };
    let microphone = Fake::new(setup, &[("pods", "Headset")], Some("pods"));
    let speakers = headset_and(silence, ("spk", "Built in speakers"), Duration::ZERO);
    let mut voice = common::quiet_voice();
    voice.devices = Devices::Fake {
        microphone: microphone.clone(),
        speakers: speakers.clone(),
    };
    let mut first = host_on(&voice, 0);
    poll(Duration::from_secs(3), "the first room opens both", || {
        both_started(&microphone, &speakers, 1)
    });
    let port = first.port();
    first.leave();

    let log = common::fresh_log("voice", "mute-waiting", "host");
    let mut config = common::config("Mara", timers());
    config.voice = voice;
    config.port = port;
    config.log = Some(log.clone());
    let second = Member::host_with(config);
    second.wait_for(
        Duration::from_millis(500),
        "the microphone's line while it waits",
        |v| v.voice.microphone == Some(closing(Direction::Input, true)),
    );
    second.room().mute(true);
    // Only once the devices thread has read mute again after the wait: an
    // unmute before that would open the microphone as if never muted.
    poll(
        CLOSE * 2,
        "the wait ends with the microphone closed",
        || {
            let text = std::fs::read_to_string(&log).ok()?;
            text.contains("voice: muted meanwhile, the microphone stays closed")
                .then_some(())
        },
    );
    assert_eq!(
        microphone.record().opens.len(),
        1,
        "the microphone opened while muted"
    );
    second.room().mute(false);
    second.holds_for(OPEN * 3 / 4, "no line while the microphone opens", |v| {
        v.voice.microphone.is_none()
    });
    poll(OPEN * 2, "the microphone opens", || {
        (microphone.record().starts == 2).then_some(())
    });
    second.holds_for(Duration::from_millis(100), "no line once it is open", |v| {
        !says_closing(v)
    });
    drop(second);
}

// Muted on a headset slow to let go, unmuted, and left at once. The
// microphone closes on its own thread, so the speakers start to close at
// Leave beside it rather than after it, and the unmute opens nothing once
// the room is left. The next room on the headset has both after one close,
// where one after the other would take two.
#[test]
fn mute_on_a_slow_headset() {
    let _alone = alone();
    const CLOSE: Duration = Duration::from_millis(1500);
    let (config, microphone, speakers) = slow_to_close("Mara", timers(), tone_440, CLOSE);
    let voice = config.voice.clone();
    let log = common::fresh_log("voice", "mute-slow", "host");
    let mut config = config;
    config.log = Some(log.clone());
    let mut first = Member::host_with(config);
    poll(Duration::from_secs(3), "the first room opens both", || {
        both_started(&microphone, &speakers, 1)
    });
    first.room().mute(true);
    poll(
        Duration::from_secs(1),
        "the microphone starts to close",
        || (microphone.record().stops == 1).then_some(()),
    );
    first.room().mute(false);
    poll(
        Duration::from_secs(1),
        "the unmute waits for the close",
        || {
            let text = std::fs::read_to_string(&log).ok()?;
            text.contains(
                "voice: the microphone is still closing, it opens again once it has let go",
            )
            .then_some(())
        },
    );
    let port = first.port();
    let leaving = Instant::now();
    let took = first.leave();
    let second = host_on(&voice, port);
    let opened = poll(Duration::from_secs(5), "the next room opens both", || {
        both_started(&microphone, &speakers, 2).map(|()| leaving.elapsed())
    });
    let (mic, out) = (microphone.record(), speakers.record());
    println!(
        "leave {:.1} ms with an unmute waiting on the close; the next room had both {:.0} ms after leave began, each close taking {CLOSE:?}; microphone opens {}",
        ms(took),
        ms(opened),
        mic.opens.len()
    );
    assert!(took < Duration::from_millis(200), "{took:?}");
    assert_eq!(
        mic.opens.len(),
        2,
        "the unmute opened the microphone after leave"
    );
    assert_eq!((mic.most_at_once, out.most_at_once), (1, 1));
    assert!(opened < CLOSE + CLOSE / 2, "{opened:?}");
    drop(second);
    let text = common::read_log(&log);
    assert_eq!(
        text.matches("voice: microphone opened").count(),
        1,
        "{text}"
    );
    assert!(
        text.contains("voice: leave waits no longer than 100 ms"),
        "{text}"
    );
}
