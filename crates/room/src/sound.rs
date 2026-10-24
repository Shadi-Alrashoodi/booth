// PC sound in a room: what a sharer's PC plays, sent with the share on a
// stream of its own (Channel::Sound) to the share's watchers whose copies
// read it (peer::PC_SOUND_SINCE), through the host as video is.
//
// The sharer's side: the room thread owns the capture (voice's PcSound) while
// Share sound is on and the share is not paused, never the voice devices
// thread, which can wait seconds behind a headset (Switch). The capture's own
// thread cuts, encodes, seals and sends each frame (send.rs), reading the
// share's gate and the links to send on from Outgoing once a packet. Every
// press that stops the sound stores its stop in the gate before anything says
// so, on the thread that pressed, so nothing captured after it goes out
// whatever the room is doing.
//
// A watcher hears it through one ear (hear.rs), the share watched's: made by
// the first frame of it that comes, and let go with the watch, the share, a
// new session, or the sharer saying its sound is off. The receive thread
// pushes into it under the state lock and the render thread pulls from it
// through the mixer, each holding the ear's own lock for one frame, as
// voice's ears are (talk.rs).

mod hear;
mod pairing;
mod send;
mod wire;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, Weak};
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use voice::audio::fake::Fake;
use voice::audio::{AudioError, Event, PcSound};

use crate::limit::Bucket;
use crate::log::{Log, log};
use crate::peer::{Clock, MEDIA_FLOWS_FOR};
use crate::screen::{MOST_SHOWN, Watching};
use crate::socket::Socket;
use crate::talk::{MOST_VOLUME, Outlet, RETIRED_FOR, ToSpeaker};
use crate::view::{AfterPicture, Latency, Numbers, SoundNumbers};

pub(crate) use hear::{Ear, Stream};
pub(crate) use wire::{MAX_SOUND, SoundFrame, read_relayed, read_sent, relay_in_place};

use pairing::Pairing;
use send::Feed;

// A sharer sends 100 frames a second, 10 ms each. Past this rate the host
// drops what comes, since each frame goes on to every watcher; the burst
// covers a sharer's capture thread catching up after a stall.
pub(crate) const SOUND_PER_SECOND: f64 = 150.0;
pub(crate) const SOUND_BURST: f64 = 50.0;

// Half the level captured: music stays under the limiter and voices on top.
pub(crate) const SOUND_LEVEL: f32 = 0.5;

// New ears a second: each is a stereo decoder and its buffer, kept
// RETIRED_FOR after it goes. One plays at a time, and a watch, or the sharer
// turning sound off and on, makes one; a sharer that says off before every
// frame, which only a hostile one does, would otherwise make one per frame.
const NEW_EARS_PER_SECOND: f64 = 2.0;
const NEW_EAR_BURST: f64 = 4.0;
// How often the log hears how the sound of the share watched goes, while
// this PC has an ear for it.
const LINE_EVERY: Duration = Duration::from_secs(10);
// The viewer hands its pictures over once a second.
const AGE_EVERY: Duration = Duration::from_secs(1);
// The render thread takes its orders every period, a few to some tens of ms;
// an ear still waiting there this long after it was made means it is away.
const TAKEN_WITHIN: Duration = Duration::from_secs(1);

// The ear of the share watched.
struct Listening {
    share: u32,
    // Its sharer's, which the mixer knows the stream by.
    sharer: [u8; 32],
    ear: Arc<Mutex<Ear>>,
}

