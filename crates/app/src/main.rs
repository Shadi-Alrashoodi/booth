#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod backlog;
mod controls;
mod elevated;
mod firewall;
mod hotkeys;
mod loopback;
mod mark;
mod messages;
mod monitors;
mod remote;
mod running;
mod screens;
mod settings;
mod sound;
mod strip;
mod theme;
mod tray;
mod update;
mod win;
mod winver;

use std::ffi::OsString;
use std::iter::Peekable;
use std::ops::RangeInclusive;
use std::process::ExitCode;
use std::str::FromStr;
use std::time::Duration;

use eframe::egui::{IconData, ViewportBuilder};
use eframe::egui_wgpu::{SurfaceConfig, WgpuConfiguration, WgpuSetup, WgpuSetupCreateNew};
use eframe::wgpu;

use crate::app::NotStarted;
use crate::backlog::Backlog;
use crate::firewall::Firewall;
use crate::win::Rights;

const USAGE: &str = "usage: booth [--profile NAME] [--port N] [--log] [--video-loss PERCENT]
       booth --loopback [--list] [--monitor N | --pattern [WxH] [--busy]] [--fps N]
                        [--bitrate MBITS] [--loss PERCENT] [--seed N]
                        [--encoder nvenc|hardware|software] [--codec h264|hevc|auto]
                        [--delay MS] [--jitter MS] [--capacity MBITS [--lift SECONDS]]
                        [--seconds N] [--profile NAME] [--log]";
// A UPnP deletion is two HTTP calls, each up to 2 s on a router that has
// stopped answering; a working one takes well under a second.
const MAPPINGS_WAIT: Duration = Duration::from_secs(3);

pub struct Args {
    pub profile: Option<String>,
    // Wins over the port in settings for this run.
    pub port: Option<u16>,
    // Write booth.log next to the identity key, for finding out why a
    // connection fails.
    pub log: bool,
    // Run the video path on this PC instead of the panel.
    pub loopback: Option<loopback::Options>,
    // Drop this share of the video packets that arrive for the share this PC
    // watches, to see with a friend how a share holds up under loss.
    // Nothing else on this PC or theirs changes.
    pub video_loss: Option<f64>,
}

impl Args {
    // Options that change only this run, which a copy already open on the
    // profile does not have. --profile only picks the folder.
    pub fn for_this_run(&self) -> bool {
        self.port.is_some() || self.log || self.video_loss.is_some()
    }
}

fn main() -> ExitCode {
    // Before anything else, so an older Windows gets a sentence rather than a
    // failure somewhere in capture, audio or the window.
    if let Some(problem) = winver::this_pc().and_then(winver::too_old) {
        eprintln!("booth: {problem}");
        win::error_box(&problem);
        return ExitCode::FAILURE;
    }
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    // First, before any window, profile or key: this may be the process the
    // administrator prompt started.
    if elevated::asked_for(&args) {
        win::system_dlls_only();
        return elevated::run(&args);
    }
    // The panel parses invites and packets from the internet, and nothing
    // of that gets administrator rights. An elevated game launcher, or Run
    // as administrator, would hand them over.
    let rights = win::rights();
    if rights == Rights::Elevated {
        eprintln!("booth: {}", messages::RUNNING_ELEVATED);
        if cfg!(not(debug_assertions)) {
            win::error_box(messages::RUNNING_ELEVATED);
        }
        return ExitCode::FAILURE;
    }
    let args = match parse_args(args.into_iter()) {
        Ok(args) => args,
        Err(problem) => {
            eprintln!("booth: {problem}");
            eprintln!("{USAGE}");
            // Started from a shortcut or Explorer, a release build has no
            // console, and a typo in the shortcut would otherwise end in
            // nothing happening at all.
            if cfg!(not(debug_assertions)) {
                win::error_box(&format!(
                    "Booth could not start: {problem}. Start it with no options, or with --profile and a name, --port and a number from 1 to 65535, --log, or --video-loss and a percentage from 0 to 100. The video test starts with --loopback and takes --list, --monitor, --pattern, --busy, --fps, --bitrate, --loss, --seed, --encoder, --codec, --delay, --jitter, --capacity, --lift, --seconds, --profile and --log."
                ));
            }
            return ExitCode::from(2);
        }
    };
    let mark = icon();
    viewer::set_icon(viewer::Icon {
        rgba: mark.rgba,
        width: mark.width,
        height: mark.height,
    });
    // Before any profile, key, socket or firewall check: the loopback needs
    // none of them.
    if let Some(options) = &args.loopback {
        return loopback::run(options, args.profile.as_deref(), args.log);
    }
    // Started before the window, which takes far longer to open, so the
    // answer is usually in by the first frame.
    let mut backlog = Backlog::default();
    // The port in settings is the one the rule is checked for, so they are
    // read first; it takes a few milliseconds.
    let setup = match app::load(&args, &mut backlog) {
        Ok(setup) => Ok(setup),
        Err(NotStarted::Blocked(why)) => Err(why),
        Err(NotStarted::BroughtForward) => return ExitCode::SUCCESS,
    };
    let port = match &setup {
        Ok(setup) => setup.port(),
        Err(_) => args.port.unwrap_or(settings::DEFAULT_PORT),
    };
    let firewall = Firewall::check_now(port, &mut backlog);
    if rights == Rights::AlwaysFull {
        backlog.add(String::from(
            "firewall: this account runs every program as administrator (uac off, or the built-in administrator), the panel included",
        ));
    }
    let result = eframe::run_native(
        "Booth",
        native_options(),
        Box::new(move |cc| Ok(Box::new(app::App::new(cc, setup, firewall, backlog)))),
    );
    // The window is gone and the room with it, but a router slow to delete
    // the port mapping may still be answering. Returning ends the process
    // and that thread with it, and the forward to this PC would stay open.
    // Nobody sees this wait.
    room::finish_port_mappings(MAPPINGS_WAIT);
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            win::error_box(&format!(
                "Could not open the Booth window: {err}. Booth draws with DirectX 12. Update the graphics driver and start Booth again."
            ));
            ExitCode::FAILURE
        }
    }
}

