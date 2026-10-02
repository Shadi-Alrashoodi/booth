// Remote control through the room: the ask, the answer, the input packets
// the host passes on, and every way control ends. Everything runs on
// loopback with the fake audio devices and no GPU. The injector is a fake
// that records what it is handed and injects nothing, and the controller's
// input is made-up events handed to the room's Controls: nothing here reads
// a real key or moves a real mouse.

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use channels::Channel;
use common::voiced::{Cue, Voiced, alone, cues, heard_cues, settled, silence, voiced};
use common::{Forwarder, Hand, Member, code_to_invite, config, loopback, poll, timers};
use invite::Invite;
use room::view::{LineKind, LinkState, View};
use room::{
    Area, Button, ControlEnd, Held, Injected, Injection, Injector, InputEvent, ScanCode, Started,
    TalkMode, Timers,
};

const WAIT: Duration = Duration::from_secs(3);

// What the fake injector was handed, and when.
#[derive(Clone, Debug, PartialEq)]
enum Heard {
    Started(Started),
    Input {
        events: Vec<InputEvent>,
        held: Held,
        area: Option<Area>,
    },
    CutOff,
    Ended(ControlEnd),
}

#[derive(Default)]
struct Recorder {
    heard: Mutex<Vec<(Instant, Heard)>>,
    // What the next Injected says about an administrator window.
    admin: AtomicBool,
}

impl Recorder {
    fn heard(&self) -> Vec<Heard> {
        self.heard_since(0)
    }

    // What came after the first `mark` calls, for a test with more than
    // one session.
    fn heard_since(&self, mark: usize) -> Vec<Heard> {
        lock(&self.heard)
            .iter()
            .skip(mark)
            .map(|(_, heard)| heard.clone())
            .collect()
    }

    fn mark(&self) -> usize {
        lock(&self.heard).len()
    }

    fn timed(&self) -> Vec<(Instant, Heard)> {
        lock(&self.heard).clone()
    }

    // Every event handed over, in order.
    fn events(&self) -> Vec<InputEvent> {
        self.events_since(0)
    }

    fn events_since(&self, mark: usize) -> Vec<InputEvent> {
        self.heard_since(mark)
            .into_iter()
            .flat_map(|heard| match heard {
                Heard::Input { events, .. } => events,
                _ => Vec::new(),
            })
            .collect()
    }

    fn inputs(&self) -> usize {
        self.heard()
            .iter()
            .filter(|heard| matches!(heard, Heard::Input { .. }))
            .count()
    }

    fn ended(&self) -> Option<ControlEnd> {
        self.ended_since(0)
    }

    fn ended_since(&self, mark: usize) -> Option<ControlEnd> {
        self.heard_since(mark)
            .into_iter()
            .rev()
            .find_map(|heard| match heard {
                Heard::Ended(why) => Some(why),
                _ => None,
            })
    }
}

impl Injector for Recorder {
    fn started(&self, started: &Started) {
        lock(&self.heard).push((Instant::now(), Heard::Started(started.clone())));
    }

    fn inject(&self, input: &Injection<'_>) -> Injected {
        lock(&self.heard).push((
            Instant::now(),
            Heard::Input {
                events: input.events.to_vec(),
                held: *input.held,
                area: input.area,
            },
        ));
        Injected {
            sent: input.events.len() as u32,
            admin: self.admin.load(Ordering::Relaxed),
            ..Injected::default()
        }
    }

    fn cut_off(&self) {
        lock(&self.heard).push((Instant::now(), Heard::CutOff));
    }

