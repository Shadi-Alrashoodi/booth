use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use eframe::egui::text::LayoutJob;
use eframe::egui::{Color32, FontId, Label, Rect, ScrollArea, Sense, TextFormat, Ui, pos2, vec2};
use net::firewall::FirewallState;
use room::view::{
    AddressChange, ControlNumbers, Latency, Level, MappingWord, NameAnswer, NameView, Numbers,
    PresentPath, Role, RouterState, SharingNumbers, Source, SteppedDown, View, VoiceLoss,
    WatchingNumbers,
};
use voice::audio::Microphone;

use crate::controls;
use crate::messages;
use crate::screens::room::addresses_hidden;
use crate::sound::{Periods, Side};
use crate::strip;
use crate::theme::{self, ASH, BAD, CHALK, FIELD_GAP, PANEL, SIDE, STEP, WARN};

// Only there when the microphone makes voice worse by itself, then in warn.
const MICROPHONE: &str = "Microphone";

pub fn show(
    ui: &mut Ui,
    view: &View,
    log_file: Option<&Path>,
    firewall: &str,
    audio: Option<&Periods>,
    control_offered: bool,
) {
    // "none" would be a guess while the router is still being asked.
    let asking = view
        .invite
        .as_ref()
        .is_some_and(|invite| invite.router == RouterState::Testing);
    let now_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let showing = Showing {
        hide_addresses: addresses_hidden(&view.share),
        control: control_offered,
    };
    let mut out = build(
        view.role,
        &view.numbers,
        firewall,
        asking,
        now_unix,
        audio,
        showing,
    );
    if let Some(path) = log_file {
        out.group(None);
        out.add("Log file", Some(path.display().to_string()));
    }
    // Over the people list and the chat, the whole height between the title
    // row and the strip, in panel tone.
    let whole = ui.available_rect_before_wrap();
    ui.painter().rect_filled(whole, 0, PANEL);
    let font = theme::mono();
    // As wide as the widest unit on show, so that unit ends on the content's
    // right edge and the shorter ones start where it does; never narrower
    // than " ms", which the round trip always has.
    let gutter = out
        .lines
        .iter()
        .flat_map(|line| [Some(&line.value), line.under.as_ref()])
        .flatten()
        .map(|value| width(ui, &value[unit_at(value)..], &font))
        .fold(width(ui, " ms", &font), f32::max);
    let scroll = ScrollArea::vertical().id_salt("stats").auto_shrink(false);
    scroll.show(ui, |ui| {
        controls::gutter(ui, SIDE, SIDE, |ui| {
            for (i, line) in out.lines.iter().enumerate() {
                match out.start_at(i) {
                    Some(Start::Group(head)) => {
                        if i > 0 {
                            ui.add_space(FIELD_GAP);
                        }
                        if let Some(head) = head {
                            controls::text(ui, head, theme::section(), CHALK);
                            ui.add_space(STEP);
                        }
                    }
                    Some(Start::Part) => ui.add_space(STEP),
                    None => {}
                }
                let font = if line.name {
                    theme::body()
                } else {
                    theme::mono()
                };
                let color = value_color(line);
                value_line(ui, line.label, &line.value, &font, color, gutter);
                if let Some(under) = &line.under {
                    value_line(ui, "", under, &font, color, gutter);
                }
            }
        });
    });
}

// Label left in ash, value right. A reading's digits end on one edge, the
// gutter's width in from the right, and its unit stands in the gutter, its
// letters starting one space in, so the column lines up on the digits and
// the units whatever they are; a count with no unit ends on the same edge. Anything else, a word, a name or an address,
// ends on the content's right edge, where the widest units end.
fn value_line(ui: &mut Ui, label: &str, value: &str, font: &FontId, color: Color32, gutter: f32) {
    let rect = line_rect(ui);
    let label_width = controls::text_width(ui, label, theme::body());
    let least = rect.left() + label_width + SIDE;
    let unit = &value[unit_at(value)..];
    // A unit written against its digits, the % of "0.0%", starts where the
    // others start after their space, so it stands in their column; the
    // gap is layout, and the value still reads "0.0%".
    let gap = if on_digits(value) && !unit.is_empty() && !unit.starts_with(' ') {
        width(ui, " ", font)
    } else {
        0.0
    };
    let right = if on_digits(value) {
        rect.right() - gutter + gap + width(ui, unit, font)
    } else {
        rect.right()
    };
    let value = if gap > 0.0 {
        value.to_owned()
    } else {
        controls::fit_middle(ui, value, font, (right - least).max(0.0))
    };
    let value_width = controls::text_width(ui, &value, font.clone()) + gap;
    controls::split_row(
        ui,
        Rect::from_min_max(rect.min, pos2(right, rect.bottom())),
        value_width,
        |ui| {
            controls::one_line(ui, label, theme::body(), ASH);
        },
        |ui| {
            if gap > 0.0 {
                let format = TextFormat {
                    font_id: font.clone(),
                    color,
                    line_height: Some(controls::line_height(font)),
                    ..TextFormat::default()
                };
                let at = unit_at(&value);
                let mut job = LayoutJob::default();
                job.append(&value[..at], 0.0, format.clone());
                job.append(&value[at..], gap, format);
                ui.add(Label::new(job).truncate().show_tooltip_when_elided(false));
            } else {
                controls::one_line(ui, &value, font.clone(), color);
            }
        },
    );
}

// Measured as laid out, not rounded up, so units of different lengths still
// leave the digits on one edge.
fn width(ui: &Ui, text: &str, font: &FontId) -> f32 {
    ui.painter()
        .layout_no_wrap(text.to_owned(), font.clone(), CHALK)
        .size()
        .x
}

// A reading with its unit, or a count with none: the values whose digits
// line up in one column.
fn on_digits(value: &str) -> bool {
    let count = !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit());
    count || unit_at(value) < value.len()
}

// Where a reading's unit starts: at " ms" in "12.4 ms", at "%" in "0.0%".
// A value that is not a number with its unit has none, and this is its end.
fn unit_at(value: &str) -> usize {
    const UNITS: [&str; 8] = ["ms", "s", "min", "B", "KB", "MB", "kHz", "Mbit/s"];
    let ends_in_digit = |body: &str| body.ends_with(|c: char| c.is_ascii_digit());
    if let Some(body) = value.strip_suffix('%')
        && ends_in_digit(body)
    {
        return body.len();
    }
    match value.rsplit_once(' ') {
        Some((body, unit)) if UNITS.contains(&unit) && ends_in_digit(body) => body.len(),
        _ => value.len(),
    }
}

// Label left, number right. A number past its first threshold is in warn
// and past the second in bad, as in the strip; one within them is chalk,
// since sage in a column of numbers would make every line look alike.
#[derive(Clone, Debug, PartialEq)]
struct Line {
    label: &'static str,
    value: String,
    level: Level,
    // The value is "Hidden while you share" in place of an address.
    hidden: bool,
    // The value is a person's name, set in Plex Sans as names are
    // everywhere else, not in the mono of the readings.
    name: bool,
    // A second part of the value on the row under it, with no label of its
    // own, where the two side by side would not fit beside the label.
    under: Option<String>,
}

fn color(level: Level) -> Color32 {
    match level {
        Level::Good => CHALK,
        Level::Warn => WARN,
        Level::Bad => BAD,
    }
}

fn value_color(line: &Line) -> Color32 {
    if line.hidden { ASH } else { color(line.level) }
}

// Collects the lines, and leaves out what has no value.
struct Lines {
    lines: Vec<Line>,
    hide_addresses: bool,
    // Where each group, and each part of the link's group, starts, by the
    // index of its first line.
    starts: Vec<(usize, Start)>,
    // What the next line starts, when it is the first since a group or a
    // part began.
    next: Option<Start>,
}

// The groups from the top: the link, chat delivery, voice, video, remote
// control, what the router check found, then the log file. Chat delivery
// and the log file are one line each that names itself, so they have no
// head over them. The link is long enough to need its four parts set apart,
// by a smaller space and no head: how good it is, where it goes, the
// session, the traffic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Start {
    Group(Option<&'static str>),
    Part,
}

impl Lines {
    fn new(hide_addresses: bool) -> Lines {
        Lines {
            lines: Vec::new(),
            hide_addresses,
            starts: Vec::new(),
            next: None,
        }
    }

    // The next line starts a group under `head`. A group with no lines
    // shows nothing, its head included.
    fn group(&mut self, head: Option<&'static str>) {
        self.next = Some(Start::Group(head));
    }

    // The next line starts a part of the group it is in, unless it is the
    // group's first.
    fn part(&mut self) {
        if self.next.is_none() {
            self.next = Some(Start::Part);
        }
    }

