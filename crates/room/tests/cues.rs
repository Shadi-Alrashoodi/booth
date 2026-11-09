mod common;

use std::thread;
use std::time::Duration;

use common::voiced::{Voiced, alone, cues, settled, silence, voiced};
use common::{Forwarder, host_invite, invite_to, loopback, poll, timers};
use room::view::{LinkState, OwnShare, RunningShare};
use room::{Config, SoundCues, TalkMode};
use voice::audio::fake::Fake;

const WAIT: Duration = Duration::from_secs(3);

fn member(name: &str) -> (Config, Fake, Fake) {
    let mut config = voiced(name, timers(), silence, TalkMode::PushToTalk, true);
    config.0.voice.cues = SoundCues::default();
    config
}

fn heard(member: &Voiced, count: usize) {
    poll(WAIT, "the cues finished", || {
        (cues(&member.speakers).len() >= count).then_some(())
    });
}

fn opened(member: &Voiced) {
    member.room().share(60, None);
    let view = member.member.wait_for(WAIT, "share granted", |v| {
        matches!(v.share.own, OwnShare::Sharing { .. })
    });
    let OwnShare::Sharing { number, .. } = view.share.own else {
        unreachable!()
    };
    member.room().sharing().opened(
        number,
        RunningShare {
            software: false,
            paused: None,
            held: false,
        },
    );
}

#[test]
fn joining_plays_nothing_for_the_joiner() {
    let _alone = alone();
    let host = Voiced::host_with(member("Mara"));
    host.room().new_invite(true);
    let ana = Voiced::join(member("Ana"), host_invite(&host.member));
    settled(&host, &[&ana]);
    heard(&host, 1);
    let bo = Voiced::join(member("Bo"), host_invite(&host.member));
    settled(&host, &[&ana, &bo]);
    heard(&host, 2);
    heard(&ana, 1);
    thread::sleep(Duration::from_millis(500));
    assert!(cues(&bo.speakers).is_empty());
    for (member, count) in [(&host, 2), (&ana, 1)] {
        let heard = cues(&member.speakers);
        assert_eq!(heard.len(), count, "{heard:?}");
        assert!(
            heard
                .iter()
                .all(|cue| (6320..=6720).contains(&cue.len) && cue.rising),
            "{heard:?}"
        );
    }
}

#[test]
fn own_voice_and_live_switches() {
    let _alone = alone();
    let host = Voiced::host_with(member("Mara"));
    let steps = [
        (false, true, 4800, false),
        (false, false, 4800, true),
        (true, true, 5760, false),
        (true, false, 5760, true),
    ];
    for (at, (deafen, on, len, rising)) in steps.into_iter().enumerate() {
        if deafen {
            host.room().deafen(on);
        } else {
            host.room().mute(on);
        }
        heard(&host, at + 1);
        let played = cues(&host.speakers);
        let cue = &played[at];
        assert!((len - 400..=len).contains(&cue.len), "{cue:?}");
        assert_eq!(cue.rising, rising, "{cue:?}");
    }
    host.room().set_cues(SoundCues {
        own_mute: false,
        ..SoundCues::default()
    });
    host.room().mute(true);
    host.member
        .wait_for(WAIT, "muted without a cue", |v| v.voice.muted);
    thread::sleep(Duration::from_millis(200));
    host.room().mute(false);
    thread::sleep(Duration::from_millis(200));
    assert_eq!(cues(&host.speakers).len(), 4);
    host.room().set_cues(SoundCues {
        volume: 60,
        ..SoundCues::default()
    });
    host.room().mute(true);
    heard(&host, 5);
    let cue = &cues(&host.speakers)[4];
    assert!((0.055..=0.061).contains(&cue.peak), "{cue:?}");
}

#[test]
fn the_saved_volume_reaches_the_first_cue() {
    let _alone = alone();
    let mut config = member("Mara");
    config.0.voice.cues.volume = 60;
    let host = Voiced::host_with(config);
    host.room().mute(true);
    heard(&host, 1);
    let cue = &cues(&host.speakers)[0];
    assert!((0.055..=0.061).contains(&cue.peak), "{cue:?}");
}

#[test]
fn switch_off_keeps_the_own_share_cue() {
    let _alone = alone();
    let mut config = member("Mara");
    config.0.voice.cues.on = false;
    let host = Voiced::host_with(config);
    let ana = Voiced::join(member("Ana"), host_invite(&host.member));
    settled(&host, &[&ana]);
    host.room().mute(true);
    host.member.wait_for(WAIT, "muted", |v| v.voice.muted);
    host.room().mute(false);
    host.member.wait_for(WAIT, "unmuted", |v| !v.voice.muted);
    drop(ana);
    host.member.wait_for(WAIT, "alone", |v| v.people.len() == 1);
    thread::sleep(Duration::from_millis(250));
    assert!(cues(&host.speakers).is_empty());
    opened(&host);
    heard(&host, 1);
    host.room().stop_sharing();
    heard(&host, 2);
    host.room().sharing().closed();
    let heard = cues(&host.speakers);
    assert_eq!(heard.len(), 2, "{heard:?}");
    assert!(
        heard
            .iter()
            .all(|cue| (7280..=7680).contains(&cue.len) && cue.peak > 0.19)
    );
    assert!(heard[0].rising && !heard[1].rising);
}