    fn ended(&self, why: ControlEnd) {
        lock(&self.heard).push((Instant::now(), Heard::Ended(why)));
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

struct Person {
    member: Member,
    injector: Arc<Recorder>,
}

impl Person {
    fn view(&self) -> View {
        self.member.view()
    }

    fn room(&self) -> &room::Room {
        self.member.room()
    }

    fn wait_for(&self, what: &str, ok: impl Fn(&View) -> bool) -> View {
        self.member.wait_for(WAIT, what, ok)
    }
}

fn injected_config(name: &str, timers: Timers) -> (room::Config, Arc<Recorder>) {
    let injector = Arc::new(Recorder::default());
    let mut config = config(name, timers);
    config.video.injector = Some(Arc::clone(&injector) as Arc<dyn Injector>);
    (config, injector)
}

// A host and friends who joined with one multi-use invite, each with a
// recording injector, all live and in everyone's roster.
fn room_of(names: &[&str], timers: Timers) -> (Person, Vec<Person>, Invite) {
    let (config, injector) = injected_config(names[0], timers);
    let host = Person {
        member: Member::host_with(config),
        injector,
    };
    host.room().new_invite(true);
    let view = host.wait_for("a multi-use invite", |v| {
        v.invite
            .as_ref()
            .is_some_and(|i| i.multi_use && !i.code.is_empty())
    });
    let invite = code_to_invite(
        &view.invite.expect("an invite").code,
        loopback(host.member.port()),
    );
    let friends: Vec<Person> = names[1..]
        .iter()
        .map(|name| {
            let (config, injector) = injected_config(name, timers);
            Person {
                member: Member::join_with(
                    config,
                    Arc::new(keys::Identity::generate()),
                    invite.clone(),
                ),
                injector,
            }
        })
        .collect();
    let people = names.len();
    for friend in &friends {
        friend.wait_for("live with everyone", |v| {
            v.strip.state == LinkState::Live
                && v.people.len() == people
                && v.numbers.clock_offset_ms.is_some()
        });
    }
    host.wait_for("everyone in", |v| v.people.len() == people);
    (host, friends, invite)
}

fn own_share(view: &View) -> Option<u32> {
    match view.share.own {
        room::screen::OwnShare::Sharing { number, .. } => Some(number),
        _ => None,
    }
}

// `sharer` shares, as a monitor at 1920,0 of 2560x1440 as far as the room
// knows, and each of `watchers` watches.
fn shares(sharer: &Person, watchers: &[&Person]) -> u32 {
    sharer.room().share(60, None);
    let share = own_share(&sharer.wait_for("the share granted", |v| own_share(v).is_some()))
        .expect("granted");
    sharer.room().sharing().set_area(Some(MONITOR));
    for watcher in watchers {
        watcher.wait_for("the share in the roster", |v| {
            v.share.current.as_ref().is_some_and(|c| c.number == share)
        });
        watcher.room().watch(share, true);
        watcher.wait_for("watching", |v| v.share.watching);
    }
    share
}

const MONITOR: Area = Area {
    left: 1920,
    top: 0,
    width: 2560,
    height: 1440,
};

fn name_of(view: &View, key: [u8; 32]) -> Option<String> {
    view.people
        .iter()
        .find(|person| person.key == key)
        .map(|person| person.name.clone())
}

// `controller` asks for `share` and `sharer` allows it.
fn controls(controller: &Person, sharer: &Person, share: u32) {
    controller.room().ask_control(share);
    controller.wait_for("asked", |v| {
        v.share.control.asking == Some(share) || v.share.control.controlling.is_some()
    });
    let controller_name = controller
        .view()
        .people
        .iter()
        .find(|p| p.is_you)
        .map(|p| p.name.clone());
    let asked = sharer.wait_for("the request on show", |v| {
        v.share
            .control
            .asked_by
            .as_ref()
            .map(|party| Some(party.name.clone()))
            == Some(controller_name.clone())
    });
    sharer.room().answer_control(request(&asked), true);
    controller.wait_for("controlling", |v| v.share.control.controlling.is_some());
    sharer.wait_for("controlled", |v| v.share.control.controlled_by.is_some());
}

// The number of the request a view shows, which the answer names.
fn request(view: &View) -> u32 {
    view.share
        .control
        .asked_by
        .as_ref()
        .expect("a request on show")
        .number
}

fn system_lines(view: &View) -> Vec<String> {
    view.chat
        .iter()
        .filter(|line| line.kind == LineKind::System)
        .map(|line| line.text.clone())
        .collect()
}

fn key(code: u8, down: bool) -> InputEvent {
    InputEvent::Key {
        key: ScanCode { code, e0: false },
        down,
    }
}

const A: u8 = 0x1E;

// Made-up input with every kind of event, and the held state it leaves.
fn made_up() -> (Vec<InputEvent>, Held) {
    let events = vec![
        key(A, true),
        InputEvent::Move { dx: 5, dy: -3 },
        InputEvent::Button {
            button: Button::Left,
            down: true,
        },
        InputEvent::Wheel { delta: 120 },
        InputEvent::HWheel { delta: -120 },
        key(0x2A, true),
        key(A, false),
    ];
    let mut held = Held::default();
    held.set_key(
        ScanCode {
            code: 0x2A,
            e0: false,
        },
        true,
    );
    held.set_button(Button::Left, true);
    (events, held)
}

// The whole path for one pair: ask, allow, made-up events in order with
// their held state, the numbers, and Stop control from the controller.
fn control_between(controller: &Person, sharer: &Person, share: u32, who: &str) {
    let mark = sharer.injector.mark();
    controls(controller, sharer, share);
    let started = poll(WAIT, "the injector started", || {
        sharer
            .injector
            .heard_since(mark)
            .into_iter()
            .find_map(|heard| match heard {
                Heard::Started(started) => Some(started),
                _ => None,
            })
    });
    let controller_name = name_of(&controller.view(), *controller_key(controller)).expect("a name");
    assert_eq!(started.name, controller_name, "{who}");
    assert_eq!(started.area, Some(MONITOR), "{who}");
    let controls = controller.room().controls();
    assert!(controls.controlling(), "{who}");
    let (events, held) = made_up();
    controls.send(&events, Instant::now());
    controls.send(&[InputEvent::At { x: 32768, y: 100 }], Instant::now());
    let want: Vec<InputEvent> = events
        .iter()
        .copied()
        .chain([InputEvent::At { x: 32768, y: 100 }])
        .collect();
    poll(WAIT, "every event injected", || {
        (sharer.injector.events_since(mark).len() >= want.len()).then_some(())
    });
    assert_eq!(
        sharer.injector.events_since(mark),
        want,
        "{who}: in order, all of them"
    );
    let last_held = sharer
        .injector
        .heard_since(mark)
        .into_iter()
        .rev()
        .find_map(|heard| match heard {
            Heard::Input { held, area, .. } => Some((held, area)),
            _ => None,
        })
        .expect("input");
    assert_eq!(last_held, (held, Some(MONITOR)), "{who}");
    // Every packet carries the held state, including the one every 100 ms
    // with no events.
    let before = sharer.injector.inputs();
    thread::sleep(Duration::from_millis(350));
    let beats = sharer.injector.inputs() - before;
    assert!(
        (2..=5).contains(&beats),
        "{who}: {beats} held-state packets"
    );
    let numbers = sharer
        .wait_for("capture to inject measured", |v| {
            v.numbers.control.capture_to_inject.is_some()
        })
        .numbers
        .control;
    println!(
        "{who}: capture to inject {:?}, receive to inject {:?}, inject call {:?}, {} injected, {} packets sent by the controller",
        numbers.capture_to_inject,
        numbers.receive_to_inject,
        numbers.inject_call,
        numbers.injected,
        controller.view().numbers.control.packets_sent
    );
    let capture = numbers.capture_to_inject.expect("measured");
    // On one PC the clock offsets read a few ms off zero, since each copy
    // ties its clock to the wall clock once and Windows time sync moves the
    // wall clock in between. share.rs allows 2 ms for it.
    assert!(capture.median_ms < 20.0, "{who}: {capture:?}");
    assert!(numbers.injected >= want.len() as u64, "{who}: {numbers:?}");
    let receive = numbers.receive_to_inject.expect("measured");
    assert!(receive.p95_ms < 5.0, "{who}: {receive:?}");

    controller.room().stop_control();
    controller.wait_for("control over", |v| v.share.control.controlling.is_none());
    sharer.wait_for("control over here", |v| {
        v.share.control.controlled_by.is_none()
    });
    poll(WAIT, "the injector told", || {
        sharer.injector.ended_since(mark)
    });
    assert_eq!(
        sharer.injector.ended_since(mark),
        Some(ControlEnd::Released),
        "{who}"
    );
    assert!(!controls.controlling(), "{who}: nothing goes after the end");
    let lines = system_lines(&controller.view());
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("You are controlling ")),
        "{who}: {lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("You stopped controlling ")),
        "{who}: {lines:?}"
    );
    let lines = system_lines(&sharer.view());
    assert!(
        lines.contains(&format!("{controller_name} is controlling this PC.")),
        "{who}: {lines:?}"
    );
    assert!(
        lines.contains(&format!("{controller_name} stopped controlling this PC.")),
        "{who}: {lines:?}"
    );
}

