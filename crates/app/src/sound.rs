// The panel's side of audio. While the settings screen shows: both device
// lists, kept current, the chosen microphone open for the level meter, and
// its rate and connection asked of Windows for the warning under the list.
// In a room the room opens the devices itself; this only asks them for their
// period each time the stats panel opens, for a side whose stream is closed,
// as a muted microphone's is. Nothing here opens a device at any other time.

use std::mem;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;

use eframe::egui::Context;
use voice::audio::{
    self, AudioError, Capture, Choice, Direction, Event, Lists, Microphone, Probe, Reading, Watch,
};

use crate::messages;

pub struct SettingsAudio {
    watch: Result<Watch, String>,
    mic: Mic,
    check: Check,
    // As the watch last read them, taken once a frame by follow.
    lists: Option<Result<Lists, String>>,
}

impl SettingsAudio {
    pub fn open(ctx: &Context, input: &Choice) -> SettingsAudio {
        let repaint = ctx.clone();
        let watch = Watch::start(move || repaint.request_repaint())
            .map_err(|err| messages::sentence(&err.to_string()));
        // The meter reads the level the capture thread keeps; the samples
        // themselves go nowhere.
        let open: Open =
            Arc::new(|choice, events| Capture::start(choice, |_: &[f32], _| {}, events));
        // A device that will not answer gets no warning; the meter's own
        // line says what is wrong with it.
        let ask: Ask = Arc::new(|choice| {
            let probe = audio::probe(Direction::Input, choice).ok()?;
            Some(Microphone::new(&probe.engine, probe.hands_free))
        });
        SettingsAudio {
            watch,
            mic: Mic::new(ctx, open, input),
            check: Check::new(ctx, ask, input),
            lists: None,
        }
    }

    // Once a frame while the screen is up. The meter follows the device
    // picked on the screen and is closed while the window is minimized, and
    // a stream that ended starts again when the device lists change, which
    // is when its device may be back.
    pub fn follow(&mut self, input: &Choice, showing: bool) {
        let opened = self.mic.follow(input, showing);
        self.check.follow(input, opened);
        let lists = match &self.watch {
            Ok(watch) => watch
                .lists()
                .map(|lists| lists.map_err(|err| messages::sentence(&err.to_string()))),
            Err(problem) => Some(Err(problem.clone())),
        };
        if lists != self.lists {
            let changed = matches!(self.lists, Some(Ok(_)));
            self.lists = lists;
            if changed {
                self.mic.retry();
                self.check.ask(false);
            }
        }
    }

    // The chosen microphone as Windows describes it, once it has answered.
    pub fn microphone(&self) -> Option<Microphone> {
        self.check.microphone()
    }

    pub fn lists(&self) -> Option<&Result<Lists, String>> {
        self.lists.as_ref()
    }

    // Why the meter's microphone is not running, until it runs.
    pub fn problem(&self) -> Option<&str> {
        self.mic.problem.as_deref()
    }

    // None while no stream runs.
    pub fn level(&self) -> Option<Reading> {
        self.mic.level()
    }
}

type Events = Box<dyn FnMut(Event) + Send>;
type Open = Arc<dyn Fn(Choice, Events) -> Result<Capture, AudioError> + Send + Sync>;

enum Stream {
    Off,
    // Handed over by the thread that starts it.
    Starting(Receiver<Result<Capture, AudioError>>),
    On(Capture),
}

impl Stream {
    // Returns once the stream's thread has ended.
    fn close(self) {
        match self {
            Stream::Off => {}
            Stream::Starting(started) => {
                if let Ok(Ok(capture)) = started.recv() {
                    capture.stop();
                }
            }
            Stream::On(capture) => capture.stop(),
        }
    }
}

// The microphone behind the meter. Stopping a stream waits for its thread,
// which may be opening a Bluetooth headset, and that can take a second while
// the headset changes profile. So streams are stopped and started on a
// thread of their own, never the panel's, and each of those threads closes
// the stream before it first, so there is never more than one.
struct Mic {
    ctx: Context,
    open: Open,
    choice: Choice,
    stream: Stream,
    // The window is minimized, and the microphone closed until it shows.
    hidden: bool,
    // Counts every start and stop. Events carry the count of the start they
    // came from, so whatever a replaced stream says is dropped, and a start
    // that was replaced before its thread got to it opens nothing.
    turn: Arc<AtomicU64>,
    events: Receiver<(u64, Event)>,
    events_in: Sender<(u64, Event)>,
    problem: Option<String>,
    // The stream ended and will not come back by itself.
    ended: bool,
}

