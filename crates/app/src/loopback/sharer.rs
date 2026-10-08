// The sharer's thread: share::Sharer on the loopback's link, with the loss
// knob in the pacer's send function, where the socket would be, and with a
// network between the two sides the rate's backoff once a second as the
// room runs it (share::rate). With none, the rate stays at --bitrate, so a
// run with --loss measures the parity at the rate asked for.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use share::rate::{Rate, Second, Step};
use share::{Codec, FineTimer, Knob, Line, Sharer, SharerNumbers, spread};

use super::link::{Link, SharerEnd};
use super::network::Network;

#[derive(Clone)]
pub struct Setup {
    pub share: share::Setup,
    pub loss_percent: f64,
    pub seed: u64,
    // The viewer counts as over the internet, as for the step down to
    // 1080p60: the run has a network between the two sides.
    pub internet: bool,
}

// What the viewer's thread needs to know before it opens its window.
pub struct Started {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub encoder: String,
}

// How the share went, for the summary.
#[derive(Debug, Default)]
pub struct Shared {
    pub numbers: SharerNumbers,
    // What the encoder in use at the end made.
    pub codec: Option<Codec>,
    pub rate_kbps: u32,
    pub allowed_kbps: u32,
    pub backoffs: u32,
    pub steps_down: u32,
    pub steps_up: u32,
}

pub fn run(
    setup: &Setup,
    link: &Arc<Link>,
    network: &Arc<Network>,
    lines: &Sender<Line>,
    started: &Sender<Result<Started, String>>,
    viewer_ready: &Receiver<()>,
) -> Result<Shared, String> {
    // The 1 ms timer resolution a share in a room runs with, so the
    // loopback's numbers are taken the same way. It is the whole process's;
    // the viewer's thread waits on the high-resolution timer either way.
    // Given back when this returns, whichever way.
    let _timer = match FineTimer::raise() {
        Ok(timer) => Some(timer),
        Err(why) => {
            let _ = lines.send(Line::Say(why));
            None
        }
    };
    let send = {
        let (link, network) = (Arc::clone(link), Arc::clone(network));
        let mut knob = Knob::new(setup.loss_percent, setup.seed);
        move |packet: &[u8]| {
            if knob.drops() {
                link.knob_dropped.fetch_add(1, Ordering::Relaxed);
            } else {
                network.video(packet);
            }
        }
    };
    // What the sharer says while it opens goes before `started`, so it is
    // said before the line about what was opened.
    let mut say = |line| {
        let _ = lines.send(line);
    };
    let mut sharer = match Sharer::open(setup.share.clone(), send, &mut say) {
        Ok(sharer) => sharer,
        Err(err) => {
            let _ = started.send(Err(err.clone()));
            return Err(err);
        }
    };
    let (width, height) = sharer.size();
    let _ = started.send(Ok(Started {
        width,
        height,
        fps: sharer.fps(),
        encoder: sharer.encoder_name().to_string(),
    }));
    let allowed_kbps = setup.share.settings.bitrate / 1000;
    // Frames sent while the viewer's window and FFmpeg are still loading
    // would pile up in the link and come out in a burst, as no room ever
    // delivers them. A viewer that failed to open says why itself.
    if viewer_ready.recv().is_err() {
        return Ok(Shared {
            rate_kbps: allowed_kbps,
            allowed_kbps,
            ..Shared::default()
        });
    }
    let mut end = SharerEnd::new(link, lines);
    let mut rating = (!network.idle()).then(|| Rating::new(allowed_kbps, setup.internet));
    let mut fps = sharer.fps();
    while !link.stopped() {
        sharer.next(&mut end)?;
        if let Some(rating) = rating.as_mut() {
            rating.second(&mut sharer, &mut end, network)?;
        }
        // The viewer's reassembler waits one interval of the rate the share
        // runs at, as the room's roster tells a watcher: a step changes it,
        // and so does a GPU encoder failing at 1440p120, which leaves the
        // share at the software encoder's 60.
        if sharer.fps() != fps {
            fps = sharer.fps();
            end.link.inbox.set_fps(fps);
        }
    }
    let codec = sharer.codec();
    let numbers = sharer.finish();
    Ok(match rating {
        Some(rating) => Shared {
            numbers,
            codec,
            rate_kbps: rating.rate.rate_kbps(),
            allowed_kbps,
            backoffs: rating.rate.backoffs(),
            steps_down: rating.steps_down,
            steps_up: rating.steps_up,
        },
        None => Shared {
            numbers,
            codec,
            rate_kbps: allowed_kbps,
            allowed_kbps,
            ..Shared::default()
        },
    })
}