    fn start_at(&self, i: usize) -> Option<Start> {
        self.starts
            .iter()
            .find(|(at, _)| *at == i)
            .map(|(_, start)| *start)
    }

    fn push(&mut self, line: Line) {
        if let Some(start) = self.next.take() {
            self.starts.push((self.lines.len(), start));
        }
        self.lines.push(line);
    }

    fn add(&mut self, label: &'static str, value: Option<String>) {
        self.level(label, value, Level::Good);
    }

    fn level(&mut self, label: &'static str, value: Option<String>, level: Level) {
        if let Some(value) = value {
            self.push(Line {
                label,
                value,
                level,
                hidden: false,
                name: false,
                under: None,
            });
        }
    }

    fn name(&mut self, label: &'static str, value: Option<String>) {
        if let Some(value) = value {
            self.push(Line {
                label,
                value,
                level: Level::Good,
                hidden: false,
                name: true,
                under: None,
            });
        }
    }

    // Two readings that belong together, the second on the row under the
    // first.
    fn two_rows(&mut self, label: &'static str, value: Option<(String, String)>) {
        if let Some((first, second)) = value {
            self.push(Line {
                label,
                value: first,
                level: Level::Good,
                hidden: false,
                name: false,
                under: Some(second),
            });
        }
    }

    // A line that can hold an IP address. The label stays while you share,
    // so the panel keeps its shape as a share starts and stops.
    fn address(&mut self, label: &'static str, value: Option<String>) {
        if !self.hide_addresses {
            self.add(label, value);
        } else if value.is_some() {
            self.push(Line {
                label,
                value: String::from(messages::HIDDEN_WHILE_SHARING),
                level: Level::Good,
                hidden: true,
                name: false,
                under: None,
            });
        }
    }
}

fn line_rect(ui: &mut Ui) -> Rect {
    let height = controls::line_height(&theme::body());
    ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::hover())
        .0
}

// What the lines leave to the rest of the panel: the addresses read hidden
// while you share, and remote control's lines show only while the panel
// offers control at all (control.rs).
#[derive(Clone, Copy, Debug)]
struct Showing {
    hide_addresses: bool,
    control: bool,
}

#[cfg(test)]
fn lines(
    role: Role,
    n: &Numbers,
    firewall: &str,
    asking: bool,
    now_unix: u64,
    audio: Option<&Periods>,
    showing: Showing,
) -> Vec<Line> {
    build(role, n, firewall, asking, now_unix, audio, showing).lines
}

// Only what has been measured: a number that is not there yet is left out
// rather than shown as a dash that looks like zero.
fn build(
    role: Role,
    n: &Numbers,
    firewall: &str,
    asking: bool,
    now_unix: u64,
    audio: Option<&Periods>,
    showing: Showing,
) -> Lines {
    let mut out = Lines::new(showing.hide_addresses);
    out.group(Some("Link"));
    let link_label = match role {
        Role::Host => "Slowest link",
        Role::Client => "Host",
    };
    out.name(link_label, n.link_name.clone());
    out.add("Round trip", n.rtt_ms.map(ms));
    out.add("Round trip average", n.rtt_avg_ms.map(ms));
    out.add("Round trip min", n.rtt_min_ms.map(ms));
    out.add("Round trip max", n.rtt_max_ms.map(ms));
    out.add("Round trip p95", n.rtt_p95_ms.map(ms));
    out.add(jitter_label(n.jitter_from), n.jitter_ms.map(ms));
    out.add(loss_label(n.loss_from), n.loss_pct.map(percent));
    out.add("Loss inbound, from pings", n.inbound_loss_pct.map(percent));
    out.add("Path", n.path.map(|path| strip::path_word(path).to_owned()));
    out.part();
    out.address("Remote address", n.peer_addr.map(|addr| addr.to_string()));
    out.add("Local port", Some(format!("UDP {}", n.local_port)));
    out.add("Firewall", Some(firewall.to_owned()));
    out.part();
    out.add("Handshake", n.handshake_ms.map(ms));
    out.add("Connect time", n.connect_ms.map(ms));
    out.add("Reconnect time", n.reconnect_ms.map(ms));
    out.add(
        "Clock offset",
        n.clock_offset_ms.map(|v| format!("{v:+.1} ms")),
    );
    out.add("Session age", n.session_age.map(age));
    out.add("Rekeys", Some(n.rekeys.to_string()));
    out.add(
        "Ping interval",
        Some(format!("{} ms", n.ping_interval.as_millis())),
    );
    out.part();
    out.add("Packets sent", Some(n.packets_sent.to_string()));
    out.add("Packets received", Some(n.packets_received.to_string()));
    out.add("Bytes sent", Some(bytes(n.bytes_sent)));
    out.add("Bytes received", Some(bytes(n.bytes_received)));
    out.add("Dropped, replayed", Some(n.dropped_replay.to_string()));
    out.add("Dropped, malformed", Some(n.dropped_bad.to_string()));
    // Only a host that has been under load has these, so a zero says nothing.
    out.add(
        "Cookie replies sent",
        (n.cookie_replies > 0).then(|| n.cookie_replies.to_string()),
    );
    out.add(
        "Dropped, no cookie",
        (n.dropped_no_cookie > 0).then(|| n.dropped_no_cookie.to_string()),
    );
    out.add("Ack delay", n.ack_delay_ms.map(ms));
    out.add("Retransmits", Some(n.retransmits.to_string()));
    out.group(None);
    out.add("Chat delivery", chat_delivery(n));
    out.group(Some("Voice"));
    // In and out on two rows: on one, with the far side's label beside
    // them, they do not fit the 360 px panel and are cut in the middle.
    out.two_rows("Audio period", audio_period(n, audio));
    out.add("Resampled by Windows", audio.and_then(resampled));
    out.level(MICROPHONE, n.microphone.and_then(microphone), Level::Warn);
    out.add("Render latency", n.render_latency_ms.map(ms));
    out.two_rows("Audio period, far side", far_period(n));
    out.add("Render latency, far side", n.far_render_latency_ms.map(ms));
    out.add("Frame size", frame_size(n));
    out.add(
        "Jitter buffer",
        n.buffer.as_ref().map(|b| {
            let name = messages::isolated(&b.name);
            format!("{name}, {} ms ({} frames)", b.ms, b.frames)
        }),
    );
    // A line for each person: two of them with the scattered part in
    // brackets would not fit the 360 px panel. The name goes with the
    // number, where a long one is cut in the middle to fit.
    for (name, loss) in &n.voice_loss {
        let name = messages::isolated(name);
        out.add("Voice loss", Some(format!("{name} {}", lost(*loss))));
    }
    out.add("Your voice, worst loss", n.own_voice_loss.map(lost));
    if let Some(heard) = &n.mouth_to_ear {
        let about = if heard.about { "about " } else { "" };
        out.add(
            "Mouth to ear",
            Some(format!(
                "{about}{:.1} ms, from {}",
                heard.last_ms, heard.name
            )),
        );
        out.add(
            "Mouth to ear, 10 s",
            Some(format!(
                "{about}{:.1} ms average, p95 {:.1} ms",
                heard.avg_ms, heard.p95_ms
            )),
        );
    }
    // Only a friend's program sends these, so a zero says nothing.
    out.add(
        "Dropped, voice",
        (n.voice_dropped > 0).then(|| n.voice_dropped.to_string()),
    );
    // The Video group follows the voice lines.
    out.group(Some("Video"));
    if let Some(sharing) = &n.sharing {
        video_out(&mut out, sharing);
    }
    if let Some(watching) = &n.watching {
        video_in(&mut out, watching);
    }
    video_counts(&mut out, n);
    out.group(None);
    if showing.control {
        control(&mut out, &n.control);
    }
    out.group(Some("Router check"));
    // What STUN found about the outside port, easy or hard. Not "Port
    // mapping" below, which is the router opening a port on request.
    out.add(
        "NAT mapping",
        n.mapping.map(|word| mapping(word).to_owned()),
    );
    out.address("Public address", n.public_addr.map(|addr| addr.to_string()));
    let mapped_by = match (role, &n.mapping_protocol) {
        (Role::Host, Some(protocol)) => Some(protocol.clone()),
        (Role::Host, None) if !asking => Some(String::from("none")),
        _ => None,
    };
    out.add("Port mapping", mapped_by);
    out.address("Mapped address", n.mapped_addr.map(|addr| addr.to_string()));
    if let Some(change) = &n.address_change {
        out.add("Address changed", Some(changed(change, now_unix)));
        out.address("Old address", Some(change.from.to_string()));
        out.address("New address", Some(change.to.to_string()));
    }
    if let Some(name) = &n.address_name {
        out.add("Address name", Some(name.name.clone()));
        // Whatever the lookup said: its answer names the addresses it
        // refused as well as the ones it took.
        out.address("Name points to", points_to(name));
    }
    out
}