impl Mic {
    fn new(ctx: &Context, open: Open, choice: &Choice) -> Mic {
        let (events_in, events) = mpsc::channel();
        let mut mic = Mic {
            ctx: ctx.clone(),
            open,
            choice: choice.clone(),
            stream: Stream::Off,
            hidden: false,
            turn: Arc::default(),
            events,
            events_in,
            problem: None,
            ended: false,
        };
        mic.replace(true);
        mic
    }

    // True when the stream opened since the last frame.
    fn follow(&mut self, choice: &Choice, showing: bool) -> bool {
        if !showing {
            if !self.hidden {
                self.hidden = true;
                self.replace(false);
            }
            return false;
        }
        if self.hidden || *choice != self.choice {
            self.hidden = false;
            self.choice = choice.clone();
            self.replace(true);
            return false;
        }
        self.take_news()
    }

    // The device lists changed, so a device that went away may be back.
    fn retry(&mut self) {
        if self.ended && !self.hidden {
            self.replace(true);
        }
    }

    fn replace(&mut self, start: bool) {
        let turn = self.turn.fetch_add(1, Ordering::Relaxed) + 1;
        self.problem = None;
        self.ended = false;
        let before = mem::replace(&mut self.stream, Stream::Off);
        let (started_in, started) = mpsc::channel();
        let latest = Arc::clone(&self.turn);
        let open = Arc::clone(&self.open);
        let choice = self.choice.clone();
        let events_in = self.events_in.clone();
        let repaint = self.ctx.clone();
        let spawned = thread::Builder::new()
            .name(String::from("microphone meter"))
            .spawn(move || {
                before.close();
                if !start || latest.load(Ordering::Relaxed) != turn {
                    return;
                }
                let on_event = repaint.clone();
                let events: Events = Box::new(move |event| {
                    let _ = events_in.send((turn, event));
                    on_event.request_repaint();
                });
                let _ = started_in.send(open(choice, events));
                repaint.request_repaint();
            });
        match spawned {
            Ok(_) if start => self.stream = Stream::Starting(started),
            Ok(_) => {}
            // `before` went down with the thread's closure, and was stopped
            // here as it dropped.
            Err(err) if start => {
                let err = AudioError::Thread(err.to_string());
                self.problem = Some(messages::sentence(&err.to_string()));
                self.ended = true;
            }
            Err(_) => {}
        }
    }

    fn take_news(&mut self) -> bool {
        let mut opened = false;
        if let Stream::Starting(started) = &self.stream {
            match started.try_recv() {
                Ok(Ok(capture)) => self.stream = Stream::On(capture),
                Ok(Err(err)) => {
                    self.stream = Stream::Off;
                    self.problem = Some(messages::sentence(&err.to_string()));
                    self.ended = true;
                }
                Err(TryRecvError::Empty) => {}
                // Only a panic on that thread ends it without an answer.
                Err(TryRecvError::Disconnected) => {
                    self.stream = Stream::Off;
                    self.ended = true;
                }
            }
        }
        let turn = self.turn.load(Ordering::Relaxed);
        while let Ok((from, event)) = self.events.try_recv() {
            if from != turn {
                continue;
            }
            match event {
                Event::Opened(_) => {
                    self.problem = None;
                    self.ended = false;
                    opened = true;
                }
                Event::Lost(err) => self.problem = Some(messages::sentence(&err.to_string())),
                Event::Gone(err) => {
                    self.problem = Some(messages::sentence(&err.to_string()));
                    self.ended = true;
                }
                Event::Failed(err) => {
                    // With no microphone at all under Windows default the
                    // stream waits for one by itself.
                    let waits =
                        self.choice == Choice::Default && matches!(err, AudioError::NoDevice(_));
                    self.ended = !waits;
                    self.problem = Some(messages::sentence(&err.to_string()));
                }
            }
        }
        opened
    }

    fn level(&self) -> Option<Reading> {
        let Stream::On(capture) = &self.stream else {
            return None;
        };
        capture.info()?;
        Some(capture.level())
    }
}

impl Drop for Mic {
    fn drop(&mut self) {
        self.replace(false);
    }
}

type Ask = Arc<dyn Fn(&Choice) -> Option<Microphone> + Send + Sync>;

// The chosen microphone as Windows describes it, for the warning under the
// input list. Asking opens no stream, but a device can take a moment to
// answer, so each question runs on a thread of its own. Asked when the
// choice changes, when the device lists change, and when the meter's stream
// opens, since what a Bluetooth headset reports can change once its
// microphone is open, and the open one is what a call gets.
struct Check {
    ctx: Context,
    ask: Ask,
    choice: Choice,
    answer: Arc<Mutex<Answer>>,
}

// Only the answer to the latest question is kept.
#[derive(Default)]
struct Answer {
    turn: u64,
    microphone: Option<Microphone>,
}