// The room's share thread's second (room::screen::sharer), with the
// loopback's network for the round trip.
struct Rating {
    rate: Rate,
    internet: bool,
    next: Instant,
    seconds: u64,
    lost_here_before: u64,
    steps_down: u32,
    steps_up: u32,
}

impl Rating {
    fn new(allowed_kbps: u32, internet: bool) -> Rating {
        Rating {
            rate: Rate::new(allowed_kbps),
            internet,
            next: Instant::now() + Duration::from_secs(1),
            seconds: 0,
            lost_here_before: 0,
            steps_down: 0,
            steps_up: 0,
        }
    }

    fn second(
        &mut self,
        sharer: &mut Sharer,
        end: &mut SharerEnd,
        network: &Network,
    ) -> Result<(), String> {
        let now = Instant::now();
        if now < self.next {
            return Ok(());
        }
        self.next = (self.next + Duration::from_secs(1)).max(now);
        self.seconds += 1;
        let frames = std::mem::take(&mut end.sent);
        let mut encode_ms: Vec<f32> = frames.iter().map(|frame| frame.encode_ms).collect();
        // Frames the sharer lost itself are no sign of the network.
        let lost_here = sharer.numbers().reported_lost_here;
        let excused =
            lost_here.saturating_sub(std::mem::replace(&mut self.lost_here_before, lost_here));
        let second = Second {
            sent: u32::try_from(frames.len()).unwrap_or(u32::MAX),
            lost: end
                .lost
                .take()
                .saturating_sub(u32::try_from(excused).unwrap_or(u32::MAX)),
            bytes: frames.iter().map(|frame| frame.packet_bytes as u64).sum(),
            round_trip: network.round_trip(now),
            // The loopback times its round trip from answered pings only.
            unanswered_ms: None,
            // As the room's sharer: a report at most 2 s old.
            shard_loss: end
                .shard_loss
                .filter(|&(_, at)| now.saturating_duration_since(at) <= Duration::from_secs(2))
                .map(|(percent, _)| percent),
            encode_ms: spread(&mut encode_ms).map(|(median, _)| median),
            interval: Duration::from_secs(1) / sharer.fps(),
            internet: self.internet,
        };
        let decision = self.rate.second(now, &second);
        if decision.backoff.is_some() {
            end.lost.backed_off();
        }
        let _ = end.lines.send(Line::Log(format!(
            "{} s, {} backoffs so far: {}",
            self.seconds,
            self.rate.backoffs(),
            self.rate.describe(&decision)
        )));
        if let Some(kbps) = decision.rate_kbps
            && let Err(err) = sharer.set_bitrate(kbps.saturating_mul(1000))
        {
            let _ = end
                .lines
                .send(Line::Say(format!("could not set {kbps} kbit/s: {err}")));
        }
        if let Some(step) = decision.step {
            let mut say = |line| {
                let _ = end.lines.send(line);
            };
            let stepped = match step {
                Step::Down(_) => sharer.step_down(&mut say)?,
                Step::Up => sharer.step_up(&mut say)?,
            };
            if stepped {
                match step {
                    Step::Down(_) => self.steps_down += 1,
                    Step::Up => self.steps_up += 1,
                }
                let (width, height) = sharer.size();
                let _ = end.lines.send(Line::Say(format!(
                    "{} s: {} to {width}x{height} at {} fps, {} kbit/s",
                    self.seconds,
                    match step {
                        Step::Down(_) => "stepped down",
                        Step::Up => "stepped back up",
                    },
                    sharer.fps(),
                    self.rate.rate_kbps()
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::thread::{self, JoinHandle};
    use std::time::{Duration, Instant};

    use capture::Adapter;
    use encode::Settings;
    use share::{Choice, Clock, fine_timers_held};

    use super::super::network::Shape;
    use super::*;

    const NVIDIA: u32 = 0x10de;

    fn setup(adapter: Adapter) -> Setup {
        Setup {
            share: share::Setup {
                choice: Choice::Pattern {
                    adapter,
                    width: 640,
                    height: 360,
                    busy: false,
                },
                fps: 120,
                settings: Settings::default(),
                encoder: None,
                codec: Some(share::Codec::H264),
                takes_hevc: false,
                payload: share::PAYLOAD_INTERNET,
                spread: false,
                clock: Clock::starting(Instant::now()),
                keep_times: true,
            },
            loss_percent: 0.0,
            seed: 1,
            internet: false,
        }
    }

    // run() on its own thread as the loopback starts it, up to its answer
    // on `started`.
    struct Running {
        link: Arc<Link>,
        started: Result<Started, String>,
        // FineTimers held once the answer came.
        held: u32,
        ready: mpsc::Sender<()>,
        thread: JoinHandle<Result<Shared, String>>,
    }

    fn start(setup: Setup) -> Running {
        let link = Arc::new(Link::new(true).expect("an inbox for the link"));
        let idle = Shape {
            delay: Duration::ZERO,
            jitter: Duration::ZERO,
            capacity_bits: None,
            lift_after: None,
            seed: 1,
        };
        let network = Arc::new(Network::new(idle, Arc::clone(&link)).expect("a network"));
        let (lines, said) = mpsc::channel();
        let (started, starting) = mpsc::channel();
        let (ready, viewer_ready) = mpsc::channel();
        let thread = thread::spawn({
            let link = Arc::clone(&link);
            move || {
                let result = run(&setup, &link, &network, &lines, &started, &viewer_ready);
                for line in said.try_iter() {
                    println!("{line:?}");
                }
                result
            }
        });
        let started = starting
            .recv()
            .unwrap_or_else(|_| Err(String::from("run() ended without an answer")));
        Running {
            link,
            started,
            held: fine_timers_held(),
            ready,
            thread,
        }
    }

    fn joined(thread: JoinHandle<Result<Shared, String>>) -> Result<Shared, String> {
        thread
            .join()
            .unwrap_or_else(|_| panic!("the sharer's thread panicked"))
    }

    // Other tests in this binary never share, so every FineTimer counted is
    // this test's. One share at a time, on a small pattern, and nothing of
    // the screen is captured.
    #[test]
    fn fine_timer_held_and_given_back() {
        assert_eq!(fine_timers_held(), 0);

        // The source does not open, and nothing reaches a GPU.
        let gone = Adapter {
            description: String::from("a graphics card that is not there"),
            vendor_id: 0,
            device_id: 0,
            luid: 0,
        };
        let Running {
            started, thread, ..
        } = start(setup(gone));
        let why = started.err().expect("no source to open");
        assert!(why.contains("a graphics card that is not there"), "{why}");
        assert_eq!(joined(thread).err(), Some(why));
        assert_eq!(fine_timers_held(), 0);

        let adapters = capture::adapters().unwrap_or_else(|err| panic!("{err}"));
        let Some(adapter) = adapters
            .iter()
            .find(|adapter| adapter.vendor_id == NVIDIA)
            .or(adapters.first())
            .cloned()
        else {
            println!("skipped the rest: this PC has no graphics card to make the pattern on");
            return;
        };

        // The viewer's window never opens.
        let Running {
            started,
            held,
            ready,
            thread,
            ..
        } = start(setup(adapter.clone()));
        if let Err(why) = started {
            panic!("{why}");
        }
        assert_eq!(held, 1, "the 1 ms timer while the sharer waits");
        drop(ready);
        let shared = joined(thread).unwrap_or_else(|err| panic!("{err}"));
        assert_eq!(shared.numbers.encoded, 0);
        assert_eq!(fine_timers_held(), 0);

        // Frames go out until the link stops, as when a timed run is over.
        let Running {
            link,
            started,
            held,
            ready,
            thread,
        } = start(setup(adapter));
        if let Err(why) = started {
            panic!("{why}");
        }
        assert_eq!(held, 1, "the 1 ms timer while sharing");
        ready.send(()).expect("the sharer waits for the viewer");
        thread::sleep(Duration::from_millis(200));
        assert_eq!(fine_timers_held(), 1);
        link.stop();
        let shared = joined(thread).unwrap_or_else(|err| panic!("{err}"));
        assert!(shared.numbers.encoded > 0, "no frame went out in 200 ms");
        assert_eq!(fine_timers_held(), 0);
    }
}