fn parse_args(args: impl Iterator<Item = OsString>) -> Result<Args, String> {
    let mut args = args.peekable();
    let mut profile = None;
    let mut port = None;
    let mut log = false;
    let mut loopback = false;
    let mut video_loss = None;
    let mut looping = Looping::default();
    while let Some(arg) = args.next() {
        match text(arg)?.as_str() {
            "--profile" if profile.is_some() => {
                return Err(String::from("--profile is given twice"));
            }
            "--port" if port.is_some() => return Err(String::from("--port is given twice")),
            "--log" if log => return Err(String::from("--log is given twice")),
            "--loopback" if loopback => return Err(String::from("--loopback is given twice")),
            "--video-loss" if video_loss.is_some() => {
                return Err(String::from("--video-loss is given twice"));
            }
            "--video-loss" => {
                let value = next_text(&mut args)?;
                video_loss = Some(percentage(&value).ok_or(
                    "--video-loss needs a percentage from 0 to 100 after it, such as 5 or 2.5",
                )?);
            }
            "--log" => log = true,
            "--loopback" => loopback = true,
            "--profile" => {
                // A name that starts with a hyphen is the next option, with
                // the name itself left out.
                let name = args.next().map(text).transpose()?;
                let name = name.filter(|name| !name.is_empty() && !name.starts_with('-'));
                profile = Some(name.ok_or("--profile needs a name after it")?);
            }
            "--port" => {
                let value = args.next().map(text).transpose()?.unwrap_or_default();
                port = Some(
                    port_number(&value).ok_or("--port needs a number from 1 to 65535 after it")?,
                );
            }
            other => {
                if !looping.take(other, &mut args)? {
                    return Err(format!("{other} is not an option Booth knows"));
                }
            }
        }
    }
    let loopback = match (loopback, looping.first.take()) {
        (true, _) if port.is_some() => {
            return Err(String::from(
                "--port is for a room, and --loopback opens none",
            ));
        }
        (true, _) if video_loss.is_some() => {
            return Err(String::from(
                "--video-loss is for a room; the loopback takes --loss",
            ));
        }
        (true, _) => Some(looping.options()?),
        (false, Some(first)) => return Err(format!("{first} works only with --loopback")),
        (false, None) => None,
    };
    Ok(Args {
        profile,
        port,
        log,
        loopback,
        video_loss,
    })
}