impl Check {
    fn new(ctx: &Context, ask: Ask, choice: &Choice) -> Check {
        let mut check = Check {
            ctx: ctx.clone(),
            ask,
            choice: choice.clone(),
            answer: Arc::default(),
        };
        check.ask(true);
        check
    }

    fn follow(&mut self, choice: &Choice, opened: bool) {
        if *choice != self.choice {
            self.choice = choice.clone();
            self.ask(true);
        } else if opened {
            self.ask(false);
        }
    }

    // `forget` for a new device, whose warning must not be the last one's.
    // Otherwise the last answer stays up until the new one is in, so the
    // warning does not blink.
    fn ask(&mut self, forget: bool) {
        let turn = {
            let mut answer = lock(&self.answer);
            if forget {
                answer.microphone = None;
            }
            answer.turn += 1;
            answer.turn
        };
        let answer = Arc::clone(&self.answer);
        let ask = Arc::clone(&self.ask);
        let choice = self.choice.clone();
        let repaint = self.ctx.clone();
        // A thread that cannot start leaves no warning, and the next change
        // asks again.
        let _ = thread::Builder::new()
            .name(String::from("microphone check"))
            .spawn(move || {
                let microphone = ask(&choice);
                let mut answer = lock(&answer);
                if answer.turn == turn {
                    answer.microphone = microphone;
                    repaint.request_repaint();
                }
            });
    }