fn controller_key(person: &Person) -> &[u8; 32] {
    person.member.identity.public()
}

#[test]
fn friend_controls_the_hosts_share() {
    let (host, friends, _) = room_of(&["Mara", "Ana", "Bo"], timers());
    let (ana, bo) = (&friends[0], &friends[1]);
    let share = shares(&host, &[ana, bo]);
    ana.room().ask_control(share);
    let asked = host.wait_for("Ana's request", |v| v.share.control.asked_by.is_some());
    assert_eq!(
        asked
            .share
            .control
            .asked_by
            .as_ref()
            .map(|party| &party.name[..]),
        Some("Ana")
    );
    assert!(host.injector.heard().is_empty(), "nothing before Allow");
    host.room().answer_control(request(&asked), true);
    // Everyone sees who controls, from the roster.
    bo.wait_for("Ana controls, as Bo sees it", |v| {
        v.share.control.controller == Some(*controller_key(ana))
    });
    // The rest of the path, from an ask already allowed.
    ana.wait_for("controlling", |v| v.share.control.controlling.is_some());
    ana.room().stop_control();
    host.wait_for("over", |v| v.share.control.controlled_by.is_none());
    control_between(ana, &host, share, "Ana, the host's share");
    bo.wait_for("nobody controls", |v| v.share.control.controller.is_none());
}

#[test]
fn friend_controls_a_friends_share() {
    let (host, friends, _) = room_of(&["Mara", "Ana", "Bo"], timers());
    let (ana, bo) = (&friends[0], &friends[1]);
    let share = shares(ana, &[bo]);
    control_between(bo, ana, share, "Bo, Ana's share through the host");
    assert!(host.injector.heard().is_empty(), "the host injects nothing");
}

#[test]
fn host_controls_a_friends_share() {
    let (host, friends, _) = room_of(&["Mara", "Ana"], timers());
    let ana = &friends[0];
    let share = shares(ana, &[&host]);
    control_between(&host, ana, share, "the host, Ana's share");
}