// The room's side of PC sound on a watcher, under the state lock: the ear of
// the share it watches, those lately let go, and what the host last said of
// its sound.
pub(crate) struct Sound {
    speaker: Sender<ToSpeaker>,
    clock: Clock,
    log: Log,
    listening: Option<Listening>,
    retired: Vec<(Arc<Mutex<Ear>>, Instant)>,
    new_ears: Bucket,
    // The oldest ear made that the speakers have not taken, and when,
    // while anything still holds it: see listen.
    made: Option<(Weak<Mutex<Ear>>, Instant)>,
    // The share whose sharer said its sound is off after the first word
    // since the watch began, or since a new session asked for it again.
    // Frames captured before the press can come after the word and make no
    // ear: until the on, only a frame that starts a spell from a reset
    // encoder does.
    off: Option<u32>,
    // What the host last said of the sound of the share watched: on or off.
    told: Option<(u32, bool)>,
    // Frames of the share watched that no ear took: past the new-ear limit,
    // or after off.
    dropped: u64,
    // The volume this PC plays each sharer's PC sound at, by key, where it is
    // not 1.0: set from the panel, kept for people not in the room yet, and
    // never sent anywhere. Their voice has its own (talk.rs).
    volumes: HashMap<[u8; 32], f32>,
    // The pictures shown set beside the sound played with them, and the two
    // lists it reads, kept between rounds and made whole at the start, so
    // nothing grows under the lock of the list it is copied from.
    pairing: Pairing,
    pictures: Vec<(u64, u64)>,
    played: Vec<(u64, u64)>,
    // Their spread as the last round that worked it out left it, and when:
    // sorted there, once a second, and not at every view build under the
    // state lock.
    after: Option<AfterPicture>,
    aged: Option<Instant>,
    // When the log next hears how the sound goes, while an ear plays.
    next_line: Option<Instant>,
}

impl Sound {
    pub(crate) fn new(speaker: Sender<ToSpeaker>, clock: Clock, log: Log, now: Instant) -> Sound {
        Sound {
            speaker,
            clock,
            log,
            listening: None,
            retired: Vec::new(),
            new_ears: Bucket::full(now, NEW_EAR_BURST),
            made: None,
            off: None,
            told: None,
            dropped: 0,
            volumes: HashMap::new(),
            pairing: Pairing::default(),
            pictures: Vec::with_capacity(MOST_SHOWN),
            played: Vec::with_capacity(hear::PLAYED_KEPT),
            after: None,
            aged: None,
            next_line: None,
        }
    }

    // A frame of the share this PC watches, from its sharer, checked by
    // read_relayed. `captured` is its capture time on this PC's ping clock,
    // and whether that is only about right, once the offsets are known. True
    // when it made the ear, which the numbers show.
    pub(crate) fn hear(
        &mut self,
        sharer: [u8; 32],
        frame: &SoundFrame,
        captured: Option<(u64, bool)>,
        now: Instant,
    ) -> bool {
        let fits = self
            .listening
            .as_ref()
            .is_some_and(|listening| listening.share == frame.share && listening.sharer == sharer);
        if !fits && !self.listen(sharer, frame, now) {
            return false;
        }
        let took = self
            .listening
            .as_ref()
            .is_some_and(|listening| lock(&listening.ear).push(frame, captured));
        if !took {
            self.dropped += 1;
        }
        !fits
    }

    // A new ear for the share of `frame`, in place of any other: one plays
    // at a time. False when none was made.
    fn listen(&mut self, sharer: [u8; 32], frame: &SoundFrame, now: Instant) -> bool {
        self.retire(now);
        if self.off == Some(frame.share) && !frame.first {
            self.dropped += 1;
            return false;
        }
        // The oldest ear made and not taken by the speakers TAKEN_WITHIN on: the
        // render thread is away, as while Windows has no output to play on,
        // and an ear made now would wait in its orders with every one after
        // it until the speakers are back. Nothing would play it meanwhile.
        if let Some((ear, at)) = &self.made
            && now.saturating_duration_since(*at) >= TAKEN_WITHIN
            && ear.upgrade().is_some_and(|ear| !lock(&ear).taken())
        {
            self.dropped += 1;
            return false;
        }
        if !self.new_ears.take(now, NEW_EARS_PER_SECOND, NEW_EAR_BURST) {
            self.dropped += 1;
            return false;
        }
        let ear = match Ear::new() {
            Ok(ear) => Arc::new(Mutex::new(ear)),
            Err(err) => {
                log!(
                    self.log,
                    "pc sound: could not make a decoder for {}: {err}",
                    keys::fingerprint(&sharer)
                );
                return false;
            }
        };
        let gain = self.volumes.get(&sharer).copied().unwrap_or(1.0);
        let _ = self
            .speaker
            .send(ToSpeaker::AddStream(sharer, Arc::clone(&ear), gain));
        // The oldest ear the speakers have not taken stays the one timed, so
        // ears made faster than TAKEN_WITHIN cannot keep it young.
        let waiting = self
            .made
            .as_ref()
            .is_some_and(|(made, _)| made.upgrade().is_some_and(|made| !lock(&made).taken()));
        if !waiting {
            self.made = Some((Arc::downgrade(&ear), now));
        }
        self.off = None;
        self.listening = Some(Listening {
            share: frame.share,
            sharer,
            ear,
        });
        true
    }

