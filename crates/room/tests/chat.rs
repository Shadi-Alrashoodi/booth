mod common;

use std::collections::HashSet;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use common::{Forwarder, Member, code_to_invite, host_invite, invite_to, loopback, timers};
use room::view::{ChatLine, LinkState, Notice, View};
use room::{ChatRefused, Timers};

// Everyone live, with a round trip and so a clock offset on every link, so
// every line from here on gets a delivery time.
fn settled(host: &Member, clients: &[&Member]) {
    let people = clients.len() + 1;
    for client in clients {
        client.wait_for(Duration::from_secs(3), "client settled", |v| {
            v.strip.state == LinkState::Live
                && v.people.len() == people
                && v.numbers.clock_offset_ms.is_some()
        });
    }
    host.wait_for(Duration::from_secs(3), "host settled", |v| {
        v.people.len() == people && v.people.iter().all(|p| p.is_you || p.rtt_ms.is_some())
    });
}

fn texts(view: &View) -> Vec<(String, String, bool)> {
    view.chat
        .iter()
        .map(|line| (line.name.clone(), line.text.clone(), line.mine))
        .collect()
}

fn line(name: &str, text: &str, mine: bool) -> (String, String, bool) {
    (name.to_owned(), text.to_owned(), mine)
}

fn has_lines(count: usize) -> impl Fn(&View) -> bool {
    move |view| view.chat.len() >= count
}

// A host with a multi-use invite on show, and its code.
fn open_room() -> (Member, String) {
    let host = Member::host("Mara", timers());
    host_invite(&host);
    host.room().new_invite(true);
    let view = host.wait_for(Duration::from_secs(1), "multi-use invite", |v| {
        v.invite
            .as_ref()
            .is_some_and(|i| i.multi_use && !i.code.is_empty())
    });
    (host, view.invite.expect("an invite").code)
}

struct Said<'a> {
    member: &'a Member,
    key: [u8; 32],
    lines: Vec<String>,
}