// While this PC's share runs: what its encoder makes and what goes out.
fn video_out(out: &mut Lines, s: &SharingNumbers) {
    // The software encoder is in warn, since it caps the share at 1080p60.
    let encoder = if s.software { Level::Warn } else { Level::Good };
    out.level("Encoder", Some(s.encoder.clone()), encoder);
    out.level(
        "Video size",
        Some(format!("{}x{}, {} fps", s.width, s.height, s.fps)),
        s.fps_level,
    );
    // Fewer than the frame rate on a still screen, and none while nobody
    // watches.
    out.add(
        "Encoded, last second",
        Some(format!("{} frames", s.encoded_fps)),
    );
    if let Some(encode) = s.encode_ms {
        let level = frame_level(encode.median_ms, s.fps);
        out.level("Encode", Some(latency(encode)), level);
    }
    // The setting is a ceiling the encoder stays under: what went out
    // against the rate the bitrate rule uses now, and what it allows while a
    // backoff holds the rate under that.
    out.add(
        "Video bitrate",
        Some(format!(
            "{} of {} Mbit/s",
            mbits(s.video_kbps),
            mbits(s.rate_kbps)
        )),
    );
    out.add(
        "Rate allowed",
        (s.allowed_kbps != s.rate_kbps).then(|| format!("{} Mbit/s", mbits(s.allowed_kbps))),
    );
    out.add("Backoffs", (s.backoffs > 0).then(|| s.backoffs.to_string()));
    out.add("Parity", Some(format!("{}%", s.parity_pct)));
    out.add("IDRs, last minute", Some(s.idrs_last_minute.to_string()));
    out.add(
        "Invalidations, last minute",
        Some(s.invalidations_last_minute.to_string()),
    );
    // Only a busy PC lets frames go, so a zero says nothing.
    out.add(
        "Frames let go",
        (s.let_go > 0).then(|| s.let_go.to_string()),
    );
    out.level(
        "Stepped down",
        s.stepped_down.map(stepped_down),
        Level::Warn,
    );
    // Voice and control included, so the setting can leave room for them.
    out.add(
        "Total upload",
        Some(format!("{} Mbit/s", mbits(s.upload_kbps))),
    );
}

// While this PC watches: how the share arrives and how fast it is shown.
fn video_in(out: &mut Lines, w: &WatchingNumbers) {
    // The codec goes with the decode time, which it sets: at 1440p HEVC
    // decodes in about half the time H.264 does.
    let decode = match (w.codec, w.decode_ms) {
        (Some(codec), Some(value)) => Some(format!("{codec}, {}", ms(value))),
        (Some(codec), None) => Some(codec.to_string()),
        (None, value) => value.map(ms),
    };
    out.level("Decode, GPU", decode, w.decode_level);
    out.level(
        "Capture to display",
        w.capture_to_display.map(latency),
        w.capture_to_display_level,
    );
    out.add("Shown, last second", Some(format!("{} frames", w.fps)));
    let loss = w.video_loss_pct.map_or(Level::Good, loss_level);
    out.level("Video loss", w.video_loss_pct.map(percent), loss);
    out.add("Frames repaired", Some(w.repaired.to_string()));
    out.add("Frames dropped", Some(w.dropped.to_string()));
    out.add(
        "Frames not decoded",
        (w.decode_failed > 0).then(|| w.decode_failed.to_string()),
    );
    out.add(
        "Frames before the first IDR",
        (w.before_first_idr > 0).then(|| w.before_first_idr.to_string()),
    );
    out.add("Present path", w.present_path.map(present_path));
    // Set with --video-loss for a test with a friend.
    out.add(
        "Loss knob",
        w.knob.map(|knob| {
            format!(
                "{}%, seed {}, {} dropped",
                knob.percent, knob.seed, knob.dropped
            )
        }),
    );
}

// Counters that only a friend's program, a busy PC or a stalled viewer make
// count, so a zero says nothing.
fn video_counts(out: &mut Lines, n: &Numbers) {
    let count = |value: u64| (value > 0).then(|| value.to_string());
    out.add("Video packets sent", count(n.video_sent));
    out.add("Video packets passed on", count(n.video_relayed));
    out.add("Dropped, over sharing limits", count(n.video_dropped));
    out.add("Dropped, viewer behind", count(n.video_overflow));
}

// The goal for remote control: under 5 ms on the PC controlled, from the
// packet's arrival to the injector's call.
const INJECT_TARGET_MS: f32 = 5.0;

// Remote control, both ways, once there is something to show: the
// latencies only on the PC controlled, the drops by why, and none of them
// while they are zero.
fn control(out: &mut Lines, c: &ControlNumbers) {
    let count = |value: u64| (value > 0).then(|| value.to_string());
    out.add("Capture to inject", c.capture_to_inject.map(latency));
    let level = match c.receive_to_inject {
        Some(took) if took.median_ms > INJECT_TARGET_MS => Level::Warn,
        _ => Level::Good,
    };
    out.level("Receive to inject", c.receive_to_inject.map(latency), level);
    out.add("Inject call", c.inject_call.map(latency));
    out.add("Input events injected", count(c.injected));
    out.add("Input packets sent", count(c.packets_sent));
    let d = &c.dropped;
    out.add("Dropped, blocked input", count(d.blocked));
    out.add("Dropped, keys over the rate", count(d.over_rate));
    out.add("Dropped, local input first", count(d.local));
    out.add("Dropped, after the panic key", count(d.cut));
    out.add("Dropped, administrator window", count(d.paused));
    out.add("Dropped, late input", count(d.late));
    out.add("Dropped, input waited too long", count(d.stale));
    out.add("Dropped, input over limits", count(d.host_over_rate));
    out.add("Control cutoffs", count(c.cutoffs));
}

fn latency(latency: Latency) -> String {
    let about = if latency.about { "about " } else { "" };
    format!(
        "{about}{:.1} ms, p95 {:.1} ms",
        latency.median_ms, latency.p95_ms
    )
}

// Encode and decode are good under one frame interval and in warn above
// it. There is no bad for them.
fn frame_level(ms: f32, fps: u32) -> Level {
    if fps > 0 && ms > 1000.0 / fps as f32 {
        Level::Warn
    } else {
        Level::Good
    }
}

// The strip's loss thresholds.
fn loss_level(pct: f32) -> Level {
    match stats::Thresholds::default().loss_level(pct) {
        stats::Level::Good => Level::Good,
        stats::Level::Warn => Level::Warn,
        stats::Level::Bad => Level::Bad,
    }
}

fn mbits(kbps: u32) -> String {
    format!("{:.1}", f64::from(kbps) / 1000.0)
}

fn stepped_down(why: SteppedDown) -> String {
    String::from(match why {
        SteppedDown::LowRate => "to 1080p60, rate under 8 Mbit/s",
        SteppedDown::SlowEncode => "to 1080p60, encode too slow",
    })
}

fn present_path(path: PresentPath) -> String {
    String::from(match path {
        PresentPath::Flip => "flip",
        PresentPath::Composed => "composed",
    })
}

// The strip's own two numbers say where they came from, since the same place
// on the strip shows voice while it flows and pings when it does not.
fn jitter_label(from: Source) -> &'static str {
    match from {
        Source::Voice => "Jitter, from voice",
        Source::Pings => "Jitter, from pings",
        Source::Video => "Jitter, from video",
    }
}

fn loss_label(from: Source) -> &'static str {
    match from {
        Source::Voice => "Loss, from voice",
        Source::Pings => "Loss, from pings",
        Source::Video => "Loss, from video",
    }
}

// The host's own dynamic DNS client keeps the name current, not Booth, so the
// host is shown whether it has.
fn points_to(name: &NameView) -> Option<String> {
    if let Some(check) = name.outside {
        return Some(if check.is_this_pc() {
            format!("{}, this PC", check.points_to)
        } else {
            format!(
                "{}, not this PC's outside address {}",
                check.points_to, check.outside
            )
        });
    }
    let text = match &name.answer {
        NameAnswer::NotAsked => return None,
        NameAnswer::Looking => String::from("looking it up"),
        NameAnswer::Found { addrs, refused } => {
            let taken = addrs.iter().map(ToString::to_string);
            let left_out = refused
                .iter()
                .map(|(ip, why)| format!("{ip} not used: {why}"));
            taken.chain(left_out).collect::<Vec<_>>().join(", ")
        }
        NameAnswer::NoSuchName => String::from("no such name"),
        NameAnswer::NoAddress => String::from("no address"),
        NameAnswer::Unanswered => String::from("no answer"),
    };
    Some(text)
}