// The loopback's own options, each at most once.
#[derive(Default)]
struct Looping {
    // The first one given, for the message when --loopback is missing.
    first: Option<String>,
    list: bool,
    monitor: Option<usize>,
    pattern: Option<(u32, u32)>,
    busy: bool,
    fps: Option<u32>,
    bitrate: Option<u32>,
    loss: Option<f64>,
    seed: Option<u64>,
    encoder: Option<encode::Kind>,
    // Given, and then None for auto.
    codec: Option<Option<encode::Codec>>,
    seconds: Option<u64>,
    delay: Option<f64>,
    jitter: Option<f64>,
    capacity: Option<f64>,
    lift: Option<u64>,
}

// A second of one-way delay is past any real path, and past what a share
// could be watched over.
const MOST_DELAY_MS: f64 = 1000.0;
// Up to the most the upload setting takes.
const CAPACITY_MBITS: RangeInclusive<f64> = 0.1..=80.0;

impl Looping {
    // False when `word` is none of them.
    fn take(
        &mut self,
        word: &str,
        args: &mut Peekable<impl Iterator<Item = OsString>>,
    ) -> Result<bool, String> {
        let once = |given: bool| {
            if given {
                Err(format!("{word} is given twice"))
            } else {
                Ok(())
            }
        };
        match word {
            "--list" => {
                once(self.list)?;
                self.list = true;
            }
            "--monitor" => {
                once(self.monitor.is_some())?;
                let value = next_text(args)?;
                self.monitor = Some(whole(&value).ok_or(
                    "--monitor needs a monitor number after it, as booth --loopback --list shows them",
                )?);
            }
            "--pattern" => {
                once(self.pattern.is_some())?;
                // The size may be left out; the next option starts with a
                // hyphen.
                let size = args.next_if(|next| !next.to_string_lossy().starts_with('-'));
                self.pattern = Some(match size {
                    None => loopback::DEFAULT_PATTERN,
                    Some(size) => {
                        let size = text(size)?;
                        pattern_size(&size).ok_or_else(|| {
                            format!(
                                "{size} is not a pattern size: give it as width x height, such as 1280x720, in even numbers from {}x{} to {}x{}",
                                PATTERN_WIDTHS.start(),
                                PATTERN_HEIGHTS.start(),
                                PATTERN_WIDTHS.end(),
                                PATTERN_HEIGHTS.end()
                            )
                        })?
                    }
                });
            }
            "--fps" => {
                once(self.fps.is_some())?;
                let value = next_text(args)?;
                self.fps = Some(
                    whole(&value)
                        .filter(|fps| (1..=240).contains(fps))
                        .ok_or("--fps needs a frame rate from 1 to 240 after it")?,
                );
            }
            "--bitrate" => {
                once(self.bitrate.is_some())?;
                let value = next_text(args)?;
                self.bitrate = Some(
                    whole(&value)
                        .filter(|mbits| (1..=80).contains(mbits))
                        .ok_or("--bitrate needs a number of Mbit/s from 1 to 80 after it")?,
                );
            }
            "--loss" => {
                once(self.loss.is_some())?;
                let value = next_text(args)?;
                self.loss =
                    Some(percentage(&value).ok_or(
                        "--loss needs a percentage from 0 to 100 after it, such as 5 or 2.5",
                    )?);
            }
            "--seed" => {
                once(self.seed.is_some())?;
                let value = next_text(args)?;
                self.seed =
                    Some(whole(&value).ok_or(
                        "--seed needs a whole number after it, as a loopback run prints it",
                    )?);
            }
            "--encoder" => {
                once(self.encoder.is_some())?;
                let value = next_text(args)?;
                if value.is_empty() || value.starts_with('-') {
                    return Err(String::from(
                        "--encoder needs nvenc, hardware or software after it",
                    ));
                }
                self.encoder = Some(value.parse()?);
            }
            "--codec" => {
                once(self.codec.is_some())?;
                let value = next_text(args)?;
                self.codec = Some(match value.to_ascii_lowercase().as_str() {
                    "auto" => None,
                    "h264" => Some(encode::Codec::H264),
                    "hevc" => Some(encode::Codec::Hevc),
                    "" => return Err(String::from("--codec needs h264, hevc or auto after it")),
                    word if word.starts_with('-') => {
                        return Err(String::from("--codec needs h264, hevc or auto after it"));
                    }
                    _ => {
                        return Err(format!(
                            "there is no codec called {value:?}: the choices are h264, hevc and auto"
                        ));
                    }
                });
            }
            "--seconds" => {
                once(self.seconds.is_some())?;
                let value = next_text(args)?;
                self.seconds = Some(
                    whole(&value)
                        .filter(|seconds| (1..=86_400).contains(seconds))
                        .ok_or("--seconds needs a number of seconds from 1 to 86400 after it")?,
                );
            }
            "--busy" => {
                once(self.busy)?;
                self.busy = true;
            }
            "--delay" | "--jitter" => {
                let slot = if word == "--delay" {
                    &mut self.delay
                } else {
                    &mut self.jitter
                };
                once(slot.is_some())?;
                let value = next_text(args)?;
                *slot = Some(
                    decimal(&value)
                        .filter(|ms| *ms <= MOST_DELAY_MS)
                        .ok_or_else(|| {
                            format!(
                                "{word} needs milliseconds from 0 to 1000 after it, such as 2 or 5.5"
                            )
                        })?,
                );
            }
            "--capacity" => {
                once(self.capacity.is_some())?;
                let value = next_text(args)?;
                self.capacity = Some(
                    decimal(&value)
                        .filter(|mbits| CAPACITY_MBITS.contains(mbits))
                        .ok_or(
                            "--capacity needs a number of Mbit/s from 0.1 to 80 after it, such as 5 or 2.5",
                        )?,
                );
            }
            "--lift" => {
                once(self.lift.is_some())?;
                let value = next_text(args)?;
                self.lift = Some(
                    whole(&value)
                        .filter(|seconds| (1..=86_400).contains(seconds))
                        .ok_or("--lift needs a number of seconds from 1 to 86400 after it")?,
                );
            }
            _ => return Ok(false),
        }
        self.first.get_or_insert_with(|| word.to_string());
        Ok(true)
    }