    // The host says whether the sound of the share this PC watches goes
    // out, for the panel's words. Its frames do not wait for an on, which
    // can come after the first of them, except after an off: until the on,
    // only a frame that starts a spell makes a new ear. The first word since
    // the watch began, or a new session asked for it again, only sets the
    // word, since frames can come before it.
    pub(crate) fn said(&mut self, share: u32, on: bool, now: Instant) {
        let first = self.word(share).is_none();
        self.told = Some((share, on));
        if first {
            return;
        }
        if on {
            if self.off == Some(share) {
                self.off = None;
            }
            return;
        }
        if self
            .listening
            .as_ref()
            .is_some_and(|listening| listening.share == share)
        {
            self.retire(now);
        }
        // Whether or not the off let an ear go: frames captured before the
        // press can still come after it.
        self.off = Some(share);
    }

    // Under the state lock, as the ears are made, so the speakers get a
    // volume and a new ear in the order they happened. A sharer with no ear
    // now gets it with the next one.
    pub(crate) fn set_volume(&mut self, key: [u8; 32], percent: u8) {
        let percent = percent.min(MOST_VOLUME);
        let gain = f32::from(percent) / 100.0;
        if percent == 100 {
            self.volumes.remove(&key);
        } else {
            self.volumes.insert(key, gain);
        }
        let _ = self.speaker.send(ToSpeaker::StreamVolume(key, gain));
    }

    // The watch is over, or the share is, or a new session asks for it again.
    pub(crate) fn stop(&mut self, now: Instant) {
        self.retire(now);
        self.off = None;
        self.told = None;
    }

    // Its next 2.5 ms still play, fading out, and the last reference stays
    // here for a while, so the decoder goes on this thread and not the
    // render thread.
    fn retire(&mut self, now: Instant) {
        if let Some(listening) = self.listening.take() {
            let _ = self.speaker.send(ToSpeaker::RemoveStream(listening.sharer));
            self.retired.push((listening.ear, now + RETIRED_FOR));
        }
        self.pairing.clear();
        self.after = None;
        self.aged = None;
        self.next_line = None;
    }

    // On the timer thread at each of its screen rounds, not in fill, which
    // every view build runs: sets the pictures the viewer handed over beside
    // the sound frames played (pairing.rs), ages the spread, and every
    // LINE_EVERY tells the log how the sound goes.
    pub(crate) fn pair(&mut self, watching: &Watching, now: Instant) {
        let share = self.listening.as_ref().map(|listening| listening.share);
        watching.take_shown(share, &mut self.pictures);
        let (Some(share), Some(listening)) = (share, &self.listening) else {
            return;
        };
        // The timer thread wakes for more than the pictures. A round without
        // them pairs nothing, since that would end the wait of those carried
        // before their sound came, and works the spread out again at most
        // once AGE_EVERY, as often as pictures come.
        let pictures = !self.pictures.is_empty();
        if pictures {
            lock(&listening.ear).played_lately(&mut self.played);
            self.pairing.pair(&self.pictures, &mut self.played);
        }
        if pictures || self.aged.is_none_or(|aged| now >= aged + AGE_EVERY) {
            let now_us = self.clock.micros(now);
            self.pairing.prune(now_us);
            self.after = self.pairing.apart(now_us);
            self.aged = Some(now);
        }
        let due = *self.next_line.get_or_insert(now + LINE_EVERY);
        if now < due {
            return;
        }
        self.next_line = Some(now + LINE_EVERY);
        if let Some(numbers) = self.numbers(now) {
            log!(
                self.log,
                "watch {share}: pc sound: {}",
                sound_line(&numbers)
            );
        }
    }