#[test]
fn dont_allow_leaves_nothing_behind() {
    let (_host, friends, _) = room_of(&["Mara", "Ana", "Bo"], timers());
    let (ana, bo) = (&friends[0], &friends[1]);
    let share = shares(ana, &[bo]);
    bo.room().ask_control(share);
    let asked = ana.wait_for("Bo's request", |v| v.share.control.asked_by.is_some());
    ana.room().answer_control(request(&asked), false);
    ana.wait_for("the request gone", |v| v.share.control.asked_by.is_none());
    let view = bo.wait_for("the answer", |v| v.share.control.asking.is_none());
    assert!(view.share.control.controlling.is_none());
    assert!(
        system_lines(&view).contains(&String::from("Ana did not allow control.")),
        "{:?}",
        system_lines(&view)
    );
    let controls = bo.room().controls();
    assert!(!controls.controlling());
    controls.send(&[key(A, true)], Instant::now());
    thread::sleep(Duration::from_millis(300));
    assert!(
        ana.injector.heard().is_empty(),
        "{:?}",
        ana.injector.heard()
    );
    assert_eq!(bo.view().numbers.control.packets_sent, 0);
}

#[test]
fn one_controller_at_a_time() {
    let (_host, friends, _) = room_of(&["Mara", "Ana", "Bo", "Cy"], timers());
    let (ana, bo, cy) = (&friends[0], &friends[1], &friends[2]);
    let share = shares(ana, &[bo, cy]);
    // Bo asked first and waits for the answer: Cy is refused with his name.
    bo.room().ask_control(share);
    let asked = ana.wait_for("Bo's request", |v| v.share.control.asked_by.is_some());
    cy.room().ask_control(share);
    let view = cy.wait_for("refused", |v| {
        v.share.control.asking.is_none() && !system_lines(v).is_empty()
    });
    let busy = String::from("Could not ask for control: Bo asked first. One controller at a time.");
    assert!(
        system_lines(&view).contains(&busy),
        "{:?}",
        system_lines(&view)
    );
    ana.room().answer_control(request(&asked), true);
    bo.wait_for("Bo controls", |v| v.share.control.controlling.is_some());
    cy.wait_for("Cy sees Bo control", |v| {
        v.share.control.controller == Some(*controller_key(bo))
    });
    // Asked again while Bo controls, refused at once from the roster.
    cy.room().ask_control(share);
    let view = cy.wait_for("refused again", |v| {
        system_lines(v).iter().filter(|line| **line == busy).count() == 2
    });
    assert!(view.share.control.asking.is_none());
    assert_eq!(
        ana.view()
            .share
            .control
            .controlled_by
            .map(|party| party.name),
        Some(String::from("Bo"))
    );
}

fn asker(view: &View) -> Option<String> {
    view.share
        .control
        .asked_by
        .as_ref()
        .map(|request| request.name.clone())
}

// The request on show can change between the owner reading it and the
// click: Bo takes his ask back and Cy's lands at once. An Allow that names
// Bo's request allows nobody, and Cy's waits for its own answer. On the
// host's own share, and on a friend's through the host.
#[test]
fn allow_answers_only_the_request_it_names() {
    let (host, friends, _) = room_of(&["Mara", "Ana", "Bo", "Cy"], timers());
    let (ana, bo, cy) = (&friends[0], &friends[1], &friends[2]);
    for (who, sharer) in [("Mara", &host), ("Ana", ana)] {
        let mark = sharer.injector.mark();
        let share = shares(sharer, &[bo, cy]);
        bo.room().ask_control(share);
        let bos = sharer.wait_for("Bo's request", |v| asker(v).as_deref() == Some("Bo"));
        bo.room().stop_control();
        sharer.wait_for("Bo's request gone", |v| v.share.control.asked_by.is_none());
        cy.room().ask_control(share);
        let cys = sharer.wait_for("Cy's request", |v| asker(v).as_deref() == Some("Cy"));
        assert_ne!(request(&bos), request(&cys), "{who}");
        sharer.room().answer_control(request(&bos), true);
        thread::sleep(Duration::from_millis(300));
        let view = sharer.view();
        assert_eq!(asker(&view).as_deref(), Some("Cy"), "{who}: still waiting");
        assert!(view.share.control.controlled_by.is_none(), "{who}");
        assert!(sharer.injector.heard_since(mark).is_empty(), "{who}");
        assert_eq!(cy.view().share.control.asking, Some(share), "{who}");
        assert!(!cy.room().controls().controlling(), "{who}");
        sharer.room().answer_control(request(&cys), false);
        let view = cy.wait_for("Cy's answer", |v| v.share.control.asking.is_none());
        assert!(
            system_lines(&view).contains(&format!("{who} did not allow control.")),
            "{who}: {:?}",
            system_lines(&view)
        );
        sharer.room().stop_sharing();
        for member in [&host, ana, bo, cy] {
            member.wait_for("no share", |v| v.share.current.is_none());
        }
    }
}

fn count_bad(member: &Person) -> u64 {
    member.view().numbers.dropped_bad
}