    fn microphone(&self) -> Option<Microphone> {
        lock(&self.answer).microphone
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

// Where each side's number comes from.
#[derive(Clone, Debug, PartialEq)]
pub enum Side {
    Period { ms: f64, resampled: bool },
    NoDevice,
    // The device would not say.
    NotKnown,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Periods {
    pub input: Side,
    pub output: Side,
}

fn side(probe: Result<Probe, AudioError>) -> Side {
    match probe {
        Ok(probe) => Side::Period {
            ms: probe.period.as_secs_f64() * 1000.0,
            resampled: probe.resampled,
        },
        Err(AudioError::NoDevice(_) | AudioError::NotConnected { .. }) => Side::NoDevice,
        Err(_) => Side::NotKnown,
    }
}

// Asked on its own thread, since the devices can take a moment to answer
// and the panel should not wait on them. Asked again each time the stats
// panel opens, since a headset may have connected or become the default in
// between; the last answer stays up until the new one is in.
#[derive(Default)]
pub struct PeriodsAsk {
    asked: bool,
    answer: Arc<Mutex<Option<Periods>>>,
}

impl PeriodsAsk {
    pub fn ask(&mut self, ctx: &Context, devices: &[Choice; 2]) {
        if self.asked {
            return;
        }
        self.asked = true;
        let [input, output] = devices.clone();
        let answer = Arc::clone(&self.answer);
        let ctx = ctx.clone();
        // A thread that cannot start leaves the last answer, or none, and
        // the next opening of the stats panel tries again.
        let _ = thread::Builder::new()
            .name(String::from("audio periods"))
            .spawn(move || {
                let periods = Periods {
                    input: side(audio::probe(Direction::Input, &input)),
                    output: side(audio::probe(Direction::Output, &output)),
                };
                *answer.lock().unwrap_or_else(PoisonError::into_inner) = Some(periods);
                ctx.request_repaint();
            });
    }

    // The stats panel closed; the next time it opens, the devices are asked
    // again.
    pub fn closed(&mut self) {
        self.asked = false;
    }

    pub fn periods(&self) -> Option<Periods> {
        self.answer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use voice::audio::fake::{Fake, Setup};

    use super::*;

    fn fake_open(fake: &Fake) -> Open {
        let fake = fake.clone();
        Arc::new(move |choice, events| {
            Capture::start_with(fake.device(), choice, |_: &[f32], _| {}, events)
        })
    }

    // Frames as the panel draws them, every 5 ms, until `done`.
    fn frames_until(mic: &mut Mic, choice: &Choice, done: impl Fn(&Mic) -> bool) {
        let until = Instant::now() + Duration::from_secs(3);
        while !done(mic) {
            assert!(Instant::now() < until, "not within 3 s");
            thread::sleep(Duration::from_millis(5));
            mic.follow(choice, true);
        }
    }

    fn wait_until(what: &str, done: impl Fn() -> bool) {
        let until = Instant::now() + Duration::from_secs(1);
        while !done() {
            assert!(Instant::now() < until, "{what}: not within 1 s");
            thread::sleep(Duration::from_millis(2));
        }
    }

    // Moving through the input list while a Bluetooth headset takes its time
    // to open: no frame waits for it, a device passed over is never opened,
    // and there are never two streams at once.
    #[test]
    fn slow_open_never_holds_up_the_panel() {
        let fake = Fake::new(
            Setup {
                open_time: Duration::from_millis(400),
                ..Setup::default()
            },
            &[("a", "Headset A"), ("b", "Headset B"), ("c", "Headset C")],
            Some("a"),
        );
        let mut mic = Mic::new(&Context::default(), fake_open(&fake), &Choice::Default);
        wait_until("a is opening", || fake.record().at_once == 1);
        let mut slowest = Duration::ZERO;
        for id in ["b", "c"] {
            thread::sleep(Duration::from_millis(20));
            let frame = Instant::now();
            mic.follow(&Choice::Device(String::from(id)), true);
            slowest = slowest.max(frame.elapsed());
        }
        let c = Choice::Device(String::from("c"));
        frames_until(&mut mic, &c, |mic| mic.level().is_some());
        println!("slowest frame {:.2} ms", slowest.as_secs_f64() * 1000.0);
        assert!(slowest < Duration::from_millis(50), "{slowest:?}");
        let record = fake.record();
        let opened: Vec<&str> = record.opens.iter().map(|(_, id)| id.as_str()).collect();
        assert_eq!(opened, ["a", "c"]);
        assert_eq!(record.starts, 1);
        assert_eq!(record.most_at_once, 1);
    }

    // The window is minimized: it shows no meter, so the microphone closes,
    // and opens again when the window comes back.
    #[test]
    fn the_microphone_closes_while_the_window_is_minimized() {
        let fake = Fake::new(Setup::default(), &[("a", "Headset A")], Some("a"));
        let mut mic = Mic::new(&Context::default(), fake_open(&fake), &Choice::Default);
        frames_until(&mut mic, &Choice::Default, |mic| mic.level().is_some());
        mic.follow(&Choice::Default, false);
        assert!(mic.level().is_none());
        wait_until("the stream closes", || fake.record().at_once == 0);
        thread::sleep(Duration::from_millis(50));
        mic.follow(&Choice::Default, false);
        assert_eq!(fake.record().opens.len(), 1);
        frames_until(&mut mic, &Choice::Default, |mic| mic.level().is_some());
        assert_eq!(fake.record().opens.len(), 2);
        drop(mic);
        wait_until("the stream closes after the screen", || {
            fake.record().at_once == 0
        });
    }

    // A device that is not connected fails a moment after it was passed
    // over. That is about the old choice, and nothing is said under the new
    // one while it opens.
    #[test]
    fn a_replaced_stream_says_nothing_about_the_new_choice() {
        let fake = Fake::new(
            Setup {
                open_time: Duration::from_millis(200),
                ..Setup::default()
            },
            &[("b", "Headset B")],
            Some("b"),
        );
        let a = Choice::Device(String::from("a"));
        let b = Choice::Device(String::from("b"));
        let mut mic = Mic::new(&Context::default(), fake_open(&fake), &a);
        wait_until("a is opening", || fake.record().at_once == 1);
        mic.follow(&b, true);
        let mut said = None;
        let until = Instant::now() + Duration::from_secs(3);
        while mic.level().is_none() {
            assert!(Instant::now() < until, "b did not open within 3 s");
            thread::sleep(Duration::from_millis(5));
            mic.follow(&b, true);
            said = said.or(mic.problem.clone());
        }
        assert_eq!(said, None);
        assert!(!mic.ended);
    }

    // A slow answer about a device that was passed over never shows under the
    // one chosen after it, and a new choice takes the old warning away at
    // once. The meter opening asks again without taking it away.
    #[test]
    fn the_warning_is_about_the_device_chosen_now() {
        const AIRPODS: Microphone = Microphone {
            rate: 8_000,
            hands_free: true,
        };
        const USB: Microphone = Microphone {
            rate: 48_000,
            hands_free: false,
        };
        let ask: Ask = Arc::new(|choice: &Choice| match choice.id() {
            Some("airpods") => Some(AIRPODS),
            Some("slow") => {
                thread::sleep(Duration::from_millis(300));
                Some(AIRPODS)
            }
            Some("usb") => Some(USB),
            _ => None,
        });
        let device = |id: &str| Choice::Device(String::from(id));
        let mut check = Check::new(&Context::default(), ask, &device("airpods"));
        wait_until("the airpods answer", || check.microphone() == Some(AIRPODS));

        check.follow(&device("usb"), false);
        assert_ne!(check.microphone(), Some(AIRPODS));
        wait_until("the usb answer", || check.microphone() == Some(USB));

        check.follow(&device("slow"), false);
        check.follow(&device("usb"), false);
        wait_until("the usb answer again", || check.microphone() == Some(USB));
        thread::sleep(Duration::from_millis(400));
        assert_eq!(check.microphone(), Some(USB));

        check.follow(&device("usb"), true);
        assert_eq!(check.microphone(), Some(USB));
    }
}