    // What the host last said of the sound of `share` since this watch
    // began, or since a new session asked for it again: None until it has,
    // and off for a sharer that has said nothing.
    pub(crate) fn word(&self, share: u32) -> Option<bool> {
        self.told
            .filter(|&(told, _)| told == share)
            .map(|(_, on)| on)
    }

    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        self.retired.iter().map(|(_, until)| *until).min()
    }

    pub(crate) fn tick(&mut self, now: Instant) {
        self.retired.retain(|(_, until)| now < *until);
        stats::gauge!("room.sound.retired", self.retired);
        if stats::mem::ON
            && let Some(listening) = &self.listening
        {
            lock(&listening.ear).gauge();
        }
    }

    // The numbers the stats panel shows about the PC sound this PC hears.
    pub(crate) fn fill(&self, numbers: &mut Numbers, now: Instant) {
        numbers.sound_dropped = self.dropped;
        if let Some(sound) = self.numbers(now) {
            numbers.sound = Some(sound);
        }
    }

    fn numbers(&self, now: Instant) -> Option<SoundNumbers> {
        let listening = self.listening.as_ref()?;
        let now_us = self.clock.micros(now);
        // Made before the lock, so the render thread never waits on an
        // allocation, and sorted after it.
        let mut recent = Vec::with_capacity(hear::SAMPLES_KEPT);
        let (stats, about) = {
            let ear = lock(&listening.ear);
            (ear.stats(), ear.heard(now_us, &mut recent))
        };
        Some(SoundNumbers {
            capture_to_ear: share::spread(&mut recent).map(|(median_ms, p95_ms)| Latency {
                median_ms,
                p95_ms,
                about,
            }),
            depth_ms: stats.depth_ms,
            loss_pct: stats.loss_percent,
            made_up: stats.concealed,
            after_picture: self.after,
            muted: self
                .volumes
                .get(&listening.sharer)
                .is_some_and(|&gain| gain <= 0.0),
        })
    }
}

fn sound_line(numbers: &SoundNumbers) -> String {
    let ear = numbers.capture_to_ear.map_or_else(
        || String::from("not measured"),
        |latency| {
            format!(
                "median {:.2} ms, p95 {:.2} ms{}",
                latency.median_ms,
                latency.p95_ms,
                if latency.about { " (about)" } else { "" }
            )
        },
    );
    let after = numbers.after_picture.map_or_else(
        || String::from("not measured yet"),
        |after| {
            format!(
                "median {:.2} ms, p5 {:.2} ms, p95 {:.2} ms over {} pictures",
                after.median_ms, after.p5_ms, after.p95_ms, after.pictures
            )
        },
    );
    format!(
        "depth {} ms, capture to ear {ear}, after the picture {after}, loss {:.1}%, {} made up",
        numbers.depth_ms, numbers.loss_pct, numbers.made_up
    )
}

// One share's gate: made when Share sound goes on for it, never handed to
// another share, and swapped whole at each press. Only sound captured from
// `since` on goes out, and nothing of a packet the capture reads once `stop`
// is set: one read before the press holds only what came before it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Gate {
    share: u32,
    // The switch going on, or the share going on after a pause or a rest.
    since: Instant,
    // A pause by hand, the secure desktop, the switch going off, Stop
    // sharing, or the share resting with nobody watching.
    stop: Option<Instant>,
}

#[derive(Default)]
struct Gates {
    gate: Option<Gate>,
    // Paused by hand; away on the secure desktop or a disconnected session;
    // turned off at a press the room has not taken in yet.
    held: bool,
    away: bool,
    off: bool,
    // Stop sharing was pressed: nothing more of the share goes out, whatever
    // the switch does before the room takes the stop in.
    ended: bool,
    // The share rested with nobody watching, and its capture, or a window
    // share's look, has not answered since: nothing looked at the desktop
    // meanwhile, and it can be the secure one by now. The capture runs on,
    // as it does while nobody can hear.
    unseen: bool,
}

impl Gates {
    // The capture is let go.
    fn paused(&self) -> bool {
        self.held || self.away || self.off || self.ended
    }

    fn shut(&self) -> bool {
        self.paused() || self.unseen
    }

    // Shut at the first of them, open again from the press that ends the
    // last.
    fn follow(&mut self, press: Instant) {
        let shut = self.shut();
        if let Some(gate) = self.gate.as_mut() {
            match (gate.stop, shut) {
                (None, true) => gate.stop = Some(press),
                (Some(_), false) => {
                    gate.since = press;
                    gate.stop = None;
                }
                _ => {}
            }
        }
    }
}