// On the host it is always its own address. A client says whose, since it
// sees both: its own router's, and the host it followed.
fn changed(change: &AddressChange, now_unix: u64) -> String {
    let whose = if change.this_pc {
        "this PC"
    } else {
        "the host"
    };
    let ago = Duration::from_secs(now_unix.saturating_sub(change.at_unix));
    format!("{whose}, {} ago", age(ago))
}

fn ms(value: f32) -> String {
    format!("{value:.1} ms")
}

// The newest and the average over the last minute, once a message from
// someone else has been timed. "about" when the link was jittery enough for
// the clock offset behind it to be off by as much.
fn chat_delivery(n: &Numbers) -> Option<String> {
    let last = n.chat_delivery_last_ms?;
    let about = if n.chat_delivery_about { "about " } else { "" };
    Some(match n.chat_delivery_avg_ms {
        Some(avg) => format!("{about}{last:.1} ms, average {avg:.1} ms"),
        None => format!("{about}{last:.1} ms"),
    })
}

// This PC's side: what the room's own streams opened, and where one is
// closed (a muted microphone) what the device said when asked.
fn audio_period(n: &Numbers, probed: Option<&Periods>) -> Option<(String, String)> {
    let side = |opened: Option<f32>, probed: Option<&Side>| match (opened, probed) {
        (Some(value), _) => Some(ms(value)),
        (None, Some(Side::Period { ms: value, .. })) => Some(ms(*value as f32)),
        (None, Some(Side::NoDevice)) => Some(String::from("none")),
        (None, Some(Side::NotKnown)) | (None, None) => None,
    };
    let input = side(n.audio_in_ms, probed.map(|p| &p.input));
    let output = side(n.audio_out_ms, probed.map(|p| &p.output));
    if input.is_none() && output.is_none() && probed.is_none() {
        return None;
    }
    let known = |side: Option<String>| side.unwrap_or_else(|| String::from("not known"));
    Some((
        format!("in {}", known(input)),
        format!("out {}", known(output)),
    ))
}

// The far side's, as it told the host or the host told this PC.
fn far_period(n: &Numbers) -> Option<(String, String)> {
    if n.far_audio_in_ms.is_none() && n.far_audio_out_ms.is_none() {
        return None;
    }
    let side = |value: Option<f32>| value.map_or_else(|| String::from("none"), ms);
    Some((
        format!("in {}", side(n.far_audio_in_ms)),
        format!("out {}", side(n.far_audio_out_ms)),
    ))
}

// Voice goes in 5 ms frames, with the copy of the one before while loss is
// reported, or in 10 ms frames with Opus's own repair data.
fn frame_size(n: &Numbers) -> Option<String> {
    match (n.send_frame_ms, n.send_repair_copy) {
        (0, _) => None,
        (ms, true) => Some(format!("{ms} ms with repair copy")),
        (ms, false) => Some(format!("{ms} ms")),
    }
}

// All of it, and in brackets the part lost one or two frames at a time,
// which is what turns the repair copy and 10 ms frames on.
fn lost(loss: VoiceLoss) -> String {
    if loss.scattered_pct > 0.0 {
        format!(
            "{} ({} scattered)",
            percent(loss.all_pct),
            percent(loss.scattered_pct)
        )
    } else {
        percent(loss.all_pct)
    }
}

// A line of its own: written after the periods, it would not fit the 360 px
// panel.
fn resampled(periods: &Periods) -> Option<String> {
    let by_windows = |side: &Side| {
        matches!(
            side,
            Side::Period {
                resampled: true,
                ..
            }
        )
    };
    let which = match (by_windows(&periods.input), by_windows(&periods.output)) {
        (true, true) => "input and output",
        (true, false) => "input",
        (false, true) => "output",
        (false, false) => return None,
    };
    Some(String::from(which))
}

// The same case as the warning under the input device in settings.
fn microphone(mic: Microphone) -> Option<String> {
    if !mic.warns() {
        return None;
    }
    let rate = messages::khz(mic.rate);
    Some(if mic.hands_free {
        format!("{rate} kHz, Bluetooth hands-free")
    } else {
        format!("{rate} kHz")
    })
}

fn percent(value: f32) -> String {
    format!("{value:.1}%")
}

fn age(age: Duration) -> String {
    let secs = age.as_secs();
    match (secs / 3600, secs / 60 % 60, secs % 60) {
        (0, 0, s) => format!("{s} s"),
        (0, m, s) => format!("{m} min {s} s"),
        (h, m, _) => format!("{h} h {m} min"),
    }
}

fn bytes(count: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * 1024;
    match count {
        0..KB => format!("{count} B"),
        KB..MB => format!("{:.1} KB", count as f64 / KB as f64),
        _ => format!("{:.1} MB", count as f64 / MB as f64),
    }
}

pub fn firewall(state: Option<&FirewallState>) -> &'static str {
    match state {
        Some(FirewallState::Allowed(_)) => "allowed",
        Some(FirewallState::Blocked(_) | FirewallState::BlockingAll(_)) => "blocked",
        Some(FirewallState::Missing(_)) => "no rule",
        Some(FirewallState::Unknown(_)) | None => "not known",
    }
}

