// booth.exe --loopback: the whole video path on this PC, capture (or the
// test pattern) to encode, packets with a loss knob, reassembly,
// decode and present, so capture to display can be seen and measured
// before any of it goes through a room. The path itself is the share crate,
// the same one a room runs; what is here stands in for the room around it,
// and with --delay, --jitter or --capacity for a network between the two
// sides (network.rs), where the rate's backoff runs as in a room. It opens no
// profile, key, socket, firewall rule or room; with --log it only appends
// to booth.log in the profile's data folder.

mod link;
mod network;
mod numbers;
mod sharer;
mod viewing;

use std::any::Any;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use capture::Monitor;
use encode::{Codec, Kind, Preset, Settings};
use share::{Choice, Clock, Line, LinkNumbers, Watch};
use viewer::{LinkState, PathWord, Show};
use windows_sys::Win32::System::Console::{
    ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, STD_ERROR_HANDLE,
};

use crate::win;
use link::{Link, StopOnDrop};
use network::{Network, QUEUE_DEPTH, Shape};
use numbers::Totals;
use sharer::{Setup, Started};

pub const DEFAULT_FPS: u32 = 120;
pub const DEFAULT_BITRATE_MBITS: u32 = 15;
pub const DEFAULT_PATTERN: (u32, u32) = (2560, 1440);

#[derive(Debug, Clone, PartialEq)]
pub struct Options {
    // Print the monitors and stop.
    pub list: bool,
    pub source: Source,
    // The pattern as noise, which sends as much as the rate lets it.
    pub busy: bool,
    pub fps: u32,
    pub bitrate_mbits: u32,
    pub loss_percent: f64,
    pub seed: Option<u64>,
    pub encoder: Option<Kind>,
    // None is auto: as a room picks it, with this PC's viewer as the one
    // watcher (share::Setup::codec).
    pub codec: Option<Codec>,
    pub seconds: Option<u64>,
    pub network: NetworkOptions,
}

// A network between the two sides (network.rs); all zero and None is none.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct NetworkOptions {
    // One way.
    pub delay_ms: f64,
    // The mean extra delay, one way.
    pub jitter_ms: f64,
    pub capacity_mbits: Option<f64>,
    // The capacity is lifted after this many seconds of sending.
    pub lift_seconds: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    // An index in capture's monitor list; None is the primary monitor.
    Screen(Option<usize>),
    // A monitor of this size, made up: one past 4096 wide or 1440 high is
    // scaled down to fit the way a capture would be.
    Pattern(u32, u32),
}

impl Default for Options {
    fn default() -> Options {
        Options {
            list: false,
            source: Source::Screen(None),
            busy: false,
            fps: DEFAULT_FPS,
            bitrate_mbits: DEFAULT_BITRATE_MBITS,
            loss_percent: 0.0,
            seed: None,
            encoder: None,
            codec: None,
            seconds: None,
            network: NetworkOptions::default(),
        }
    }
}

pub fn run(options: &Options, profile: Option<&str>, log: bool) -> ExitCode {
    console_for_stderr();
    let mut out = Output::none();
    let result = Output::open(profile, log).and_then(|opened| {
        out = opened;
        loopback(options, &mut out)
    });
    let Err(problem) = result else {
        return ExitCode::SUCCESS;
    };
    out.say(&format!("loopback stopped: {problem}"));
    // A timed run is someone measuring, maybe next to a game, and a box
    // would take the focus.
    if cfg!(not(debug_assertions)) && options.seconds.is_none() {
        win::error_box(&format!("The loopback stopped: {problem}."));
    }
    ExitCode::FAILURE
}