// A made-up input packet as a controller sends it: kind SENT, slot 0, a
// sequence number, a capture time, no flags, `held` keys held, and the
// events as the wire has them.
fn input(seq: u32, held: &[u8], count: u8, events: &[u8]) -> Vec<u8> {
    let mut out = vec![1, 0];
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(&1_700_000_000_000_000u64.to_le_bytes());
    out.extend_from_slice(&[0, 0]);
    let mut keys = [0u8; 64];
    for &code in held {
        keys[usize::from(code) / 8] |= 1 << (code % 8);
    }
    out.extend_from_slice(&keys);
    out.push(count);
    out.extend_from_slice(events);
    out
}

fn press(code: u8) -> [u8; 3] {
    [1, code, 1]
}

fn watch(hand: &mut Hand, share: u32) {
    let mut message = vec![12];
    message.extend_from_slice(&share.to_le_bytes());
    message.extend_from_slice(&[1, 0]);
    hand.say(&message);
}

fn ask(hand: &mut Hand, share: u32, ask: u32) {
    let mut message = vec![19];
    message.extend_from_slice(&share.to_le_bytes());
    message.extend_from_slice(&ask.to_le_bytes());
    hand.say(&message);
}

// Input from someone the sharer did not allow goes nowhere and counts as
// bad: before asking, while the ask waits, and after Don't allow.
#[test]
fn input_without_approval_counts_as_bad() {
    let (host, _friends, invite) = room_of(&["Mara", "Ana"], timers());
    let share = shares(&host, &[]);
    let mut eve = Hand::join(&invite, loopback(host.member.port()), "Eve");
    host.wait_for("Eve in", |v| v.people.len() == 3);
    watch(&mut eve, share);
    host.wait_for("Eve watching", |v| {
        v.share.current.as_ref().and_then(|c| c.watchers) == Some(1)
    });
    let mut sent = 0u64;
    let send = |eve: &mut Hand, seq: u32| {
        eve.send(Channel::Input, &input(seq, &[A], 1, &press(A)));
    };
    let before = count_bad(&host);
    for seq in 0..5 {
        send(&mut eve, seq);
        sent += 1;
    }
    ask(&mut eve, share, 1);
    let asked = host.wait_for("Eve's request", |v| v.share.control.asked_by.is_some());
    for seq in 5..10 {
        send(&mut eve, seq);
        sent += 1;
    }
    host.room().answer_control(request(&asked), false);
    host.wait_for("declined", |v| v.share.control.asked_by.is_none());
    for seq in 10..15 {
        send(&mut eve, seq);
        sent += 1;
    }
    let view = host.wait_for("all counted as bad", |v| {
        v.numbers.dropped_bad >= before + sent
    });
    println!(
        "{sent} input packets from Eve, never allowed: {} counted as bad",
        view.numbers.dropped_bad - before
    );
    thread::sleep(Duration::from_millis(200));
    assert!(
        host.injector.heard().is_empty(),
        "{:?}",
        host.injector.heard()
    );
}

// The cutoff lets go of everything held when no packet came for 500 ms, and
// runs even though no packet comes to run it. Eve holds a key and goes quiet,
// as a controller whose link died would: on the host's own share, and on a
// friend's through the host.
#[test]
fn cutoff_lets_go_after_half_a_second() {
    let (host, friends, invite) = room_of(&["Mara", "Ana"], timers());
    let ana = &friends[0];
    for (who, sharer) in [("the host", &host), ("Ana", ana)] {
        let share = shares(sharer, &[]);
        // The first round's Eve may still be in the room, until she is lost.
        let mut eve = Hand::join(&invite, loopback(host.member.port()), "Eve");
        host.wait_for("Eve in", |v| v.people.len() >= 3);
        watch(&mut eve, share);
        ask(&mut eve, share, 1);
        let asked = sharer.wait_for("Eve's request", |v| v.share.control.asked_by.is_some());
        sharer.room().answer_control(request(&asked), true);
        sharer.wait_for("controlled", |v| v.share.control.controlled_by.is_some());
        // Her answer comes before her first packet, as with a real
        // controller.
        thread::sleep(Duration::from_millis(100));
        eve.send(Channel::Input, &input(1, &[A], 1, &press(A)));
        let cut = poll(Duration::from_secs(2), "the cutoff", || {
            let timed = sharer.injector.timed();
            let pressed = timed
                .iter()
                .find(|(_, heard)| matches!(heard, Heard::Input { .. }))?
                .0;
            let cut = timed.iter().find(|(_, heard)| *heard == Heard::CutOff)?.0;
            Some(cut.saturating_duration_since(pressed))
        });
        println!(
            "{who}: the cutoff let go {:.1} ms after the last packet",
            cut.as_secs_f64() * 1000.0
        );
        // Counted from the packet's arrival, which is a moment before the
        // injector is handed it.
        assert!(
            cut >= Duration::from_millis(495) && cut < Duration::from_millis(600),
            "{who}: {cut:?}"
        );
        let heard = sharer.injector.heard();
        let mut held = Held::default();
        held.set_key(ScanCode { code: A, e0: false }, true);
        assert_eq!(
            heard[..3],
            [
                Heard::Started(Started {
                    name: String::from("Eve"),
                    area: Some(MONITOR),
                }),
                Heard::Input {
                    events: vec![key(A, true)],
                    held,
                    area: Some(MONITOR),
                },
                Heard::CutOff,
            ],
            "{who}"
        );
        let numbers = sharer
            .wait_for("counted", |v| v.numbers.control.cutoffs == 1)
            .numbers;
        assert_eq!(numbers.control.cutoffs, 1, "{who}");
        // Control goes on: the next packet is taken.
        eve.send(Channel::Input, &input(2, &[], 0, &[]));
        poll(WAIT, "the next packet", || {
            (sharer.injector.inputs() == 2).then_some(())
        });
        // Eve leaves, and with her the control; the share ends for the next
        // round.
        drop(eve);
        sharer.room().stop_sharing();
        for member in [&host, ana] {
            member.wait_for("no share", |v| v.share.current.is_none());
        }
    }
}