    fn options(self) -> Result<loopback::Options, String> {
        let source = match (self.monitor, self.pattern) {
            (Some(_), Some(_)) => {
                return Err(String::from(
                    "--monitor and --pattern do not go together: the pattern stands in for a monitor",
                ));
            }
            (Some(index), None) => loopback::Source::Screen(Some(index)),
            (None, Some((width, height))) => loopback::Source::Pattern(width, height),
            (None, None) => loopback::Source::Screen(None),
        };
        if self.busy && self.pattern.is_none() {
            return Err(String::from(
                "--busy works only with --pattern: the screen is shared as it is",
            ));
        }
        if self.lift.is_some() && self.capacity.is_none() {
            return Err(String::from(
                "--lift needs --capacity: it frees the link after that many seconds",
            ));
        }
        Ok(loopback::Options {
            list: self.list,
            source,
            busy: self.busy,
            fps: self.fps.unwrap_or(loopback::DEFAULT_FPS),
            bitrate_mbits: self.bitrate.unwrap_or(loopback::DEFAULT_BITRATE_MBITS),
            loss_percent: self.loss.unwrap_or(0.0),
            seed: self.seed,
            encoder: self.encoder,
            codec: self.codec.flatten(),
            seconds: self.seconds,
            network: loopback::NetworkOptions {
                delay_ms: self.delay.unwrap_or(0.0),
                jitter_ms: self.jitter.unwrap_or(0.0),
                capacity_mbits: self.capacity,
                lift_seconds: self.lift,
            },
        })
    }
}

// The value after an option, or nothing when the options end there.
fn next_text(args: &mut impl Iterator<Item = OsString>) -> Result<String, String> {
    Ok(args.next().map(text).transpose()?.unwrap_or_default())
}

// Digits only, as for the port.
fn whole<T: FromStr>(text: &str) -> Option<T> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

fn percentage(text: &str) -> Option<f64> {
    decimal(text).filter(|pct| (0.0..=100.0).contains(pct))
}

// Digits, with a fraction after a point if any: f64's own parser also takes
// signs, exponents, "inf" and "NaN".
fn decimal(text: &str) -> Option<f64> {
    let (units, fraction) = text.split_once('.').unwrap_or((text, "0"));
    let digits = |part: &str| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit());
    if !digits(units) || !digits(fraction) {
        return None;
    }
    text.parse().ok()
}

// Up to two 4K monitors' width side by side, as the widest monitors sold
// are: the share scales those to 4096 wide, and the loopback is how that
// path gets measured on a PC without one.
const PATTERN_WIDTHS: RangeInclusive<u32> = 64..=7680;
const PATTERN_HEIGHTS: RangeInclusive<u32> = 64..=2160;