// The links the capture sends on, each with its session's sealer and the
// address its media goes to.
#[derive(Default)]
struct Route {
    socket: Option<Arc<Socket>>,
    outlets: Vec<Outlet>,
}

// Where this PC's own PC sound goes, shared by the room and the capture
// thread: the gate the capture reads once a packet, the links it sends on,
// and its numbering. Leave takes the route back, socket and all, so the
// socket closes with the room while a capture still lets go on its own
// thread.
pub(crate) struct Outgoing {
    clock: Clock,
    // The host sends its own as RELAYED from its own slot: clients take no
    // other kind.
    host: bool,
    gates: Mutex<Gates>,
    route: Mutex<Arc<Route>>,
    // The share this PC shares now, 0 for none.
    live: AtomicU32,
    // Kept across captures and shares, so a listener never sees the
    // numbering go back within a room.
    next_seq: AtomicU16,
    // The capture time of the last frame sent, on the ping clock, 0 before
    // the first. It keeps every link pinging at the media rate.
    last_sent_us: AtomicU64,
    left: AtomicBool,
    // What each capture said, by the number the room gave it.
    events: Mutex<Vec<(u64, Event)>>,
    // Asks the timer thread to take what waits.
    work: OnceLock<Box<dyn Fn() + Send + Sync>>,
}

impl Outgoing {
    pub(crate) fn new(clock: Clock, host: bool, socket: Option<Arc<Socket>>) -> Outgoing {
        Outgoing {
            clock,
            host,
            gates: Mutex::default(),
            route: Mutex::new(Arc::new(Route {
                socket,
                outlets: Vec::new(),
            })),
            live: AtomicU32::new(0),
            next_seq: AtomicU16::new(crate::talk::random_seq()),
            last_sent_us: AtomicU64::new(0),
            left: AtomicBool::new(false),
            events: Mutex::default(),
            work: OnceLock::new(),
        }
    }

    pub(crate) fn on_work(&self, work: impl Fn() + Send + Sync + 'static) {
        let _ = self.work.set(Box::new(work));
    }

    fn wake_room(&self) {
        if let Some(work) = self.work.get() {
            work();
        }
    }

    // On the capture thread, once a packet: the gate, while its share is the
    // one this PC shares now.
    fn gate(&self) -> Option<Gate> {
        let gate = lock(&self.gates).gate?;
        (gate.share == self.live.load(Ordering::Acquire)).then_some(gate)
    }

    fn route(&self) -> Arc<Route> {
        Arc::clone(&lock(&self.route))
    }

    // Share sound went on for `share` at `press`. Paused, it opens at the
    // press that ends the pause.
    fn open(&self, share: u32, press: Instant) {
        if self.left.load(Ordering::Relaxed) {
            return;
        }
        let mut gates = lock(&self.gates);
        gates.off = false;
        gates.gate = Some(Gate {
            share,
            since: press,
            stop: gates.shut().then_some(press),
        });
    }

    fn close(&self) {
        let mut gates = lock(&self.gates);
        gates.gate = None;
        gates.off = false;
    }

    // Paused by hand, away, or turned off at a press not taken in yet.
    pub(crate) fn paused(&self) -> bool {
        lock(&self.gates).paused()
    }

    // Paused, or rested and not looked at since.
    #[cfg(test)]
    pub(crate) fn shut(&self) -> bool {
        lock(&self.gates).shut()
    }

    // Share sound turned off at `press`, before the room takes the press in.
    pub(crate) fn stop(&self, press: Instant) {
        let mut gates = lock(&self.gates);
        gates.off = true;
        gates.follow(press);
    }

    // Stop sharing pressed at `press`, before the room takes it in.
    pub(crate) fn end_share(&self, press: Instant) {
        let mut gates = lock(&self.gates);
        gates.ended = true;
        gates.follow(press);
    }

    // Room::pause_share, at the press, on whichever thread pressed.
    pub(crate) fn set_held(&self, held: bool, press: Instant) {
        let gated = {
            let mut gates = lock(&self.gates);
            gates.held = held;
            gates.follow(press);
            gates.gate.is_some()
        };
        // The room lets the capture go, or opens it again.
        if gated {
            self.wake_room();
        }
    }