fn mapping(word: MappingWord) -> &'static str {
    match word {
        MappingWord::Easy => "easy",
        MappingWord::Hard => "hard",
        MappingWord::Unknown => "not known",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use net::firewall::Profiles;
    use room::view::{Codec, KnobNumbers, NameMatch};
    use std::net::Ipv4Addr;

    // As the panel shows the lines outside a share, once it offers control.
    const SHOWING: Showing = Showing {
        hide_addresses: false,
        control: true,
    };

    // The words alone, for the tests about what a line says.
    fn pairs(
        role: Role,
        n: &Numbers,
        firewall: &str,
        asking: bool,
        now_unix: u64,
        audio: Option<&Periods>,
    ) -> Vec<(&'static str, String)> {
        lines(role, n, firewall, asking, now_unix, audio, SHOWING)
            .into_iter()
            .map(|line| match line.under {
                Some(under) => (line.label, format!("{}\n{under}", line.value)),
                None => (line.label, line.value),
            })
            .collect()
    }

    fn sharing() -> SharingNumbers {
        SharingNumbers {
            encoder: String::from("NVENC H.264"),
            software: false,
            width: 2560,
            height: 1440,
            fps: 120,
            fps_level: Level::Good,
            encoded_fps: 118,
            encode_ms: Some(Latency {
                median_ms: 2.61,
                p95_ms: 3.04,
                about: false,
            }),
            video_kbps: 11_240,
            rate_kbps: 15_000,
            allowed_kbps: 15_000,
            encoder_kbps: 15_000,
            backoffs: 0,
            parity_pct: 10,
            idrs_last_minute: 2,
            invalidations_last_minute: 5,
            idrs: 3,
            let_go: 0,
            idrs_let_go: 0,
            stepped_down: None,
            upload_kbps: 12_960,
        }
    }

    fn watching() -> WatchingNumbers {
        WatchingNumbers {
            decode_ms: Some(1.42),
            decode_level: Level::Good,
            capture_to_display: Some(Latency {
                median_ms: 11.24,
                p95_ms: 13.0,
                about: true,
            }),
            capture_to_display_level: Level::Good,
            fps: 119,
            video_loss_pct: Some(0.0),
            shown: 7_000,
            repaired: 4,
            dropped: 1,
            decode_failed: 0,
            before_first_idr: 0,
            present_path: Some(PresentPath::Flip),
            knob: None,
            codec: Some(Codec::Hevc),
        }
    }

    fn video(n: &Numbers) -> Vec<Line> {
        let voice_ends = |line: &Line| line.label == "NAT mapping";
        lines(Role::Client, n, "allowed", false, 0, None, SHOWING)
            .into_iter()
            .skip_while(|line| line.label != "Encoder" && line.label != "Decode, GPU")
            .take_while(|line| !voice_ends(line))
            .collect()
    }

    fn line(label: &'static str, value: &str, level: Level) -> Line {
        Line {
            label,
            value: value.to_owned(),
            level,
            hidden: false,
            name: false,
            under: None,
        }
    }

    fn control_lines(c: &ControlNumbers) -> Vec<Line> {
        let mut out = Lines::new(false);
        control(&mut out, c);
        out.lines
    }

    // The inject numbers on the PC controlled, receive to inject in warn
    // past 5 ms, and each drop by why, none of them before there is anything
    // to count.
    #[test]
    fn control_lines_once_measured() {
        assert!(control_lines(&ControlNumbers::default()).is_empty());
        let took = |median_ms, p95_ms, about| {
            Some(Latency {
                median_ms,
                p95_ms,
                about,
            })
        };
        let mut controlled = ControlNumbers {
            capture_to_inject: took(3.24, 4.8, true),
            receive_to_inject: took(0.41, 0.9, false),
            inject_call: took(0.05, 0.12, false),
            injected: 5_120,
            cutoffs: 1,
            ..ControlNumbers::default()
        };
        controlled.dropped.over_rate = 3;
        controlled.dropped.cut = 2;
        let good = Level::Good;
        assert_eq!(
            control_lines(&controlled),
            [
                line("Capture to inject", "about 3.2 ms, p95 4.8 ms", good),
                line("Receive to inject", "0.4 ms, p95 0.9 ms", good),
                line("Inject call", "0.1 ms, p95 0.1 ms", good),
                line("Input events injected", "5120", good),
                line("Dropped, keys over the rate", "3", good),
                line("Dropped, after the panic key", "2", good),
                line("Control cutoffs", "1", good),
            ]
        );
        let slow = ControlNumbers {
            receive_to_inject: took(6.5, 9.0, false),
            ..ControlNumbers::default()
        };
        assert_eq!(
            control_lines(&slow),
            [line("Receive to inject", "6.5 ms, p95 9.0 ms", Level::Warn)]
        );
        // The controller's side has only what it sent.
        let controller = ControlNumbers {
            packets_sent: 812,
            ..ControlNumbers::default()
        };
        assert_eq!(
            control_lines(&controller),
            [line("Input packets sent", "812", good)]
        );
    }

    // With the switch off the panel has no control lines, whatever the
    // numbers hold, and every other line stays as it was.
    #[test]
    fn no_control_lines_while_control_is_not_offered() {
        let mut n = Numbers::default();
        n.control.receive_to_inject = Some(Latency {
            median_ms: 0.41,
            p95_ms: 0.9,
            about: false,
        });
        n.control.injected = 5_120;
        n.control.packets_sent = 812;
        n.control.dropped.cut = 2;
        n.control.cutoffs = 1;
        let controls: Vec<&str> = control_lines(&n.control)
            .into_iter()
            .map(|line| line.label)
            .collect();
        assert_eq!(controls.len(), 5, "{controls:?}");
        let labels = |control| -> Vec<&str> {
            let showing = Showing { control, ..SHOWING };
            lines(Role::Host, &n, "allowed", false, 0, None, showing)
                .into_iter()
                .map(|line| line.label)
                .collect()
        };
        let (on, off) = (labels(true), labels(false));
        for label in &controls {
            assert!(on.contains(label), "{label}");
            assert!(!off.contains(label), "{label}");
        }
        let rest: Vec<&str> = on
            .into_iter()
            .filter(|label| !controls.contains(label))
            .collect();
        assert_eq!(rest, off);
    }

    // While you share, the encoder, frame size and fps, encode ms, the
    // bitrate against the rate in use, parity, IDRs and invalidations, all
    // after the voice lines.
    #[test]
    fn video_group_while_sharing() {
        let n = Numbers {
            voice_dropped: 2,
            sharing: Some(sharing()),
            ..Numbers::default()
        };
        let all = pairs(Role::Client, &n, "allowed", false, 0, None);
        let at = |label| all.iter().position(|(l, _)| *l == label).unwrap();
        assert_eq!(at("Encoder"), at("Dropped, voice") + 1);
        let good = Level::Good;
        assert_eq!(
            video(&n),
            [
                line("Encoder", "NVENC H.264", good),
                line("Video size", "2560x1440, 120 fps", good),
                line("Encoded, last second", "118 frames", good),
                line("Encode", "2.6 ms, p95 3.0 ms", good),
                line("Video bitrate", "11.2 of 15.0 Mbit/s", good),
                line("Parity", "10%", good),
                line("IDRs, last minute", "2", good),
                line("Invalidations, last minute", "5", good),
                line("Total upload", "13.0 Mbit/s", good),
            ]
        );
    }

    // Past a threshold a number is in warn or bad, as in the strip, and a
    // share that had to give something up says so in warn.
    #[test]
    fn video_thresholds() {
        let slow = SharingNumbers {
            encoder: String::from("Microsoft H.264 software encoder"),
            software: true,
            width: 1920,
            height: 1080,
            fps: 60,
            fps_level: Level::Warn,
            encode_ms: Some(Latency {
                median_ms: 19.0,
                p95_ms: 24.0,
                about: false,
            }),
            rate_kbps: 12_000,
            backoffs: 1,
            let_go: 7,
            stepped_down: Some(SteppedDown::SlowEncode),
            ..sharing()
        };
        let n = Numbers {
            sharing: Some(slow),
            ..Numbers::default()
        };
        let lines = video(&n);
        let find = |label| lines.iter().find(|line| line.label == label).unwrap();
        assert_eq!(find("Encoder").level, Level::Warn);
        assert_eq!(find("Video size").level, Level::Warn);
        assert_eq!(find("Encode").level, Level::Warn);
        assert_eq!(
            *find("Stepped down"),
            line("Stepped down", "to 1080p60, encode too slow", Level::Warn)
        );
        assert_eq!(find("Rate allowed").value, "15.0 Mbit/s");
        assert_eq!(find("Backoffs").value, "1");
        assert_eq!(find("Frames let go").value, "7");

        let lossy = WatchingNumbers {
            decode_level: Level::Warn,
            capture_to_display_level: Level::Bad,
            video_loss_pct: Some(3.5),
            ..watching()
        };
        let n = Numbers {
            watching: Some(lossy),
            ..Numbers::default()
        };
        let lines = video(&n);
        let find = |label| lines.iter().find(|line| line.label == label).unwrap();
        assert_eq!(find("Decode, GPU").level, Level::Warn);
        assert_eq!(find("Capture to display").level, Level::Bad);
        assert_eq!(find("Video loss").level, Level::Bad);
        assert_eq!(color(Level::Bad), BAD);
        assert_eq!(color(Level::Warn), WARN);
        assert_eq!(color(Level::Good), CHALK);
    }

    // While you watch: decode on the GPU, capture to display with "about"
    // when the clock offset is uncertain, loss, repaired and dropped, and
    // the present path.
    #[test]
    fn video_group_while_watching() {
        let knob = KnobNumbers {
            percent: 5.0,
            seed: 1234,
            dropped: 312,
        };
        let n = Numbers {
            watching: Some(WatchingNumbers {
                knob: Some(knob),
                ..watching()
            }),
            video_overflow: 9,
            ..Numbers::default()
        };
        let good = Level::Good;
        assert_eq!(
            video(&n),
            [
                line("Decode, GPU", "HEVC, 1.4 ms", good),
                line("Capture to display", "about 11.2 ms, p95 13.0 ms", good),
                line("Shown, last second", "119 frames", good),
                line("Video loss", "0.0%", good),
                line("Frames repaired", "4", good),
                line("Frames dropped", "1", good),
                line("Present path", "flip", good),
                line("Loss knob", "5%, seed 1234, 312 dropped", good),
                line("Dropped, viewer behind", "9", good),
            ]
        );
        // Before the GPU's first time is in, the codec alone.
        let early = Numbers {
            watching: Some(WatchingNumbers {
                decode_ms: None,
                codec: Some(Codec::H264),
                ..watching()
            }),
            ..Numbers::default()
        };
        assert_eq!(video(&early)[0], line("Decode, GPU", "H.264", good));
    }

    #[test]
    fn no_video_lines_before_anything_was_shared_or_watched() {
        let none = Numbers::default();
        let labels: Vec<&str> = lines(Role::Host, &none, "allowed", false, 0, None, SHOWING)
            .into_iter()
            .map(|line| line.label)
            .collect();
        for label in ["Encoder", "Decode, GPU", "Video packets sent", "Video loss"] {
            assert!(!labels.contains(&label), "{label}");
        }
    }

    // Each group under its head, the link's parts set apart with none, and
    // nothing at all for a group with no lines.
    #[test]
    fn groups_and_their_heads() {
        let n = Numbers {
            rtt_ms: Some(4.2),
            local_port: 41000,
            chat_delivery_last_ms: Some(3.0),
            send_frame_ms: 5,
            ..Numbers::default()
        };
        let out = build(Role::Host, &n, "allowed", false, 0, None, SHOWING);
        let starts: Vec<(&str, Start)> = out
            .starts
            .iter()
            .map(|(at, start)| (out.lines[*at].label, *start))
            .collect();
        assert_eq!(
            starts,
            [
                ("Round trip", Start::Group(Some("Link"))),
                ("Local port", Start::Part),
                ("Rekeys", Start::Part),
                ("Packets sent", Start::Part),
                ("Chat delivery", Start::Group(None)),
                ("Frame size", Start::Group(Some("Voice"))),
                ("Port mapping", Start::Group(Some("Router check"))),
            ]
        );
    }

    // Only a number and its unit are split, so the digits can end on one
    // edge; words, an address or a count with a word after it stay whole.
    #[test]
    fn readings_split_at_their_unit() {
        let split = |value: &'static str| value.split_at(unit_at(value));
        assert_eq!(split("12.4 ms"), ("12.4", " ms"));
        assert_eq!(split("-1.6 ms"), ("-1.6", " ms"));
        assert_eq!(split("0.0%"), ("0.0", "%"));
        assert_eq!(split("1023 B"), ("1023", " B"));
        assert_eq!(split("2 min 13 s"), ("2 min 13", " s"));
        assert_eq!(split("3 h 7 min"), ("3 h 7", " min"));
        assert_eq!(split("11.2 of 15.0 Mbit/s"), ("11.2 of 15.0", " Mbit/s"));
        assert_eq!(
            split("about 3.1 ms, average 2.7 ms"),
            ("about 3.1 ms, average 2.7", " ms")
        );
        for whole in [
            "30",
            "UDP 41000",
            "LAN",
            "192.0.2.44:41000",
            "Hidden while you share",
            "118 frames",
            "5%, seed 1234, 312 dropped",
            "8 kHz, Bluetooth hands-free",
        ] {
            assert_eq!(split(whole), (whole, ""), "{whole}");
        }
    }

    // Readings and counts end on the digit column; words, names and
    // addresses end on the right edge, where the widest units end.
    #[test]
    fn only_readings_and_counts_end_on_the_digits() {
        for reading in ["12.4 ms", "0.0%", "31", "0", "in 10.0 ms", "2 min 13 s"] {
            assert!(on_digits(reading), "{reading}");
        }
        for word in [
            "Shadi",
            "LAN",
            "allowed",
            "input",
            "UDP 41801",
            "192.0.2.44:41000",
            "in none",
            "Hidden while you share",
        ] {
            assert!(!on_digits(word), "{word}");
        }
    }

    // The host's name is a name like any other, in Plex Sans.
    #[test]
    fn the_host_is_named_in_sans() {
        let n = Numbers {
            link_name: Some(String::from("Ines")),
            ..Numbers::default()
        };
        let host = lines(Role::Client, &n, "allowed", false, 0, None, SHOWING)
            .into_iter()
            .find(|line| line.label == "Host")
            .expect("a Host line");
        assert!(host.name);
        let round_trip = Numbers {
            rtt_ms: Some(4.0),
            ..Numbers::default()
        };
        let lines = lines(
            Role::Client,
            &round_trip,
            "allowed",
            false,
            0,
            None,
            SHOWING,
        );
        assert!(lines.iter().all(|line| !line.name));
    }

    #[test]
    fn ages_read_in_the_largest_units() {
        assert_eq!(age(Duration::from_secs(9)), "9 s");
        assert_eq!(age(Duration::from_secs(133)), "2 min 13 s");
        assert_eq!(age(Duration::from_secs(3 * 3600 + 7 * 60 + 5)), "3 h 7 min");
    }

    #[test]
    fn byte_counts_carry_a_unit() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(1023), "1023 B");
        assert_eq!(bytes(1536), "1.5 KB");
        assert_eq!(bytes(3 * 1024 * 1024), "3.0 MB");
    }

    #[test]
    fn leaves_out_what_is_not_measured() {
        let numbers = Numbers {
            local_port: 41000,
            ping_interval: Duration::from_secs(1),
            ..Numbers::default()
        };
        let lines = pairs(Role::Host, &numbers, "no rule", true, 0, None);
        let labels: Vec<&str> = lines.iter().map(|(label, _)| *label).collect();
        assert!(!labels.contains(&"Round trip"));
        assert!(!labels.contains(&"Reconnect time"));
        assert!(!labels.contains(&"Address changed"));
        assert!(!labels.contains(&"Slowest link"));
        assert!(!labels.contains(&"Port mapping"));
        assert!(!labels.contains(&"Mapped address"));
        assert_eq!(lines[0], ("Local port", String::from("UDP 41000")));
        assert_eq!(lines[1], ("Firewall", String::from("no rule")));
    }

    #[test]
    fn cookie_lines_only_after_the_host_was_under_load() {
        let cookie_lines = |numbers: &Numbers| -> Vec<(&'static str, String)> {
            pairs(Role::Host, numbers, "allowed", false, 0, None)
                .into_iter()
                .filter(|(label, _)| matches!(*label, "Cookie replies sent" | "Dropped, no cookie"))
                .collect()
        };
        assert!(cookie_lines(&Numbers::default()).is_empty());
        let loaded = Numbers {
            cookie_replies: 212,
            dropped_no_cookie: 4031,
            ..Numbers::default()
        };
        assert_eq!(
            cookie_lines(&loaded),
            [
                ("Cookie replies sent", String::from("212")),
                ("Dropped, no cookie", String::from("4031")),
            ]
        );
        // Every one of them was rate limited, so no reply went out.
        let held = Numbers {
            dropped_no_cookie: 9,
            ..Numbers::default()
        };
        assert_eq!(
            cookie_lines(&held),
            [("Dropped, no cookie", String::from("9"))]
        );
    }

    #[test]
    fn chat_delivery_shows_once_a_message_was_timed() {
        let value = |numbers: &Numbers| {
            pairs(Role::Client, numbers, "allowed", false, 0, None)
                .into_iter()
                .find(|(label, _)| *label == "Chat delivery")
                .map(|(_, value)| value)
        };
        assert_eq!(value(&Numbers::default()), None);
        let timed = Numbers {
            chat_delivery_last_ms: Some(3.06),
            chat_delivery_avg_ms: Some(2.71),
            ..Numbers::default()
        };
        assert_eq!(value(&timed).as_deref(), Some("3.1 ms, average 2.7 ms"));
        let jittery = Numbers {
            chat_delivery_about: true,
            ..timed.clone()
        };
        assert_eq!(
            value(&jittery).as_deref(),
            Some("about 3.1 ms, average 2.7 ms")
        );
        // Nothing timed in the last minute: the newest stands alone.
        let quiet = Numbers {
            chat_delivery_avg_ms: None,
            ..jittery
        };
        assert_eq!(value(&quiet).as_deref(), Some("about 3.1 ms"));
    }

    // Every line that can hold an IP address has one, with a share running.
    fn addressed() -> Numbers {
        Numbers {
            link_name: Some(String::from("Ines")),
            rtt_ms: Some(4.2),
            peer_addr: Some("203.0.113.5:41000".parse().unwrap()),
            local_port: 41000,
            mapping: Some(MappingWord::Easy),
            public_addr: Some("198.51.100.7:41000".parse().unwrap()),
            mapping_protocol: Some(String::from("UPnP")),
            mapped_addr: Some("198.51.100.7:41000".parse().unwrap()),
            address_change: Some(AddressChange {
                at_unix: 1_000,
                this_pc: true,
                from: "192.0.2.44:41000".parse().unwrap(),
                to: "198.51.100.7:41000".parse().unwrap(),
            }),
            address_name: Some(NameView {
                name: String::from("myroom.duckdns.org"),
                answer: found(&["203.0.113.9"]),
                outside: Some(NameMatch {
                    points_to: Ipv4Addr::new(203, 0, 113, 9),
                    outside: Ipv4Addr::new(198, 51, 100, 7),
                }),
            }),
            sharing: Some(sharing()),
            ..Numbers::default()
        }
    }

    // The lines as the panel shows them with sharing off, and on.
    fn off_and_on(role: Role, n: &Numbers) -> (Vec<Line>, Vec<Line>) {
        let hidden = Showing {
            hide_addresses: true,
            ..SHOWING
        };
        let off = lines(role, n, "allowed", false, 1_133, None, SHOWING);
        let on = lines(role, n, "allowed", false, 1_133, None, hidden);
        (off, on)
    }

    // While you share, the lines that show an IP address read "Hidden while
    // you share" in ash, and every other line, the Video group included,
    // stays as it was.
    #[test]
    fn addresses_read_hidden_while_you_share() {
        let addresses = [
            "Remote address",
            "Public address",
            "Mapped address",
            "Old address",
            "New address",
            "Name points to",
        ];
        let n = addressed();
        for role in [Role::Host, Role::Client] {
            let (off, on) = off_and_on(role, &n);
            let labels = |lines: &[Line]| lines.iter().map(|l| l.label).collect::<Vec<_>>();
            assert_eq!(labels(&off), labels(&on));
            for label in addresses {
                let at = labels(&on).iter().position(|l| *l == label);
                let at = at.unwrap_or_else(|| panic!("no {label} line"));
                assert_eq!(on[at].value, "Hidden while you share");
                assert_eq!(value_color(&on[at]), ASH);
                assert_eq!(value_color(&off[at]), CHALK);
            }
            for (before, after) in off.iter().zip(&on) {
                if !addresses.contains(&before.label) {
                    assert_eq!(before, after);
                }
            }
            // Nothing else holds one of them either.
            for ip in ["203.0.113.5", "203.0.113.9", "198.51.100.7", "192.0.2.44"] {
                assert!(off.iter().any(|line| line.value.contains(ip)), "{ip}");
                for line in &on {
                    assert!(!line.value.contains(ip), "{}: {}", line.label, line.value);
                }
            }
            let video = on.iter().find(|line| line.label == "Encoder");
            assert_eq!(video.map(|line| line.value.as_str()), Some("NVENC H.264"));
        }
    }

    // A client's lookup names the addresses it refused too.
    #[test]
    fn a_hidden_name_hides_what_the_lookup_refused() {
        let refused = NameView {
            name: String::from("myroom.duckdns.org"),
            answer: NameAnswer::Found {
                addrs: Vec::new(),
                refused: vec![("127.0.0.1".parse().unwrap(), "it is a loopback address")],
            },
            outside: None,
        };
        let n = Numbers {
            address_name: Some(refused),
            ..Numbers::default()
        };
        let (off, on) = off_and_on(Role::Client, &n);
        let points_to = |lines: &[Line]| {
            let line = lines.iter().find(|line| line.label == "Name points to");
            line.map(|line| line.value.clone())
        };
        assert_eq!(
            points_to(&off).as_deref(),
            Some("127.0.0.1 not used: it is a loopback address")
        );
        assert_eq!(points_to(&on).as_deref(), Some("Hidden while you share"));
    }

    // An address not known yet is left out, as with sharing off.
    #[test]
    fn nothing_to_hide_adds_no_line() {
        let n = Numbers {
            local_port: 41000,
            ..Numbers::default()
        };
        let (off, on) = off_and_on(Role::Host, &n);
        assert_eq!(off, on);
    }

    #[test]
    fn port_mapping_lines_on_the_host_only() {
        let value = |role, numbers: &Numbers, label| {
            pairs(role, numbers, "allowed", false, 0, None)
                .into_iter()
                .find(|(l, _)| *l == label)
                .map(|(_, value)| value)
        };
        let unmapped = Numbers::default();
        assert_eq!(
            value(Role::Host, &unmapped, "Port mapping").as_deref(),
            Some("none")
        );
        assert_eq!(value(Role::Client, &unmapped, "Port mapping"), None);
        let mapped = Numbers {
            mapping_protocol: Some(String::from("NAT-PMP")),
            mapped_addr: Some("203.0.113.7:41000".parse().unwrap()),
            ..Numbers::default()
        };
        assert_eq!(
            value(Role::Host, &mapped, "Port mapping").as_deref(),
            Some("NAT-PMP")
        );
        assert_eq!(
            value(Role::Host, &mapped, "Mapped address").as_deref(),
            Some("203.0.113.7:41000")
        );
    }

    fn name_lines(role: Role, name: NameView) -> Vec<(&'static str, String)> {
        let numbers = Numbers {
            address_name: Some(name),
            ..Numbers::default()
        };
        pairs(role, &numbers, "allowed", false, 0, None)
            .into_iter()
            .filter(|(label, _)| matches!(*label, "Address name" | "Name points to"))
            .collect()
    }

    fn found(addrs: &[&str]) -> NameAnswer {
        NameAnswer::Found {
            addrs: addrs.iter().map(|ip| ip.parse().unwrap()).collect(),
            refused: Vec::new(),
        }
    }

    #[test]
    fn the_host_sees_whether_its_name_points_here() {
        let name = String::from("myroom.duckdns.org");
        let here = NameView {
            name: name.clone(),
            answer: found(&["198.51.100.20"]),
            outside: Some(NameMatch {
                points_to: Ipv4Addr::new(198, 51, 100, 20),
                outside: Ipv4Addr::new(198, 51, 100, 20),
            }),
        };
        assert_eq!(
            name_lines(Role::Host, here),
            [
                ("Address name", name.clone()),
                ("Name points to", String::from("198.51.100.20, this PC")),
            ]
        );
        let stale = NameView {
            name: name.clone(),
            answer: found(&["203.0.113.5"]),
            outside: Some(NameMatch {
                points_to: Ipv4Addr::new(203, 0, 113, 5),
                outside: Ipv4Addr::new(198, 51, 100, 20),
            }),
        };
        assert_eq!(
            name_lines(Role::Host, stale)[1].1,
            "203.0.113.5, not this PC's outside address 198.51.100.20"
        );
        // STUN and the router have not said yet.
        let unknown = NameView {
            name,
            answer: found(&["203.0.113.5"]),
            outside: None,
        };
        assert_eq!(name_lines(Role::Host, unknown)[1].1, "203.0.113.5");
    }

    #[test]
    fn client_name_lookup() {
        let view = |answer| NameView {
            name: String::from("myroom.duckdns.org"),
            answer,
            outside: None,
        };
        let not_yet = name_lines(Role::Client, view(NameAnswer::NotAsked));
        assert_eq!(
            not_yet,
            [("Address name", String::from("myroom.duckdns.org"))]
        );
        let value = |answer| name_lines(Role::Client, view(answer))[1].1.clone();
        assert_eq!(value(NameAnswer::Looking), "looking it up");
        assert_eq!(
            value(found(&["203.0.113.5", "2001:db8::5"])),
            "203.0.113.5, 2001:db8::5"
        );
        let refused = NameAnswer::Found {
            addrs: Vec::new(),
            refused: vec![("127.0.0.1".parse().unwrap(), "it is a loopback address")],
        };
        assert_eq!(
            value(refused),
            "127.0.0.1 not used: it is a loopback address"
        );
        assert_eq!(value(NameAnswer::NoSuchName), "no such name");
        assert_eq!(value(NameAnswer::NoAddress), "no address");
        assert_eq!(value(NameAnswer::Unanswered), "no answer");
    }

    #[test]
    fn firewall_states_read_as_one_word_each() {
        let public = Profiles::PUBLIC;
        assert_eq!(firewall(Some(&FirewallState::Allowed(public))), "allowed");
        assert_eq!(firewall(Some(&FirewallState::Blocked(public))), "blocked");
        let shut = FirewallState::BlockingAll(public);
        assert_eq!(firewall(Some(&shut)), "blocked");
        assert_eq!(firewall(Some(&FirewallState::Missing(public))), "no rule");
        let off = FirewallState::Unknown(String::from("off"));
        assert_eq!(firewall(Some(&off)), "not known");
        assert_eq!(firewall(None), "not known");
    }

    fn change_lines(role: Role, this_pc: bool) -> Vec<(&'static str, String)> {
        let numbers = Numbers {
            address_change: Some(AddressChange {
                at_unix: 1_000,
                this_pc,
                from: "203.0.113.5:41000".parse().unwrap(),
                to: "198.51.100.7:41000".parse().unwrap(),
            }),
            reconnect_ms: Some(1234.56),
            ..Numbers::default()
        };
        pairs(role, &numbers, "allowed", false, 1_133, None)
            .into_iter()
            .filter(|(label, _)| {
                matches!(
                    *label,
                    "Reconnect time" | "Address changed" | "Old address" | "New address"
                )
            })
            .collect()
    }

    #[test]
    fn the_host_sees_its_old_and_new_address() {
        assert_eq!(
            change_lines(Role::Host, true),
            [
                ("Reconnect time", String::from("1234.6 ms")),
                ("Address changed", String::from("this PC, 2 min 13 s ago")),
                ("Old address", String::from("203.0.113.5:41000")),
                ("New address", String::from("198.51.100.7:41000")),
            ]
        );
    }

    #[test]
    fn a_client_says_whose_address_changed() {
        let followed = change_lines(Role::Client, false);
        assert_eq!(
            followed[1],
            ("Address changed", String::from("the host, 2 min 13 s ago"))
        );
        let own = change_lines(Role::Client, true);
        assert_eq!(
            own[1],
            ("Address changed", String::from("this PC, 2 min 13 s ago"))
        );
        // A clock set back since then is not a change in the future.
        let early = AddressChange {
            at_unix: 2_000,
            this_pc: true,
            from: "203.0.113.5:41000".parse().unwrap(),
            to: "198.51.100.7:41000".parse().unwrap(),
        };
        assert_eq!(changed(&early, 1_000), "this PC, 0 s ago");
    }

    fn audio_lines(periods: Option<&Periods>) -> Vec<(&'static str, String)> {
        pairs(
            Role::Client,
            &Numbers::default(),
            "allowed",
            false,
            0,
            periods,
        )
        .into_iter()
        .filter(|(label, _)| matches!(*label, "Audio period" | "Resampled by Windows"))
        .collect()
    }

    #[test]
    fn audio_periods_once_the_devices_have_answered() {
        assert!(audio_lines(None).is_empty());
        let good = Periods {
            input: Side::Period {
                ms: 128.0 / 48.0,
                resampled: false,
            },
            output: Side::Period {
                ms: 128.0 / 48.0,
                resampled: false,
            },
        };
        assert_eq!(
            audio_lines(Some(&good)),
            [("Audio period", String::from("in 2.7 ms\nout 2.7 ms"))]
        );
        // This PC's AirPods: a 16 kHz hands-free microphone.
        let airpods = Periods {
            input: Side::Period {
                ms: 10.0,
                resampled: true,
            },
            output: Side::Period {
                ms: 10.0,
                resampled: false,
            },
        };
        assert_eq!(
            audio_lines(Some(&airpods)),
            [
                ("Audio period", String::from("in 10.0 ms\nout 10.0 ms")),
                ("Resampled by Windows", String::from("input")),
            ]
        );
        let odd = Periods {
            input: Side::NoDevice,
            output: Side::NotKnown,
        };
        assert_eq!(
            audio_lines(Some(&odd)),
            [("Audio period", String::from("in none\nout not known"))]
        );
    }

    fn voice_lines(n: &Numbers, probed: Option<&Periods>) -> Vec<(&'static str, String)> {
        let voice = [
            "Audio period",
            "Render latency",
            "Audio period, far side",
            "Render latency, far side",
            "Frame size",
            "Jitter buffer",
            "Voice loss",
            "Your voice, worst loss",
            "Mouth to ear",
            "Mouth to ear, 10 s",
            "Dropped, voice",
        ];
        pairs(Role::Host, n, "allowed", false, 0, probed)
            .into_iter()
            .filter(|(label, _)| voice.contains(label))
            .collect()
    }

    fn loss(all_pct: f32, scattered_pct: f32) -> VoiceLoss {
        VoiceLoss {
            all_pct,
            scattered_pct,
        }
    }

    // While voice flows the strip's jitter and loss come from it, and the
    // panel says which is which.
    #[test]
    fn jitter_and_loss_say_where_they_came_from() {
        let link_lines = |n: &Numbers| -> Vec<(&'static str, String)> {
            pairs(Role::Client, n, "allowed", false, 0, None)
                .into_iter()
                .filter(|(label, _)| label.starts_with("Jitter") || label.starts_with("Loss"))
                .collect()
        };
        let pings = Numbers {
            jitter_ms: Some(0.42),
            loss_pct: Some(1.0),
            inbound_loss_pct: Some(0.0),
            ..Numbers::default()
        };
        let text = |s: &str| String::from(s);
        assert_eq!(
            link_lines(&pings),
            [
                ("Jitter, from pings", text("0.4 ms")),
                ("Loss, from pings", text("1.0%")),
                ("Loss inbound, from pings", text("0.0%")),
            ]
        );
        let voice = Numbers {
            jitter_from: Source::Voice,
            loss_from: Source::Voice,
            jitter_ms: Some(3.24),
            loss_pct: Some(10.0),
            ..pings.clone()
        };
        assert_eq!(
            link_lines(&voice),
            [
                ("Jitter, from voice", text("3.2 ms")),
                ("Loss, from voice", text("10.0%")),
                ("Loss inbound, from pings", text("0.0%")),
            ]
        );
        // Voice with no capture time to read yet: its loss, the pings' jitter.
        let early = Numbers {
            jitter_from: Source::Pings,
            ..voice
        };
        assert_eq!(link_lines(&early)[0].0, "Jitter, from pings");
        assert_eq!(link_lines(&early)[1].0, "Loss, from voice");
    }

    #[test]
    fn voice_numbers() {
        use room::view::{Buffer, MouthToEar};
        let n = Numbers {
            audio_in_ms: Some(2.667),
            audio_out_ms: Some(10.0),
            render_latency_ms: Some(20.0),
            far_audio_in_ms: Some(2.667),
            far_audio_out_ms: None,
            far_render_latency_ms: Some(6.8),
            send_frame_ms: 5,
            send_repair_copy: true,
            buffer: Some(Buffer {
                name: String::from("Ana"),
                ms: 10,
                frames: 2,
            }),
            voice_loss: vec![
                (String::from("Ana"), loss(10.5, 1.5)),
                (String::from("Bo"), loss(0.0, 0.0)),
            ],
            own_voice_loss: Some(loss(0.5, 0.0)),
            mouth_to_ear: Some(MouthToEar {
                name: String::from("Ana"),
                last_ms: 21.04,
                avg_ms: 20.7,
                p95_ms: 21.14,
                about: true,
            }),
            voice_dropped: 3,
            ..Numbers::default()
        };
        let text = |s: &str| String::from(s);
        assert_eq!(
            voice_lines(&n, None),
            [
                ("Audio period", text("in 2.7 ms\nout 10.0 ms")),
                ("Render latency", text("20.0 ms")),
                ("Audio period, far side", text("in 2.7 ms\nout none")),
                ("Render latency, far side", text("6.8 ms")),
                ("Frame size", text("5 ms with repair copy")),
                (
                    "Jitter buffer",
                    text("\u{2068}Ana\u{2069}, 10 ms (2 frames)")
                ),
                (
                    "Voice loss",
                    text("\u{2068}Ana\u{2069} 10.5% (1.5% scattered)")
                ),
                ("Voice loss", text("\u{2068}Bo\u{2069} 0.0%")),
                ("Your voice, worst loss", text("0.5%")),
                ("Mouth to ear", text("about 21.0 ms, from Ana")),
                (
                    "Mouth to ear, 10 s",
                    text("about 20.7 ms average, p95 21.1 ms")
                ),
                ("Dropped, voice", text("3")),
            ]
        );
        let tens = Numbers {
            send_frame_ms: 10,
            send_repair_copy: false,
            ..Numbers::default()
        };
        assert_eq!(voice_lines(&tens, None), [("Frame size", text("10 ms"))]);
        assert!(voice_lines(&Numbers::default(), None).is_empty());
    }

    // Muted, the microphone is closed, and its period is what the device
    // said when the stats panel asked it.
    #[test]
    fn closed_stream_period() {
        let n = Numbers {
            audio_out_ms: Some(2.667),
            ..Numbers::default()
        };
        let probed = Periods {
            input: Side::Period {
                ms: 10.0,
                resampled: false,
            },
            output: Side::Period {
                ms: 20.0,
                resampled: false,
            },
        };
        assert_eq!(
            voice_lines(&n, Some(&probed))[0],
            ("Audio period", String::from("in 10.0 ms\nout 2.7 ms"))
        );
        assert_eq!(
            voice_lines(&n, None)[0],
            ("Audio period", String::from("in not known\nout 2.7 ms"))
        );
    }

    // The same case as the warning in settings, and nothing for a microphone
    // that is fine or while none is open.
    #[test]
    fn microphone_line() {
        let line = |microphone: Option<Microphone>| {
            let n = Numbers {
                microphone,
                ..Numbers::default()
            };
            pairs(Role::Client, &n, "allowed", false, 0, None)
                .into_iter()
                .find(|(label, _)| *label == MICROPHONE)
                .map(|(_, value)| value)
        };
        let mic = |rate, hands_free| Some(Microphone { rate, hands_free });
        assert_eq!(
            line(mic(8_000, true)).as_deref(),
            Some("8 kHz, Bluetooth hands-free")
        );
        assert_eq!(
            line(mic(16_000, true)).as_deref(),
            Some("16 kHz, Bluetooth hands-free")
        );
        assert_eq!(line(mic(11_025, false)).as_deref(), Some("11.025 kHz"));
        assert_eq!(line(mic(48_000, false)), None);
        assert_eq!(line(mic(44_100, false)), None);
        assert_eq!(line(None), None);
    }
}