// A controller's program can send as fast as its link goes. Past the host's
// burst and rate, input is dropped and counted, and none of it waits.
#[test]
fn input_flood_is_limited() {
    let (host, _friends, invite) = room_of(&["Mara", "Ana"], timers());
    let share = shares(&host, &[]);
    let mut eve = Hand::join(&invite, loopback(host.member.port()), "Eve");
    host.wait_for("Eve in", |v| v.people.len() == 3);
    watch(&mut eve, share);
    ask(&mut eve, share, 1);
    let asked = host.wait_for("Eve's request", |v| v.share.control.asked_by.is_some());
    host.room().answer_control(request(&asked), true);
    host.wait_for("controlled", |v| v.share.control.controlled_by.is_some());
    thread::sleep(Duration::from_millis(100));
    let started = Instant::now();
    for seq in 0..1000u32 {
        eve.send(Channel::Input, &input(seq, &[], 0, &[]));
    }
    let numbers = host
        .wait_for("every packet counted", |v| {
            host.injector.inputs() as u64 + v.numbers.control.dropped.host_over_rate >= 1000
        })
        .numbers;
    let took = started.elapsed().as_secs_f64();
    let most = 200.0 + 1000.0 * took;
    let taken = host.injector.inputs();
    println!(
        "1000 input packets from Eve: {taken} injected, {} dropped; {most:.0} may pass in the {:.0} ms it took",
        numbers.control.dropped.host_over_rate,
        took * 1000.0
    );
    assert!(numbers.control.dropped.host_over_rate > 0);
    assert!(taken as f64 <= most + 1.0);
}

struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| self.next() as u8).collect()
    }
}

// Whatever a controller's program sends as input or as the control
// messages remote control added, the host and the sharer take none of it as
// anything but what it is, and the room goes on: afterwards a friend still
// asks, is allowed and controls.
#[test]
fn hostile_input_breaks_nothing() {
    let (host, friends, invite) = room_of(&["Mara", "Ana", "Bo"], timers());
    let (ana, bo) = (&friends[0], &friends[1]);
    let share = shares(ana, &[bo]);
    let mut eve = Hand::join(&invite, loopback(host.member.port()), "Eve");
    host.wait_for("Eve in", |v| v.people.len() == 4);
    watch(&mut eve, share);
    ask(&mut eve, share, 9);
    let asked = ana.wait_for("Eve's request", |v| v.share.control.asked_by.is_some());
    ana.room().answer_control(request(&asked), true);
    ana.wait_for("Eve controls", |v| v.share.control.controlled_by.is_some());
    let good = input(1, &[A], 1, &press(A));
    let mut random = Random(0x9E37_79B9_7F4A_7C15);
    for round in 0..3000usize {
        let len = (random.next() % 400) as usize;
        match round % 4 {
            0 => eve.send(Channel::Input, &random.bytes(len)),
            1 => {
                let mut packet = good.clone();
                for _ in 0..3 {
                    let at = (random.next() as usize) % packet.len();
                    packet[at] = random.next() as u8;
                }
                eve.send(Channel::Input, &packet);
            }
            2 => {
                let mut message = vec![19 + (random.next() % 5) as u8];
                message.extend(random.bytes(len % 60));
                eve.say(&message);
            }
            _ => {
                let mut message = vec![22];
                message.extend_from_slice(&share.to_le_bytes());
                message.extend(random.bytes(5));
                eve.say(&message);
            }
        }
        if round % 5 == 4 {
            thread::sleep(Duration::from_millis(1));
        }
    }
    println!(
        "3000 hostile input packets and messages from Eve; the host counted {} bad, Ana's injector was handed {} packets",
        count_bad(&host),
        ana.injector.inputs()
    );
    // Whatever reached Ana's injector is what the wire allows.
    for heard in ana.injector.heard() {
        if let Heard::Input { events, held, .. } = heard {
            assert!(events.len() <= room::remote::MAX_EVENTS);
            assert!(held.keys().all(|key| key.code != 0 && key.code != 0xFF));
        }
    }
    // Eve lets go, however her messages left it, and Bo takes over.
    ana.room().stop_control();
    ana.wait_for("no control", |v| {
        v.share.control.controlled_by.is_none() && v.share.control.asked_by.is_none()
    });
    host.wait_for("no controller", |v| v.share.control.controller.is_none());
    control_between(bo, ana, share, "Bo, after Eve");
    for member in [&host, ana, bo] {
        assert_eq!(member.view().strip.state, LinkState::Live);
    }
}