fn pattern_size(text: &str) -> Option<(u32, u32)> {
    let (width, height) = text.split_once('x')?;
    let (width, height) = (whole::<u32>(width)?, whole::<u32>(height)?);
    let even = width % 2 == 0 && height % 2 == 0;
    (even && PATTERN_WIDTHS.contains(&width) && PATTERN_HEIGHTS.contains(&height))
        .then_some((width, height))
}

fn text(arg: OsString) -> Result<String, String> {
    arg.into_string()
        .map_err(|_| String::from("one of the options is not valid text"))
}

// Digits only: u16's own parser also takes a leading plus sign.
fn port_number(text: &str) -> Option<u16> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok().filter(|&port| port != 0)
}

fn native_options() -> eframe::NativeOptions {
    let mut setup = WgpuSetupCreateNew::without_display_handle();
    // DX12 only: it is on every Windows 10 and 11 PC, and leaving Vulkan and
    // GL out means one driver path to test instead of three.
    setup.instance_descriptor.backends = wgpu::Backends::DX12;
    eframe::NativeOptions {
        viewport: ViewportBuilder::default()
            .with_title("Booth")
            .with_app_id("Booth")
            .with_inner_size([360.0, 640.0])
            .with_min_inner_size([320.0, 400.0])
            .with_icon(icon()),
        wgpu_options: WgpuConfiguration {
            // One frame in flight, so a click shows up on the next vblank
            // rather than two behind.
            surface: SurfaceConfig::LOW_LATENCY,
            wgpu_setup: WgpuSetup::CreateNew(setup),
            ..WgpuConfiguration::default()
        },
        ..eframe::NativeOptions::default()
    }
}

// The title bar, taskbar and Alt+Tab icon of the panel and the viewer: the
// mark in amber on a plate in panel tone, its corners rounded 3/16 of its
// size. The plate keeps the mark readable on a light taskbar, where amber
// alone falls to about 2:1.
fn icon() -> IconData {
    const SIZE: u32 = 32;
    let bars = mark::rects(SIZE);
    let mut rgba = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    for y in 0..SIZE {
        for x in 0..SIZE {
            let on_mark = bars
                .iter()
                .any(|[l, t, r, b]| (*l..*r).contains(&x) && (*t..*b).contains(&y));
            let color = if on_mark { theme::AMBER } else { theme::PANEL };
            let [r, g, b, _] = color.to_array();
            rgba.extend_from_slice(&[r, g, b, plate_cover(x, y, SIZE)]);
        }
    }
    IconData {
        rgba,
        width: SIZE,
        height: SIZE,
    }
}