    // Share `share`'s capture, or a window share's own look at the desktop,
    // found the secure desktop or a disconnected session, or it is back from
    // it: the PC may be left alone then, and what it plays is not the
    // sharer's to send. Named, as set_unseen is: a share that ended can
    // still be reporting.
    pub(crate) fn set_away(&self, share: u32, away: bool, at: Instant) {
        let gated = {
            let mut gates = lock(&self.gates);
            if self.live.load(Ordering::Acquire) != share {
                return;
            }
            gates.away = away;
            gates.follow(at);
            gates.gate.is_some()
        };
        if gated {
            self.wake_room();
        }
    }

    // Share `share` rested with nobody watching, or its capture answered
    // after the rest, unpaused, or for a window its look at the desktop
    // found it the sharer's. Nothing looks at the desktop while it rests, so
    // a friend who starts watching while the PC is on the lock screen hears
    // nothing before the share has looked and reported it. Named, since the
    // share's thread can still be on a share the room has ended.
    pub(crate) fn set_unseen(&self, share: u32, unseen: bool, at: Instant) {
        let mut gates = lock(&self.gates);
        if self.live.load(Ordering::Acquire) == share {
            gates.unseen = unseen;
            gates.follow(at);
        }
    }

    // A new share, or none: no gate outlives its share.
    pub(crate) fn share_changed(&self, share: Option<u32>) {
        let mut gates = lock(&self.gates);
        self.live.store(share.unwrap_or(0), Ordering::Release);
        *gates = Gates::default();
    }

    // Leave, before anything else: nothing goes out from here on.
    pub(crate) fn let_go(&self) {
        self.left.store(true, Ordering::Relaxed);
        lock(&self.gates).gate = None;
        *lock(&self.route) = Arc::default();
    }

    // On the capture thread.
    fn event(&self, capture: u64, event: Event) {
        lock(&self.events).push((capture, event));
        self.wake_room();
    }

    fn take_events(&self) -> Vec<(u64, Event)> {
        std::mem::take(&mut *lock(&self.events))
    }

    // A frame captured at `captured_us` went out. The first after a quiet
    // spell brings the pings to the media rate at once, as video's does.
    fn sent(&self, captured_us: u64) {
        let before = self.last_sent_us.swap(captured_us, Ordering::Relaxed);
        if before == 0 || captured_us.saturating_sub(before) >= MEDIA_FLOWS_FOR.as_micros() as u64 {
            self.wake_room();
        }
    }

    pub(crate) fn sent_lately(&self, now: Instant) -> bool {
        let last = self.last_sent_us.load(Ordering::Relaxed);
        last != 0
            && self.clock.micros(now).saturating_sub(last) < MEDIA_FLOWS_FOR.as_micros() as u64
    }
}

// A capture the room runs. Dropped, it is let go and closes on its own
// thread: PcSound's own drop would wait for that.
struct Running {
    sound: Option<PcSound>,
    feed: Arc<Mutex<Feed>>,
    number: u64,
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(sound) = self.sound.take() {
            sound.let_go();
        }
    }
}

// What the room says of a step: whether the watchers hear the sound is on,
// and a problem line.
#[derive(Default)]
pub(crate) struct Said {
    pub tell: Option<bool>,
    pub problem: Option<String>,
}

// Share sound on this PC's own share, under the state lock: the switch, the
// capture it runs, and what the watchers were told.
pub(crate) struct Switch {
    out: Arc<Outgoing>,
    // Windows' process loopback, or this fake's, so no test captures this PC.
    fake: Option<Fake>,
    log: Log,
    // The share it is on for; off at every new one.
    on: Option<u32>,
    capture: Option<Running>,
    captures: u64,
    // A capture opened since the switch went on, so one that fails after it
    // stopped rather than could not start.
    opened: bool,
    // What step tells the watchers while the switch is on, that the sound
    // goes out: the capture opened and was not lost since, or the switch
    // went on during a pause, which opens nothing until the share goes on
    // again.
    flowing: bool,
    // Why the capture is lost, in its own words, while it waits to open
    // again: the line under the switch, and the log's once an outage.
    lost: Option<String>,
    // The watchers were told it is on.
    told: bool,
    // Which sessions and addresses the capture's links have now.
    published: Vec<(u32, SocketAddr)>,
}