// The host's End control, from the row menu: both sides are told the host
// ended it, and the injector lets go.
#[test]
fn host_ends_control_for_both() {
    let (host, friends, _) = room_of(&["Mara", "Ana", "Bo"], timers());
    let (ana, bo) = (&friends[0], &friends[1]);
    let share = shares(ana, &[bo]);
    controls(bo, ana, share);
    host.wait_for("the host sees Bo control", |v| {
        v.share.control.controller == Some(*controller_key(bo))
    });
    host.room().end_control();
    let ended = String::from("Control ended: the host ended it.");
    for (who, person) in [("Ana", ana), ("Bo", bo)] {
        let view = person.wait_for("ended", |v| {
            v.share.control.controlled_by.is_none() && v.share.control.controlling.is_none()
        });
        assert!(
            system_lines(&view).contains(&ended),
            "{who}: {:?}",
            system_lines(&view)
        );
    }
    poll(WAIT, "Ana's injector told", || ana.injector.ended());
    assert_eq!(ana.injector.ended(), Some(ControlEnd::EndedByHost));
    let lines = system_lines(&host.view());
    assert!(
        lines.contains(&String::from("Control of Ana's PC ended: you ended it.")),
        "{lines:?}"
    );
    assert!(!bo.room().controls().controlling());
}

// Control dies with the share, and with the panic key, and each side says
// why.
#[test]
fn share_end_and_panic_key_end_control() {
    let (_host, friends, _) = room_of(&["Mara", "Ana", "Bo"], timers());
    let (ana, bo) = (&friends[0], &friends[1]);
    let share = shares(ana, &[bo]);
    controls(bo, ana, share);
    ana.room().panic_key();
    bo.wait_for("ended by the panic key", |v| {
        v.share.control.controlling.is_none()
    });
    let panic = String::from("Control ended: the panic key.");
    assert!(system_lines(&bo.view()).contains(&panic));
    assert!(system_lines(&ana.view()).contains(&panic));
    // Made on Ana's timer thread, woken by the panic key's call.
    poll(WAIT, "Ana's injector told", || ana.injector.ended());
    assert_eq!(ana.injector.ended(), Some(ControlEnd::Panic));

    controls(bo, ana, share);
    ana.room().stop_sharing();
    let view = bo.wait_for("ended with the share", |v| {
        v.share.control.controlling.is_none() && v.share.current.is_none()
    });
    let ended = String::from("Control ended: the share ended.");
    assert!(
        system_lines(&view).contains(&ended),
        "{:?}",
        system_lines(&view)
    );
    poll(WAIT, "Ana's injector told", || {
        (ana.injector.ended() == Some(ControlEnd::ShareEnded)).then_some(())
    });
    assert!(system_lines(&ana.view()).contains(&ended));
    assert!(!bo.room().controls().controlling());
}

// A rekey keeps control, a fresh handshake after the session was lost does
// not, and the controller has to ask again.
#[test]
fn rekey_keeps_control_new_session_ends_it() {
    let rekeying = Timers {
        rekey_after: Duration::from_millis(700),
        ..timers()
    };
    let (config, host_injector) = injected_config("Mara", rekeying);
    let host = Person {
        member: Member::host_with(config),
        injector: host_injector,
    };
    let (config, ana_injector) = injected_config("Ana", rekeying);
    let forwarder = Forwarder::new(loopback(host.member.port()));
    let invite = common::invite_to(&host.member, forwarder.addr);
    let ana = Person {
        member: Member::join_with(config, Arc::new(keys::Identity::generate()), invite),
        injector: ana_injector,
    };
    ana.wait_for("live", |v| {
        v.strip.state == LinkState::Live && v.people.len() == 2
    });
    let share = shares(&host, &[&ana]);
    controls(&ana, &host, share);
    let rekeys = ana
        .wait_for("two rekeys", |v| v.numbers.rekeys >= 2)
        .numbers
        .rekeys;
    let view = ana.view();
    assert!(
        view.share.control.controlling.is_some(),
        "still controlling"
    );
    assert!(host.view().share.control.controlled_by.is_some());
    let before = host.injector.events().len();
    ana.room()
        .controls()
        .send(&[key(A, true), key(A, false)], Instant::now());
    poll(WAIT, "input after the rekeys", || {
        (host.injector.events().len() >= before + 2).then_some(())
    });
    println!("after {rekeys} rekeys control goes on and input still arrives");

    // Lost for longer than lost_after, then back through a new handshake.
    let long = Duration::from_secs(8);
    forwarder.block(true);
    ana.member
        .wait_for(long, "lost", |v| v.strip.state == LinkState::Lost);
    host.member.wait_for(long, "control over on the host", |v| {
        v.share.control.controlled_by.is_none()
    });
    forwarder.block(false);
    let view = ana
        .member
        .wait_for(long, "back", |v| v.strip.state == LinkState::Live);
    assert!(view.share.control.controlling.is_none());
    assert!(view.share.control.asking.is_none());
    assert_eq!(host.injector.ended(), Some(ControlEnd::SessionLost));
    let lost = String::from("Control ended: the connection was lost.");
    assert!(
        system_lines(&view).contains(&lost),
        "{:?}",
        system_lines(&view)
    );
    assert!(system_lines(&host.view()).contains(&lost));
    let before = host.injector.inputs();
    assert!(!ana.room().controls().controlling());
    ana.room().controls().send(&[key(A, true)], Instant::now());
    thread::sleep(Duration::from_millis(300));
    assert_eq!(
        host.injector.inputs(),
        before,
        "nothing without a new Allow"
    );
}