fn loopback(options: &Options, out: &mut Output) -> Result<(), String> {
    // winit does this for the panel's window; capture refuses to open
    // without it, and the viewer's window wants it too.
    capture::make_process_dpi_aware().map_err(|err| err.to_string())?;
    let monitors = capture::monitors().map_err(|err| err.to_string())?;
    if options.list {
        for (index, monitor) in monitors.iter().enumerate() {
            out.say(&format!("monitor {index}: {monitor}"));
        }
        if monitors.is_empty() {
            out.say("no monitor is attached to the desktop");
        }
        return Ok(());
    }
    let seed = options.seed.unwrap_or_else(share::fresh_seed);
    let shape = shape(&options.network, seed);
    // Auto asks what the room's viewer thread asks as a room opens: whether
    // a viewer on this PC decodes HEVC. The viewer's own device answers
    // again once its window is open.
    let takes_hevc = match options.codec {
        Some(_) => true,
        None => match share::primary_takes_hevc() {
            Ok(()) => true,
            Err(why) => {
                out.say(&format!("the viewer does not take HEVC: {why}"));
                false
            }
        },
    };
    let setup = Setup {
        share: share::Setup {
            choice: choose(options.source, options.busy, &monitors)?,
            fps: options.fps,
            settings: Settings {
                bitrate: options.bitrate_mbits * 1_000_000,
                preset: Preset::P1,
            },
            encoder: options.encoder,
            codec: options.codec,
            takes_hevc,
            // The size the internet path uses, which makes more packets of
            // each frame for the knob to hit.
            payload: share::PAYLOAD_INTERNET,
            // As the room spreads them for a watcher over the internet;
            // with nothing queued on the way there is nothing to spread for.
            spread: !shape.idle(),
            // Standing in for the room's ping clock, on both sides.
            clock: Clock::starting(Instant::now()),
            // For the summary line.
            keep_times: true,
        },
        loss_percent: options.loss_percent,
        seed,
        internet: !shape.idle(),
    };
    out.say(&describe(options, &setup));

    let link = Arc::new(Link::new(takes_hevc).map_err(|err| err.to_string())?);
    let network = Arc::new(Network::new(shape, Arc::clone(&link)).map_err(|err| err.to_string())?);
    // With no network asked for, packets go straight to the viewer and
    // nothing pings: the strip shows no round trip.
    let networking = if network.idle() {
        None
    } else {
        let thread = thread::Builder::new()
            .name("loopback network".into())
            .spawn({
                let (network, link) = (Arc::clone(&network), Arc::clone(&link));
                move || {
                    let _stop = StopOnDrop(link);
                    network.run()
                }
            })
            .map_err(|err| format!("could not start the network's thread: {err}"))?;
        Some(thread)
    };
    let (lines, said) = mpsc::channel();
    let (started, starting) = mpsc::channel();
    let (ready, viewer_ready) = mpsc::channel();
    let sharing = thread::Builder::new()
        .name("loopback sharer".into())
        .spawn({
            let (setup, link, network, lines) = (
                setup.clone(),
                Arc::clone(&link),
                Arc::clone(&network),
                lines.clone(),
            );
            move || {
                let _stop = StopOnDrop(Arc::clone(&link));
                sharer::run(&setup, &link, &network, &lines, &started, &viewer_ready)
            }
        });
    let sharing = match sharing {
        Ok(thread) => thread,
        Err(err) => {
            let _ = stop_network(&link, networking);
            return Err(format!("could not start the sharer's thread: {err}"));
        }
    };
    let started: Started = match starting.recv() {
        Ok(Ok(started)) => started,
        // The thread says why in its result.
        Ok(Err(_)) | Err(_) => {
            drop(lines);
            for line in said {
                out.line(line);
            }
            let sharer = joined(sharing.join(), "sharer");
            let _ = stop_network(&link, networking);
            return match sharer {
                Ok(_) => Err(String::from("the sharer stopped before its first frame")),
                Err(problem) => Err(problem),
            };
        }
    };
    // What the sharer said while it opened, such as why it runs at a smaller
    // size, goes before the line about what it opened.
    for line in said.try_iter() {
        out.line(line);
    }
    out.say(&format!(
        "sharing {}x{} at {} fps with {}",
        started.width, started.height, started.fps, started.encoder
    ));
    let watch = Watch {
        title: String::from("Booth loopback"),
        width: started.width,
        height: started.height,
        fps: started.fps,
        vsync: false,
        hevc: true,
        // A timed run never takes the focus from a game.
        show: if options.seconds.is_some() {
            Show::NoActivate
        } else {
            Show::Activate
        },
        clock: Some(setup.share.clock),
        // No network, so no round trip, jitter or ping trace: they are left
        // out rather than shown as zero, "loop" says why, and the loss is
        // the knob's. A network's pings fill them in as they come.
        link: LinkNumbers {
            state: LinkState::Live,
            path: Some(PathWord::Loop),
            ..LinkNumbers::default()
        },
        keep_times: true,
    };
    let seconds = options.seconds.map(Duration::from_secs);
    let watching = thread::Builder::new()
        .name("loopback viewer".into())
        .spawn({
            let (link, network, lines) = (Arc::clone(&link), Arc::clone(&network), lines.clone());
            move || {
                let _stop = StopOnDrop(Arc::clone(&link));
                viewing::run(&watch, seconds, &link, &network, &lines, ready)
            }
        });
    let watching = match watching {
        Ok(thread) => thread,
        Err(err) => {
            // The sharer is waiting for the viewer, and the ready sender
            // went with the closure that never ran.
            link.stop();
            let _ = sharing.join();
            let _ = stop_network(&link, networking);
            return Err(format!("could not start the viewer's thread: {err}"));
        }
    };
    // Every line until both threads are done: they hold the other senders.
    drop(lines);
    for line in said {
        out.line(line);
    }
    let viewer = joined(watching.join(), "viewer");
    let sharer = joined(sharing.join(), "sharer");
    let network_ran = stop_network(&link, networking);
    let (mut shared, mut viewer) = match (sharer, viewer) {
        (Ok(shared), Ok(viewer)) => (shared, viewer),
        (Err(problem), Ok(_)) | (Ok(_), Err(problem)) => return Err(problem),
        (Err(sharer), Err(viewer)) => return Err(format!("{viewer}; and {sharer}")),
    };
    network_ran?;
    out.say(&format!(
        "loopback {}",
        numbers::summary(Totals {
            shared: &mut shared,
            viewer: &mut viewer,
            knob_dropped: link.knob_dropped.load(Ordering::Relaxed),
            overflow: link.inbox.overflow(),
            network: (!network.idle()).then(|| network.numbers()),
        })
    ));
    Ok(())
}

