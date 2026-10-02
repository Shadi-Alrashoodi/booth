use stats::{LOSS_WINDOW, Level, Thresholds};

#[test]
fn round_trip_boundaries() {
    let t = Thresholds::default();
    assert_eq!(t.rtt_level(0.0), Level::Good);
    assert_eq!(t.rtt_level(40.0f32.next_down()), Level::Good);
    assert_eq!(t.rtt_level(40.0), Level::Warn);
    assert_eq!(t.rtt_level(100.0), Level::Warn);
    assert_eq!(t.rtt_level(100.0f32.next_up()), Level::Bad);
    assert_eq!(t.rtt_level(5000.0), Level::Bad);
}

#[test]
fn jitter_boundaries() {
    let t = Thresholds::default();
    assert_eq!(t.jitter_level(0.0), Level::Good);
    assert_eq!(t.jitter_level(5.0f32.next_down()), Level::Good);
    assert_eq!(t.jitter_level(5.0), Level::Warn);
    assert_eq!(t.jitter_level(15.0), Level::Warn);
    assert_eq!(t.jitter_level(15.0f32.next_up()), Level::Bad);
}

#[test]
fn loss_boundaries() {
    let t = Thresholds::default();
    assert_eq!(t.loss_level(0.0), Level::Good);
    assert_eq!(t.loss_level(0.0f32.next_up()), Level::Warn);
    assert_eq!(t.loss_level(2.0f32.next_down()), Level::Warn);
    assert_eq!(t.loss_level(2.0), Level::Bad);
    assert_eq!(t.loss_level(100.0), Level::Bad);
}

// A single dropped ping must not turn the strip red; the second one does.
#[test]
fn one_lost_ping_in_a_hundred_is_warn_and_two_are_bad() {
    let t = Thresholds::default();
    assert_eq!(t.loss_level(100.0 / LOSS_WINDOW as f32), Level::Warn);
    assert_eq!(t.loss_level(200.0 / LOSS_WINDOW as f32), Level::Bad);
}

#[test]
fn edited_thresholds_are_used() {
    let t = Thresholds {
        rtt_good_below_ms: 20.0,
        rtt_warn_up_to_ms: 50.0,
        jitter_good_below_ms: 2.0,
        jitter_warn_up_to_ms: 4.0,
        loss_warn_below_pct: 10.0,
    };
    assert_eq!(t.rtt_level(19.0), Level::Good);
    assert_eq!(t.rtt_level(20.0), Level::Warn);
    assert_eq!(t.rtt_level(51.0), Level::Bad);
    assert_eq!(t.jitter_level(4.0), Level::Warn);
    assert_eq!(t.jitter_level(4.5), Level::Bad);
    assert_eq!(t.loss_level(5.0), Level::Warn);
    assert_eq!(t.loss_level(10.0), Level::Bad);
}

#[test]
fn nan_is_bad_not_good() {
    let t = Thresholds::default();
    assert_eq!(t.rtt_level(f32::NAN), Level::Bad);
    assert_eq!(t.jitter_level(f32::NAN), Level::Bad);
    assert_eq!(t.loss_level(f32::NAN), Level::Bad);
}