// How much of the pixel at x, y the rounded plate covers, from 4 by 4
// samples, so its corners are smooth without a path renderer.
fn plate_cover(x: u32, y: u32, size: u32) -> u8 {
    let side = size as f32;
    let corner = side * 3.0 / 16.0;
    let mut inside = 0u32;
    for sy in 0..4 {
        for sx in 0..4 {
            let px = x as f32 + (sx as f32 + 0.5) / 4.0;
            let py = y as f32 + (sy as f32 + 0.5) / 4.0;
            let nearest_x = px.clamp(corner, side - corner);
            let nearest_y = py.clamp(corner, side - corner);
            if (px - nearest_x).hypot(py - nearest_y) <= corner {
                inside += 1;
            }
        }
    }
    (inside * 255 / 16) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Args, String> {
        parse_args(args.iter().map(OsString::from))
    }

    #[test]
    fn reads_profile_and_port() {
        let args = parse(&["--profile", "a", "--port", "41010"]).unwrap();
        assert_eq!(args.profile.as_deref(), Some("a"));
        assert_eq!(args.port, Some(41010));
        assert!(!args.log);
        assert_eq!(parse(&[]).unwrap().port, None);
    }

    #[test]
    fn log_is_off_unless_asked_for() {
        assert!(!parse(&[]).unwrap().log);
        assert!(parse(&["--log"]).unwrap().log);
        let args = parse(&["--port", "41060", "--log", "--profile", "b"]).unwrap();
        assert!(args.log);
        assert_eq!(args.port, Some(41060));
        assert!(parse(&["--log", "--log"]).is_err());
    }

    #[test]
    fn refuses_anything_else() {
        for args in [
            &["--port"][..],
            &["--port", "0"],
            &["--port", "70000"],
            &["--port", "x"],
            &["--profile"],
            &["--profile", "a", "--profile", "b"],
            &["--port", "+41010"],
            &["--port", "41010", "--port", "41011"],
            &["--profile", "--port", "41010"],
            &["--profile", ""],
            &["--verbose"],
            &["a"],
        ] {
            assert!(parse(args).is_err(), "{args:?}");
        }
    }

    // The loss knob for a test with a friend, which only this PC's watching
    // feels.
    #[test]
    fn video_loss_is_a_percentage_for_a_room() {
        assert_eq!(parse(&[]).unwrap().video_loss, None);
        let args = parse(&["--video-loss", "5", "--profile", "a"]).unwrap();
        assert_eq!(args.video_loss, Some(5.0));
        assert_eq!(
            parse(&["--video-loss", "2.5"]).unwrap().video_loss,
            Some(2.5)
        );
        for bad in [
            &["--video-loss"][..],
            &["--video-loss", "101"],
            &["--video-loss", "-1"],
            &["--video-loss", "5%"],
            &["--video-loss", "5", "--video-loss", "6"],
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
        assert_eq!(
            parse(&["--video-loss", "5", "--loopback"]).err().as_deref(),
            Some("--video-loss is for a room; the loopback takes --loss")
        );
    }

    // A copy started with these is not handed to one already open, which
    // would run on without them.
    #[test]
    fn only_the_profile_lets_an_open_copy_take_over() {
        for args in [&[][..], &["--profile", "a"]] {
            assert!(!parse(args).unwrap().for_this_run(), "{args:?}");
        }
        for args in [
            &["--log"][..],
            &["--port", "41010"],
            &["--video-loss", "5"],
            &["--profile", "a", "--log"],
        ] {
            assert!(parse(args).unwrap().for_this_run(), "{args:?}");
        }
    }

    #[test]
    fn says_which_option_was_wrong() {
        let problem = parse(&["--prot", "41010"]).err().unwrap();
        assert_eq!(problem, "--prot is not an option Booth knows");
    }

    #[test]
    fn refuses_options_that_are_not_text() {
        use std::os::windows::ffi::OsStringExt;
        let broken = OsString::from_wide(&[0x2d, 0xd800]);
        assert!(parse_args([broken].into_iter()).is_err());
    }

    fn looping(args: &[&str]) -> loopback::Options {
        parse(args)
            .unwrap_or_else(|problem| panic!("{args:?}: {problem}"))
            .loopback
            .unwrap_or_else(|| panic!("{args:?}: no loopback"))
    }

    #[test]
    fn no_loopback_unless_asked_for() {
        assert!(parse(&[]).unwrap().loopback.is_none());
        assert!(parse(&["--log"]).unwrap().loopback.is_none());
        assert_eq!(looping(&["--loopback"]), loopback::Options::default());
        let defaults = loopback::Options::default();
        assert_eq!(defaults.source, loopback::Source::Screen(None));
        assert_eq!((defaults.fps, defaults.bitrate_mbits), (120, 15));
        assert_eq!(defaults.loss_percent, 0.0);
    }

    #[test]
    fn reads_every_loopback_option() {
        let args = parse(&[
            "--loopback",
            "--pattern",
            "1280x720",
            "--fps",
            "60",
            "--bitrate",
            "20",
            "--loss",
            "2.5",
            "--seed",
            "77",
            "--encoder",
            "Software",
            "--codec",
            "HEVC",
            "--seconds",
            "3",
            "--busy",
            "--delay",
            "2",
            "--jitter",
            "5.5",
            "--capacity",
            "2.5",
            "--lift",
            "10",
            "--profile",
            "p3loop",
            "--log",
        ])
        .unwrap();
        assert_eq!(args.profile.as_deref(), Some("p3loop"));
        assert!(args.log);
        assert_eq!(
            args.loopback,
            Some(loopback::Options {
                list: false,
                source: loopback::Source::Pattern(1280, 720),
                busy: true,
                fps: 60,
                bitrate_mbits: 20,
                loss_percent: 2.5,
                seed: Some(77),
                encoder: Some(encode::Kind::MfSoftware),
                codec: Some(encode::Codec::Hevc),
                seconds: Some(3),
                network: loopback::NetworkOptions {
                    delay_ms: 2.0,
                    jitter_ms: 5.5,
                    capacity_mbits: Some(2.5),
                    lift_seconds: Some(10),
                },
            })
        );
        let no_network = looping(&["--loopback", "--jitter", "0"]).network;
        assert_eq!(no_network, loopback::NetworkOptions::default());
        let options = looping(&["--monitor", "1", "--encoder", "nvenc", "--loopback"]);
        assert_eq!(options.source, loopback::Source::Screen(Some(1)));
        assert_eq!(options.encoder, Some(encode::Kind::Nvenc));
        assert_eq!(
            looping(&["--loopback", "--encoder", "hardware"]).encoder,
            Some(encode::Kind::MfHardware)
        );
        assert_eq!(
            looping(&["--loopback", "--codec", "h264"]).codec,
            Some(encode::Codec::H264)
        );
        // Auto is what a room does, and what no --codec means.
        assert_eq!(looping(&["--codec", "auto", "--loopback"]).codec, None);
        assert_eq!(looping(&["--loopback"]).codec, None);
        assert!(looping(&["--loopback", "--list"]).list);
        assert_eq!(looping(&["--loopback", "--loss", "0"]).loss_percent, 0.0);
        assert_eq!(
            looping(&["--loopback", "--loss", "100"]).loss_percent,
            100.0
        );
    }

    #[test]
    fn the_pattern_size_can_be_left_out() {
        let at_the_end = looping(&["--loopback", "--pattern"]);
        assert_eq!(at_the_end.source, loopback::Source::Pattern(2560, 1440));
        let before_another = looping(&["--loopback", "--pattern", "--fps", "30"]);
        assert_eq!(before_another.source, loopback::Source::Pattern(2560, 1440));
        assert_eq!(before_another.fps, 30);
    }

    // A super-wide monitor's 7680x2160 is the largest, for the loopback to
    // measure the 4096-wide path a share scales it to.
    #[test]
    fn pattern_sizes() {
        for (size, width, height) in [
            ("7680x2160", 7680, 2160),
            ("5120x1440", 5120, 1440),
            ("3840x2160", 3840, 2160),
            ("64x64", 64, 64),
        ] {
            assert_eq!(
                looping(&["--loopback", "--pattern", size]).source,
                loopback::Source::Pattern(width, height),
                "{size}"
            );
        }
        for size in ["7682x2160", "7680x2162", "62x64", "64x62", "7681x2160"] {
            assert_eq!(
                parse(&["--loopback", "--pattern", size]).err(),
                Some(format!(
                    "{size} is not a pattern size: give it as width x height, such as 1280x720, in even numbers from 64x64 to 7680x2160"
                )),
                "{size}"
            );
        }
    }

    #[test]
    fn bad_loopback_values_say_what_is_wrong() {
        let cases: &[(&[&str], &str)] = &[
            (
                &["--loopback", "--fps", "0"],
                "--fps needs a frame rate from 1 to 240 after it",
            ),
            (
                &["--loopback", "--fps", "241"],
                "--fps needs a frame rate from 1 to 240 after it",
            ),
            (
                &["--loopback", "--fps"],
                "--fps needs a frame rate from 1 to 240 after it",
            ),
            (
                &["--loopback", "--bitrate", "81"],
                "--bitrate needs a number of Mbit/s from 1 to 80 after it",
            ),
            (
                &["--loopback", "--bitrate", "7.5"],
                "--bitrate needs a number of Mbit/s from 1 to 80 after it",
            ),
            (
                &["--loopback", "--loss", "101"],
                "--loss needs a percentage from 0 to 100 after it, such as 5 or 2.5",
            ),
            (
                &["--loopback", "--loss", "-1"],
                "--loss needs a percentage from 0 to 100 after it, such as 5 or 2.5",
            ),
            (
                &["--loopback", "--loss", "5."],
                "--loss needs a percentage from 0 to 100 after it, such as 5 or 2.5",
            ),
            (
                &["--loopback", "--loss", "NaN"],
                "--loss needs a percentage from 0 to 100 after it, such as 5 or 2.5",
            ),
            (
                &["--loopback", "--seed", "x"],
                "--seed needs a whole number after it, as a loopback run prints it",
            ),
            (
                &["--loopback", "--seconds", "0"],
                "--seconds needs a number of seconds from 1 to 86400 after it",
            ),
            (
                &["--loopback", "--monitor", "-1"],
                "--monitor needs a monitor number after it, as booth --loopback --list shows them",
            ),
            (
                &["--loopback", "--encoder", "x264"],
                "there is no encoder called \"x264\": the choices are nvenc, hardware and software",
            ),
            (
                &["--loopback", "--encoder"],
                "--encoder needs nvenc, hardware or software after it",
            ),
            (
                &["--loopback", "--codec", "av1"],
                "there is no codec called \"av1\": the choices are h264, hevc and auto",
            ),
            (
                &["--loopback", "--codec"],
                "--codec needs h264, hevc or auto after it",
            ),
            (
                &["--loopback", "--codec", "--seconds", "2"],
                "--codec needs h264, hevc or auto after it",
            ),
            (
                &["--loopback", "--codec", "hevc", "--codec", "h264"],
                "--codec is given twice",
            ),
            (
                &["--loopback", "--pattern", "1281x720"],
                "1281x720 is not a pattern size: give it as width x height, such as 1280x720, in even numbers from 64x64 to 7680x2160",
            ),
            (
                &["--loopback", "--pattern", "7680x4320"],
                "7680x4320 is not a pattern size: give it as width x height, such as 1280x720, in even numbers from 64x64 to 7680x2160",
            ),
            (
                &["--loopback", "--pattern", "big"],
                "big is not a pattern size: give it as width x height, such as 1280x720, in even numbers from 64x64 to 7680x2160",
            ),
            (
                &["--loopback", "--monitor", "0", "--pattern"],
                "--monitor and --pattern do not go together: the pattern stands in for a monitor",
            ),
            (
                &["--loopback", "--fps", "60", "--fps", "30"],
                "--fps is given twice",
            ),
            (&["--loopback", "--loopback"], "--loopback is given twice"),
            (&["--loopback", "--list", "--list"], "--list is given twice"),
            (
                &["--loopback", "--busy"],
                "--busy works only with --pattern: the screen is shared as it is",
            ),
            (
                &["--loopback", "--pattern", "--busy", "--busy"],
                "--busy is given twice",
            ),
            (
                &["--loopback", "--delay", "1001"],
                "--delay needs milliseconds from 0 to 1000 after it, such as 2 or 5.5",
            ),
            (
                &["--loopback", "--jitter", "-3"],
                "--jitter needs milliseconds from 0 to 1000 after it, such as 2 or 5.5",
            ),
            (
                &["--loopback", "--jitter", "1", "--jitter", "2"],
                "--jitter is given twice",
            ),
            (
                &["--loopback", "--capacity", "0"],
                "--capacity needs a number of Mbit/s from 0.1 to 80 after it, such as 5 or 2.5",
            ),
            (
                &["--loopback", "--capacity", "1e3"],
                "--capacity needs a number of Mbit/s from 0.1 to 80 after it, such as 5 or 2.5",
            ),
            (
                &["--loopback", "--lift", "10"],
                "--lift needs --capacity: it frees the link after that many seconds",
            ),
            (
                &["--loopback", "--capacity", "5", "--lift", "0"],
                "--lift needs a number of seconds from 1 to 86400 after it",
            ),
        ];
        for (args, expected) in cases {
            assert_eq!(parse(args).err().as_deref(), Some(*expected), "{args:?}");
        }
    }

    #[test]
    fn loopback_and_room_options() {
        for args in [
            &["--loopback", "--port", "41010"][..],
            &["--port", "41010", "--loopback", "--pattern"],
        ] {
            assert_eq!(
                parse(args).err().as_deref(),
                Some("--port is for a room, and --loopback opens none"),
                "{args:?}"
            );
        }
        for (args, first) in [
            (&["--pattern"][..], "--pattern"),
            (&["--log", "--seconds", "5", "--fps", "60"], "--seconds"),
            (&["--list"], "--list"),
            (&["--encoder", "nvenc"], "--encoder"),
            (&["--codec", "hevc"], "--codec"),
            (&["--jitter", "6", "--capacity", "5"], "--jitter"),
            (&["--busy"], "--busy"),
        ] {
            assert_eq!(
                parse(args).err(),
                Some(format!("{first} works only with --loopback")),
                "{args:?}"
            );
        }
    }
}