impl Switch {
    pub(crate) fn new(out: Arc<Outgoing>, fake: Option<Fake>, log: Log) -> Switch {
        Switch {
            out,
            fake,
            log,
            on: None,
            capture: None,
            captures: 0,
            opened: false,
            flowing: false,
            lost: None,
            told: false,
            published: Vec::new(),
        }
    }

    // On for `live`, the share this PC shares now.
    pub(crate) fn is_on(&self, live: Option<u32>) -> bool {
        self.on.is_some() && self.on == live
    }

    // Why its capture is lost, while it is on for `live` and waits to open
    // again.
    pub(crate) fn lost(&self, live: Option<u32>) -> Option<String> {
        self.lost.clone().filter(|_| self.is_on(live))
    }

    // The panel's switch for `share`, pressed at `press`. False when it was
    // already so.
    pub(crate) fn turn(&mut self, on: bool, share: u32, press: Instant) -> bool {
        if on == (self.on == Some(share)) {
            return false;
        }
        self.end_capture();
        if on {
            self.on = Some(share);
            self.opened = false;
            self.out.open(share, press);
        } else {
            self.on = None;
            self.out.close();
        }
        // Turned on during a pause, it says it goes out, as one whose
        // capture opened before the pause keeps saying so through it: the
        // pause holds it back, and its watchers hear the share is paused. If
        // the capture cannot start when the share goes on again, they hear
        // it is off then.
        self.flowing = on && self.out.paused();
        self.lost = None;
        log!(
            self.log,
            "share {share}: share sound {}",
            if on { "on" } else { "off" }
        );
        true
    }

    // After every step of the room's: the share it was on for is over, and
    // its capture goes at once. Its watchers drop its sound with it, so
    // nothing is said.
    pub(crate) fn follow(&mut self, live: Option<u32>) {
        if self.on.is_some() && self.on != live {
            self.end_capture();
            self.out.close();
            self.on = None;
            self.flowing = false;
            self.lost = None;
            self.told = false;
        }
    }

    // What the capture said, and the pause: the capture runs while the
    // switch is on and the share is not paused.
    pub(crate) fn step(&mut self, live: Option<u32>) -> Said {
        self.follow(live);
        let mut said = Said::default();
        let events = self.out.take_events();
        if let Some(share) = self.on {
            self.take_in(share, events, &mut said);
        }
        let flowing = self.on.is_some() && self.flowing;
        if flowing != self.told {
            self.told = flowing;
            said.tell = Some(flowing);
        }
        said
    }

    fn take_in(&mut self, share: u32, events: Vec<(u64, Event)>, said: &mut Said) {
        let current = self.capture.as_ref().map(|running| running.number);
        for (number, event) in events {
            if current != Some(number) || self.on.is_none() {
                continue;
            }
            match event {
                Event::Opened(info) => {
                    let period = if info.period.is_zero() {
                        String::from("not given")
                    } else {
                        format!("{:.2} ms", info.period_ms())
                    };
                    log!(
                        self.log,
                        "share {share}: pc sound opened in {:.1} ms, period {period}, stream latency {:.2} ms",
                        info.opened_in.as_secs_f64() * 1000.0,
                        info.stream_latency.as_secs_f64() * 1000.0
                    );
                    self.opened = true;
                    self.flowing = true;
                    self.lost = None;
                }
                // The capture tries again every second, and each try that
                // fails says so again: the log has the first of an outage.
                Event::Lost(err) | Event::Gone(err) => {
                    if self.lost.is_none() {
                        log!(self.log, "share {share}: {err}");
                    }
                    self.lost = Some(err.to_string());
                    self.flowing = false;
                }
                Event::Failed(err) => {
                    let why = clause(&err);
                    said.problem = Some(if self.opened {
                        log!(self.log, "share {share}: pc sound stopped: {why}");
                        stopped(&why)
                    } else {
                        could_not_start(share, &why, &self.log)
                    });
                    self.end_capture();
                    self.out.close();
                    self.on = None;
                    self.flowing = false;
                    self.lost = None;
                }
            }
        }
        if self.on.is_none() {
            return;
        }
        let wanted = !self.out.paused();
        if wanted && self.capture.is_none() {
            if let Err(why) = self.start(share) {
                said.problem = Some(could_not_start(share, &why, &self.log));
                self.out.close();
                self.on = None;
            }
        } else if !wanted && self.capture.is_some() {
            log!(
                self.log,
                "share {share}: pc sound let go, nothing more is read until the share goes on"
            );
            self.end_capture();
            self.lost = None;
        }
    }