impl<'a> Said<'a> {
    fn by(member: &'a Member) -> Said<'a> {
        Said {
            member,
            key: *member.identity.public(),
            lines: Vec::new(),
        }
    }

    fn say(&mut self, text: String) {
        self.member.room().say(&text).expect("said");
        self.lines.push(text);
    }
}

// Every line exactly once, each author's lines in the order they were said,
// and on a client everyone else's in the order the host took them.
fn check(
    who: &str,
    got: &[Arc<ChatLine>],
    host_order: &[Arc<ChatLine>],
    said: &[&Said],
    own: [u8; 32],
) {
    let total: usize = said.iter().map(|s| s.lines.len()).sum();
    assert_eq!(got.len(), total, "{who}: {} lines", got.len());
    let mut seen = HashSet::new();
    for line in got {
        assert!(seen.insert(&line.text), "{who}: {:?} twice", line.text);
        assert_eq!(line.mine, line.author == own, "{who}: {:?}", line.text);
    }
    for author in said {
        let theirs: Vec<&String> = got
            .iter()
            .filter(|line| line.author == author.key)
            .map(|line| &line.text)
            .collect();
        let want: Vec<&String> = author.lines.iter().collect();
        assert_eq!(theirs, want, "{who}: out of order");
    }
    let others = |lines: &[Arc<ChatLine>]| -> Vec<String> {
        lines
            .iter()
            .filter(|line| line.author != own)
            .map(|line| line.text.clone())
            .collect()
    };
    assert_eq!(
        others(got),
        others(host_order),
        "{who}: not the host's order"
    );
}

#[test]
fn lines_go_both_ways_and_are_timed() {
    let host = Member::host("Mara", timers());
    let ana = Member::join("Ana", timers(), host_invite(&host));
    settled(&host, &[&ana]);

    host.room()
        .say("  anyone got the key for the east door\n")
        .unwrap();
    // The one who said it sees it at once, cleaned.
    assert_eq!(
        texts(&host.view()),
        [line("Mara", "anyone got the key for the east door", true)]
    );
    ana.wait_for(Duration::from_secs(2), "the host's line", has_lines(1));
    ana.room().say("on my way").unwrap();
    assert_eq!(texts(&ana.view())[1], line("Ana", "on my way", true));
    host.wait_for(Duration::from_secs(2), "Ana's line", has_lines(2));

    let hosted = host.wait_for(Duration::from_secs(2), "a delivery time", |v| {
        v.numbers.chat_delivery_last_ms.is_some()
    });
    let seen = ana.wait_for(Duration::from_secs(2), "a delivery time", |v| {
        v.numbers.chat_delivery_last_ms.is_some()
    });
    assert_eq!(
        texts(&hosted),
        [
            line("Mara", "anyone got the key for the east door", true),
            line("Ana", "on my way", false),
        ]
    );
    assert_eq!(
        texts(&seen),
        [
            line("Mara", "anyone got the key for the east door", false),
            line("Ana", "on my way", true),
        ]
    );
    assert_eq!(hosted.chat[0].author, *host.identity.public());
    assert_eq!(seen.chat[0].author, *host.identity.public());
    assert_eq!(hosted.chat[1].author, *ana.identity.public());

    for (who, view) in [("host", &hosted), ("client", &seen)] {
        let n = &view.numbers;
        let last = n.chat_delivery_last_ms.unwrap();
        println!(
            "chat delivery on the {who}: last {last:.3} ms, average {:.3} ms, about {}, round trip {:.3} ms, clock offset {:+.3} ms",
            n.chat_delivery_avg_ms.unwrap_or(f32::NAN),
            n.chat_delivery_about,
            n.rtt_ms.unwrap_or(f32::NAN),
            n.clock_offset_ms.unwrap_or(f32::NAN),
        );
        assert!((0.0..1000.0).contains(&last), "{who}: {last} ms");
    }
}

#[test]
fn refused_lines_are_not_shown() {
    let mut host = Member::host("Mara", timers());
    let ana = Member::join("Ana", timers(), host_invite(&host));
    settled(&host, &[&ana]);

    let long = "a".repeat(901);
    let lines = ["line"; 21].join("\n");
    for (text, why) in [
        ("", ChatRefused::Empty),
        (" \n\t\r\n ", ChatRefused::Empty),
        ("\u{7}\u{202E}\u{200B}", ChatRefused::Empty),
        (long.as_str(), ChatRefused::TooLong),
        (lines.as_str(), ChatRefused::TooManyLines),
    ] {
        assert_eq!(ana.room().say(text), Err(why), "{text:?}");
        assert_eq!(host.room().say(text), Err(why), "{text:?}");
    }
    assert!(ana.view().chat.is_empty());
    host.holds_for(Duration::from_millis(300), "nothing arrives", |v| {
        v.chat.is_empty()
    });

    host.leave();
    ana.wait_for(Duration::from_secs(1), "room closed", |v| {
        v.notice == Some(Notice::RoomClosed)
    });
    assert_eq!(ana.room().say("anyone?"), Err(ChatRefused::NotLive));
    assert!(ana.view().chat.is_empty());
}

#[test]
fn line_waits_for_a_quiet_host() {
    let host = Member::host("Mara", timers());
    let router = Forwarder::new(loopback(host.port()));
    let ana = Member::join("Ana", timers(), invite_to(&host, router.addr));
    settled(&host, &[&ana]);

    router.block(true);
    ana.wait_for(Duration::from_secs(3), "reconnecting", |v| {
        v.strip.state == LinkState::Reconnecting
    });
    ana.room().say("still there?").unwrap();
    assert_eq!(texts(&ana.view()), [line("Ana", "still there?", true)]);
    host.holds_for(Duration::from_millis(200), "nothing gets through", |v| {
        v.chat.is_empty()
    });

    router.block(false);
    let back = host.wait_for(Duration::from_secs(3), "the line", has_lines(1));
    assert_eq!(texts(&back), [line("Ana", "still there?", false)]);
}

// Quiet for longer than lost_after: the host let the friend go, and the
// friend comes back on a new session with a new stream. The line said while
// the host was only quiet already shows as said, so it goes on that stream,
// once.
#[test]
fn line_goes_on_the_next_session() {
    let host = Member::host("Mara", timers());
    let router = Forwarder::new(loopback(host.port()));
    let ana = Member::join("Ana", timers(), invite_to(&host, router.addr));
    settled(&host, &[&ana]);

    router.block(true);
    ana.wait_for(Duration::from_secs(3), "reconnecting", |v| {
        v.strip.state == LinkState::Reconnecting
    });
    ana.room().say("you there?").unwrap();
    ana.wait_for(Duration::from_secs(5), "lost", |v| {
        v.strip.state == LinkState::Lost
    });
    host.wait_for(Duration::from_secs(2), "the host let Ana go", |v| {
        v.people.len() == 1
    });
    assert_eq!(ana.room().say("anyone?"), Err(ChatRefused::NotLive));
    router.block(false);

    ana.wait_for(Duration::from_secs(5), "back in the room", |v| {
        v.strip.state == LinkState::Live && v.people.len() == 2
    });
    let back = host.wait_for(Duration::from_secs(3), "the line", has_lines(1));
    assert_eq!(texts(&back), [line("Ana", "you there?", false)]);
    host.holds_for(Duration::from_millis(300), "the line only once", |v| {
        v.chat.len() == 1
    });
    assert_eq!(texts(&ana.view()), [line("Ana", "you there?", true)]);
}

#[test]
fn three_people_see_the_hosts_order() {
    let (host, code) = open_room();
    let invite = code_to_invite(&code, loopback(host.port()));
    let ana = Member::join("Ana", timers(), invite.clone());
    let bo = Member::join("Bo", timers(), invite);
    settled(&host, &[&ana, &bo]);

    // All at once, so the host takes them in an order of its own.
    let (mut mara_said, mut ana_said, mut bo_said) =
        (Said::by(&host), Said::by(&ana), Said::by(&bo));
    for n in 0..10 {
        mara_said.say(format!("mara {n}"));
        ana_said.say(format!("ana {n}"));
        bo_said.say(format!("bo {n}"));
    }
    let said = [&mara_said, &ana_said, &bo_said];
    let hosted = host.wait_for(Duration::from_secs(3), "every line", has_lines(30));
    for (who, member) in [("host", &host), ("Ana", &ana), ("Bo", &bo)] {
        let view = member.wait_for(Duration::from_secs(3), who, has_lines(30));
        check(
            who,
            &view.chat,
            &hosted.chat,
            &said,
            *member.identity.public(),
        );
        for chat_line in view.chat.iter() {
            let name = if chat_line.author == *host.identity.public() {
                "Mara"
            } else if chat_line.author == *ana.identity.public() {
                "Ana"
            } else {
                "Bo"
            };
            assert_eq!(chat_line.name, name, "{who}: {:?}", chat_line.text);
        }
    }
}

// Chat has to arrive once and in order under 20 percent loss. Both friends
// reach the host through a link that loses one packet in five each way.
#[test]
fn lines_through_20_percent_loss() {
    let (host, code) = open_room();
    let to_ana = Forwarder::new(loopback(host.port()));
    let to_bo = Forwarder::new(loopback(host.port()));
    let ana = Member::join("Ana", timers(), code_to_invite(&code, to_ana.addr));
    let bo = Member::join("Bo", timers(), code_to_invite(&code, to_bo.addr));
    settled(&host, &[&ana, &bo]);
    to_ana.lose(20);
    to_bo.lose(20);

    let started = Instant::now();
    let (mut mara_said, mut ana_said, mut bo_said) =
        (Said::by(&host), Said::by(&ana), Said::by(&bo));
    for n in 0..80 {
        ana_said.say(format!("ana {n}"));
        if n < 60 {
            mara_said.say(format!("mara {n}"));
            bo_said.say(format!("bo {n}"));
        }
    }
    let said = [&mara_said, &ana_said, &bo_said];
    let limit = Duration::from_secs(30);
    let hosted = host.wait_for(limit, "every line on the host", has_lines(200));
    let mut views = vec![("host", hosted.clone())];
    for (who, member) in [("Ana", &ana), ("Bo", &bo)] {
        views.push((who, member.wait_for(limit, who, has_lines(200))));
    }
    let took = started.elapsed();
    for (who, view) in &views {
        let own = match *who {
            "host" => *host.identity.public(),
            "Ana" => *ana.identity.public(),
            _ => *bo.identity.public(),
        };
        check(who, &view.chat, &hosted.chat, &said, own);
    }
    let lost = to_ana.lost() + to_bo.lost();
    assert!(lost > 0, "the forwarders lost nothing");
    println!(
        "200 lines through 20 percent loss: all in after {:.0} ms, {lost} packets lost on the way",
        took.as_secs_f64() * 1000.0
    );
    for (who, view) in &views {
        let n = &view.numbers;
        println!(
            "  {who}: {} retransmits, chat delivery last {:.3} ms, average {:.3} ms, about {}",
            n.retransmits,
            n.chat_delivery_last_ms.unwrap_or(f32::NAN),
            n.chat_delivery_avg_ms.unwrap_or(f32::NAN),
            n.chat_delivery_about,
        );
    }
}

#[test]
fn rekey_mid_burst_loses_nothing() {
    let short = Timers {
        rekey_after: Duration::from_millis(300),
        reject_after: Duration::from_millis(600),
        ..timers()
    };
    let host = Member::host("Mara", short);
    let ana = Member::join("Ana", short, host_invite(&host));
    settled(&host, &[&ana]);

    let (mut mara_said, mut ana_said) = (Said::by(&host), Said::by(&ana));
    let rekeys_before = ana.view().numbers.rekeys;
    for burst in 0..25 {
        for k in 0..4 {
            ana_said.say(format!("ana {burst}.{k}"));
            mara_said.say(format!("mara {burst}.{k}"));
        }
        thread::sleep(Duration::from_millis(40));
    }
    let rekeys_during = ana.view().numbers.rekeys - rekeys_before;
    let said = [&mara_said, &ana_said];
    let hosted = host.wait_for(Duration::from_secs(3), "every line", has_lines(200));
    let seen = ana.wait_for(Duration::from_secs(3), "every line", has_lines(200));
    check(
        "host",
        &hosted.chat,
        &hosted.chat,
        &said,
        *host.identity.public(),
    );
    check(
        "Ana",
        &seen.chat,
        &hosted.chat,
        &said,
        *ana.identity.public(),
    );
    println!("rekeys while the lines went out: {rekeys_during}");
    assert!(rekeys_during >= 2, "{rekeys_during} rekeys");
}

#[test]
fn history_keeps_the_newest_2000_lines() {
    let host = Member::host("Mara", timers());
    for n in 0..2010 {
        host.room().say(&format!("line {n}")).unwrap();
    }
    let view = host.view();
    assert_eq!(view.chat.len(), 2000);
    assert_eq!(view.chat[0].text, "line 10");
    assert_eq!(view.chat[1999].text, "line 2009");
}