// Control starting and stopping plays its own cue in the ears of the one
// controlled and the controller, even deafened, since over a game in
// exclusive fullscreen the sound is what tells them. The share cue plays
// nowhere here: the share's thread, played by hand, never says it opened.
#[test]
fn control_cues_play_even_deafened() {
    let _alone = alone();
    let (mut config, microphone, speakers) =
        voiced("Mara", timers(), silence, TalkMode::PushToTalk, true);
    let injector = Arc::new(Recorder::default());
    config.video.injector = Some(Arc::clone(&injector) as Arc<dyn Injector>);
    let host = Voiced {
        member: Member::host_with(config),
        microphone,
        speakers,
    };
    let ana = Voiced::join(
        voiced("Ana", timers(), silence, TalkMode::PushToTalk, true),
        common::host_invite(&host.member),
    );
    settled(&host, &[&ana]);
    host.room().deafen(true);
    ana.room().deafen(true);
    host.room().share(60, None);
    let view = host
        .member
        .wait_for(WAIT, "the host's share", |v| own_share(v).is_some());
    let share = own_share(&view).expect("granted");
    ana.member
        .wait_for(WAIT, "the share", |v| v.share.current.is_some());
    ana.room().watch(share, true);
    ana.member.wait_for(WAIT, "watching", |v| v.share.watching);
    ana.room().ask_control(share);
    let asked = host
        .member
        .wait_for(WAIT, "the request", |v| v.share.control.asked_by.is_some());
    assert!(cues(&host.speakers).is_empty(), "a request plays nothing");
    host.room().answer_control(request(&asked), true);
    for (who, voiced) in [("Mara", &host), ("Ana", &ana)] {
        let heard = heard_cues(&voiced.speakers, 1);
        assert_eq!(heard.len(), 1, "{who}: {heard:?}");
        check_control_cue(&heard[0], true, who);
    }
    ana.room().stop_control();
    for (who, voiced) in [("Mara", &host), ("Ana", &ana)] {
        let heard = heard_cues(&voiced.speakers, 2);
        assert_eq!(heard.len(), 2, "{who}: {heard:?}");
        check_control_cue(&heard[1], false, who);
    }
    // Long enough for a cue played twice to be heard whole.
    thread::sleep(Duration::from_millis(500));
    for (who, voiced) in [("Mara", &host), ("Ana", &ana)] {
        assert_eq!(cues(&voiced.speakers).len(), 2, "{who}");
    }
}

// Two 80 ms notes less their quiet ends, as the share cue, at the level of
// a voice: deafen passes the control cue through as it is.
fn check_control_cue(cue: &Cue, rising: bool, who: &str) {
    println!("{who}: {cue:?}");
    assert_eq!(cue.rising, rising, "{who}: {cue:?}");
    assert!((7400..=7680).contains(&cue.len), "{who}: {cue:?}");
    assert!(
        cue.peak > 0.1 && cue.peak <= voice::mix::KNEE,
        "{who}: {cue:?}"
    );
}

// An administrator window on the PC controlled pauses control, and the
// controller's view says so until it goes.
#[test]
fn admin_window_pauses_control() {
    let (_host, friends, _) = room_of(&["Mara", "Ana", "Bo"], timers());
    let (ana, bo) = (&friends[0], &friends[1]);
    let share = shares(ana, &[bo]);
    controls(bo, ana, share);
    ana.injector.admin.store(true, Ordering::Relaxed);
    bo.wait_for("paused", |v| v.share.control.paused);
    ana.wait_for("paused here", |v| v.share.control.admin_here);
    ana.injector.admin.store(false, Ordering::Relaxed);
    bo.wait_for("going again", |v| !v.share.control.paused);
}

// Nothing in an invite or a join grants control, and a friend who is not
// watching cannot ask.
#[test]
fn control_is_off_until_a_watcher_asks() {
    let (host, friends, _) = room_of(&["Mara", "Ana", "Bo"], timers());
    let (ana, bo) = (&friends[0], &friends[1]);
    let share = shares(ana, &[]);
    bo.room().ask_control(share);
    thread::sleep(Duration::from_millis(300));
    for member in [&host, ana, bo] {
        let control = member.view().share.control;
        assert!(
            control.asked_by.is_none()
                && control.controlled_by.is_none()
                && control.asking.is_none()
                && control.controlling.is_none(),
            "{control:?}"
        );
    }
    assert!(ana.injector.heard().is_empty());
}