    // Hands the capture a new list of links when the sessions or addresses
    // changed. The list is its own, the links that read sound, so it has its
    // own key, kept as the video's is.
    pub(crate) fn publish(
        &mut self,
        links: impl Iterator<Item = (u32, SocketAddr)> + Clone,
        outlets: impl FnOnce() -> Vec<Outlet>,
    ) {
        if links.clone().eq(self.published.iter().copied()) {
            return;
        }
        self.published = links.collect();
        let outlets = outlets();
        // Read and swapped under one lock, so a route Leave took back never
        // gets its socket again.
        let mut route = lock(&self.out.route);
        let socket = route.socket.clone();
        *route = Arc::new(Route { socket, outlets });
    }

    // Made here, off the capture thread, since the encoder allocates.
    fn start(&mut self, share: u32) -> Result<(), String> {
        let feed = Feed::new(Arc::clone(&self.out)).map_err(|err| err.to_string())?;
        let feed = Arc::new(Mutex::new(feed));
        self.captures += 1;
        let number = self.captures;
        let audio = {
            let feed = Arc::clone(&feed);
            move |samples: &[f32], captured: Instant, after_gap: bool| {
                lock(&feed).hear(samples, captured, after_gap);
            }
        };
        let events = {
            let out = Arc::clone(&self.out);
            move |event: Event| out.event(number, event)
        };
        let sound = match &self.fake {
            None => PcSound::start(audio, events),
            Some(fake) => PcSound::start_with(fake.device(), audio, events),
        }
        .map_err(|err| clause(&err))?;
        log!(self.log, "share {share}: pc sound opening");
        self.capture = Some(Running {
            sound: Some(sound),
            feed,
            number,
        });
        Ok(())
    }

    // The spell under way ends before the capture is let go: faded with
    // LAST while the share goes on, dropped with a share that is over.
    fn end_capture(&mut self) {
        if let Some(running) = self.capture.take() {
            lock(&running.feed).end();
        }
    }
}

// Access denied says itself to turn Share PC sound on again once access is
// allowed, so the line does not say it twice.
fn stopped(why: &str) -> String {
    if why.ends_with("turn Share PC sound on again") {
        format!("PC sound stopped: {why}.")
    } else {
        format!("PC sound stopped: {why}. Turn Share PC sound on to try again.")
    }
}

fn could_not_start(share: u32, why: &str, log: &Log) -> String {
    log!(log, "share {share}: pc sound could not start: {why}");
    format!("PC sound could not start: {why}. The share goes on without it.")
}

// The capture's error as a clause for the share's own lines: its words
// without the start and the advice of its own sentence, which the lines put
// their own way.
fn clause(err: &AudioError) -> String {
    let said = err.to_string();
    let said = said
        .strip_prefix("could not start PC sound: ")
        .unwrap_or(&said);
    said.strip_suffix(". Try again, and restart Windows if it keeps happening")
        .unwrap_or(said)
        .to_owned()
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Access denied already says to turn Share PC sound on again, so its
    // stopped line says it once; any other says it after the reason.
    #[test]
    fn a_stopped_line_says_what_to_do_once() {
        let denied = AudioError::PcSound {
            code: 0x8007_0005,
            text: String::new(),
            lost: false,
        };
        assert_eq!(
            stopped(&clause(&denied)),
            "PC sound stopped: Windows says access is denied (0x80070005). Allow microphone access for desktop apps in Settings, Privacy and security, Microphone, then turn Share PC sound on again."
        );
        let format = AudioError::PcSound {
            code: 0x8889_0008,
            text: String::from("Windows would not give its sound as 48 kHz stereo"),
            lost: false,
        };
        assert_eq!(
            stopped(&clause(&format)),
            "PC sound stopped: Windows would not give its sound as 48 kHz stereo (0x88890008). Turn Share PC sound on to try again."
        );
    }
}