#[test]
fn shares_play_for_the_other_members() {
    let _alone = alone();
    let mut host_config = member("Mara");
    host_config.0.voice.cues.joins = false;
    let host = Voiced::host_with(host_config);
    let ana = Voiced::join(member("Ana"), host_invite(&host.member));
    settled(&host, &[&ana]);
    opened(&host);
    heard(&ana, 1);
    heard(&host, 1);
    host.room().stop_sharing();
    heard(&ana, 2);
    heard(&host, 2);
    host.room().sharing().closed();
    let played = cues(&ana.speakers);
    assert_eq!(played.len(), 2, "{played:?}");
    assert!(played.iter().all(|cue| (8240..=8640).contains(&cue.len)));
    assert!(played[0].rising && !played[1].rising, "{played:?}");
    let own = cues(&host.speakers);
    assert_eq!(own.len(), 2, "{own:?}");
    assert!(own.iter().all(|cue| (7280..=7680).contains(&cue.len)));

    host.room().set_cues(SoundCues {
        shares: false,
        ..SoundCues::default()
    });
    opened(&ana);
    heard(&ana, 3);
    ana.room().stop_sharing();
    heard(&ana, 4);
    ana.room().sharing().closed();
    thread::sleep(Duration::from_millis(250));
    assert_eq!(cues(&host.speakers).len(), 2);
}

#[test]
fn connection_cues_follow_the_link() {
    let _alone = alone();
    let config = |name| {
        let mut config = member(name);
        config.0.timers.lost_after = Duration::from_secs(15);
        config.0.voice.cues.joins = false;
        config
    };
    let host = Voiced::host_with(config("Mara"));
    host.room().new_invite(true);
    let router = Forwarder::new(loopback(host.member.port()));
    let ana = Voiced::join(config("Ana"), invite_to(&host.member, router.addr));
    let bo = Voiced::join(config("Bo"), host_invite(&host.member));
    settled(&host, &[&ana, &bo]);
    router.block(true);
    ana.member.wait_for(WAIT, "Ana's link dropped", |v| {
        v.strip.state == LinkState::Reconnecting
    });
    for other in [&host, &bo] {
        other.member.wait_for(WAIT, "Ana is reconnecting", |v| {
            v.people.iter().any(|p| p.name == "Ana" && p.reconnecting)
        });
        heard(other, 1);
    }
    heard(&ana, 1);
    router.block(false);
    settled(&host, &[&ana, &bo]);
    for other in [&host, &bo] {
        other.member.wait_for(WAIT, "Ana's link came back", |v| {
            v.people.iter().any(|p| p.name == "Ana" && !p.reconnecting)
        });
        heard(other, 2);
    }
    heard(&ana, 2);
    thread::sleep(Duration::from_millis(250));
    for (member, len) in [(&host, 7200), (&bo, 7200), (&ana, 10560)] {
        let played = cues(&member.speakers);
        assert_eq!(played.len(), 2, "{played:?}");
        assert!(
            played
                .iter()
                .all(|cue| (len - 400..=len).contains(&cue.len))
        );
        assert!(!played[0].rising && played[1].rising, "{played:?}");
    }
}

#[test]
fn cues_never_go_out_in_the_voice() {
    let _alone = alone();
    let host = Voiced::host("Mara", timers(), silence);
    host.room().new_invite(true);
    let ana = Voiced::join(member("Ana"), host_invite(&host.member));
    settled(&host, &[&ana]);
    let mut config = member("Bo");
    config.0.voice.cues.on = false;
    let bo = Voiced::join(config, host_invite(&host.member));
    settled(&host, &[&ana, &bo]);
    heard(&ana, 1);
    ana.room().talk(true);
    let before = ana.view().numbers.voice_sent;
    ana.room().mute(true);
    heard(&ana, 2);
    ana.room().mute(false);
    heard(&ana, 3);
    let cy = Voiced::join(member("Cy"), host_invite(&host.member));
    settled(&host, &[&ana, &bo, &cy]);
    heard(&ana, 4);
    drop(cy);
    ana.member
        .wait_for(WAIT, "Cy left", |v| v.people.len() == 3);
    heard(&ana, 5);
    assert!(ana.view().numbers.voice_sent > before);
    assert!(
        bo.speakers
            .record()
            .written
            .iter()
            .all(|sample| sample.abs() < 1e-4)
    );
}