// The network's thread ends within a ping's interval of the link stopping.
fn stop_network(
    link: &Link,
    thread: Option<thread::JoinHandle<Result<(), String>>>,
) -> Result<(), String> {
    link.stop();
    thread.map_or(Ok(()), |thread| joined(thread.join(), "network"))
}

fn shape(options: &NetworkOptions, seed: u64) -> Shape {
    let ms = |ms: f64| Duration::from_secs_f64(ms / 1000.0);
    Shape {
        delay: ms(options.delay_ms),
        jitter: ms(options.jitter_ms),
        capacity_bits: options
            .capacity_mbits
            .map(|mbits| (mbits * 1_000_000.0).round() as u64),
        lift_after: options.lift_seconds.map(Duration::from_secs),
        seed,
    }
}

fn joined<T>(result: thread::Result<Result<T, String>>, which: &str) -> Result<T, String> {
    result.unwrap_or_else(|payload| {
        Err(format!(
            "the {which}'s thread panicked: {}",
            panic_text(&*payload)
        ))
    })
}

fn panic_text(payload: &(dyn Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("no message")
}

fn choose(source: Source, busy: bool, monitors: &[Monitor]) -> Result<Choice, String> {
    let primary = || monitors.iter().find(|m| m.primary).or(monitors.first());
    match source {
        Source::Pattern(width, height) => {
            // On the GPU that drives the primary monitor, where the viewer
            // opens too unless the pointer is on another GPU's monitor.
            let adapter = match primary() {
                Some(monitor) => monitor.adapter.clone(),
                None => capture::adapters()
                    .map_err(|err| err.to_string())?
                    .into_iter()
                    .next()
                    .ok_or("this PC has no graphics card the pattern can be made on")?,
            };
            Ok(Choice::Pattern {
                adapter,
                width,
                height,
                busy,
            })
        }
        Source::Screen(None) => primary()
            .cloned()
            .map(Choice::Screen)
            .ok_or_else(|| String::from("no monitor is attached to the desktop")),
        Source::Screen(Some(index)) => match monitors.get(index) {
            Some(monitor) => Ok(Choice::Screen(monitor.clone())),
            None if monitors.is_empty() => {
                Err(String::from("no monitor is attached to the desktop"))
            }
            None => Err(format!(
                "there is no monitor {index}: this PC has monitors 0 to {}, and booth --loopback --list shows them",
                monitors.len() - 1
            )),
        },
    }
}

fn describe(options: &Options, setup: &Setup) -> String {
    let source = match &setup.share.choice {
        Choice::Screen(monitor) => format!("the screen of {}", monitor.name),
        Choice::Pattern {
            width,
            height,
            busy,
            ..
        } => format!(
            "the pattern at {width}x{height}{}",
            if *busy { " as noise" } else { "" }
        ),
    };
    let encoder = match options.encoder {
        Some(kind) => kind.to_string(),
        None => String::from("the encoder a share would pick"),
    };
    let codec = match options.codec {
        Some(codec) => codec.to_string(),
        None => format!(
            "the codec a room would pick for a viewer that takes {}",
            if setup.share.takes_hevc {
                "HEVC"
            } else {
                "H.264 only"
            }
        ),
    };
    let loss = if options.loss_percent > 0.0 {
        format!(
            "{}% of packets dropped (seed {})",
            options.loss_percent, setup.seed
        )
    } else {
        format!("no packets dropped (seed {})", setup.seed)
    };
    let length = match options.seconds {
        Some(seconds) => format!("for {seconds} s"),
        None => String::from("until the viewer is closed"),
    };
    format!(
        "loopback: {source}, up to {} fps, {encoder} in {codec} at {} Mbit/s, {loss}, {}, {length}",
        setup.share.fps,
        options.bitrate_mbits,
        network_text(&options.network)
    )
}

fn network_text(network: &NetworkOptions) -> String {
    if shape(network, 0).idle() {
        return String::from("no network between the two sides and no backoff");
    }
    let mut text = format!(
        "a network of {} ms each way and {} ms of jitter on average",
        network.delay_ms, network.jitter_ms
    );
    if let Some(mbits) = network.capacity_mbits {
        text.push_str(&format!(
            " through {mbits} Mbit/s with a {} ms queue",
            QUEUE_DEPTH.as_millis()
        ));
        if let Some(seconds) = network.lift_seconds {
            text.push_str(&format!(", lifted after {seconds} s"));
        }
    }
    text
}

// A release booth.exe has no console of its own. Started from a terminal
// with nothing redirected, its lines go to that terminal.
fn console_for_stderr() {
    // SAFETY: plain calls with constant arguments; a failure leaves stderr
    // as it was, which only means the lines go nowhere.
    unsafe {
        if GetStdHandle(STD_ERROR_HANDLE).is_null() {
            AttachConsole(ATTACH_PARENT_PROCESS);
        }
    }
}

struct Output {
    log: Option<(File, PathBuf)>,
    failed: bool,
}

impl Output {
    fn none() -> Output {
        Output {
            log: None,
            failed: false,
        }
    }

    // The data folder is made if it is not there, as the panel makes it;
    // nothing else in it is touched.
    fn open(profile: Option<&str>, log: bool) -> Result<Output, String> {
        if !log {
            return Ok(Output::none());
        }
        let dir = keys::data_dir(profile).map_err(|err| err.to_string())?;
        let path = dir.join("booth.log");
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|err| format!("could not open {}: {err}", path.display()))?;
        Ok(Output {
            log: Some((file, path)),
            failed: false,
        })
    }

    fn line(&mut self, line: Line) {
        match line {
            Line::Say(text) => self.say(&text),
            Line::Log(text) => self.log(&text),
        }
    }

    fn say(&mut self, text: &str) {
        eprintln!("booth: {text}");
        self.log(text);
    }

    fn log(&mut self, text: &str) {
        let Some((file, path)) = &mut self.log else {
            return;
        };
        let line = format!("{} {:<6} {text}\r\n", win::utc_stamp(), "loop");
        if let Err(err) = file.write_all(line.as_bytes())
            && !self.failed
        {
            self.failed = true;
            eprintln!(
                "booth: could not write to {}: {err}; the rest of this run is not in the log",
                path.display()
            );
        }
    }
}
