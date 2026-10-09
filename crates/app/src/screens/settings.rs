use eframe::egui::{
    Align, Key, Layout, Rect, Response, ScrollArea, Sense, Ui, UiBuilder, WidgetType, vec2,
};
use input::{Action, Bindings, Chord};
use room::{BlockedKey, KnownDevice, KnownDevices, TalkMode};
use voice::audio::{Choice, DeviceList, Direction};
use zeroize::Zeroize;

use crate::controls::{self, Button, Lead};
use crate::messages;
use crate::settings::{LEAST_UPLOAD_MBITS, MOST_UPLOAD_MBITS, NAME_CHARS, Settings};
use crate::sound::SettingsAudio;
use crate::theme::{
    self, ASH, BAD, CHALK, CONTROL_HEIGHT, FIELD_GAP, HALF_STEP, PANEL, ROW_HEIGHT, Role,
    SECTION_GAP, SIDE, STEP, WARN,
};
use crate::update;
use crate::win;

// Far above the 253 characters a name can have, so a long paste is refused
// with the sentence rather than cut to something that passes.
const ADDRESS_NAME_CHARS: usize = 512;
const PORT_CHARS: usize = 5;
const STUN_CHARS: usize = 2048;
// Save and Cancel on a 64 px bar, with the gutter above and below them.
const BAR_HEIGHT: f32 = CONTROL_HEIGHT + 2.0 * SIDE;
// A hotkey's key in words and a device's second line, one body line tall.
const DETAIL_HEIGHT: f32 = 18.0;
// The meter's scale: this far below full scale is an empty bar. Speech
// peaks sit around -20 to -6 dB, which a linear bar would show as a sliver.
const METER_FLOOR_DB: f32 = -60.0;

// The settings as typed, until Save or Cancel.
pub struct Draft {
    pub name: String,
    pub port: String,
    pub stun_servers: String,
    pub address_name: String,
    pub input: Choice,
    pub output: Choice,
    pub talk: TalkMode,
    pub constant_rate: bool,
    pub upload_mbits: u32,
    pub vsync: bool,
    pub hide_strip: bool,
    pub check_for_new_versions: bool,
    pub hotkeys: Bindings,
    // The row waiting for its new key, and the last key refused for it with
    // the action that already has it.
    pub waiting: Option<Action>,
    pub key_refused: Option<Action>,
    // Why no key can be read: the hotkeys did not start.
    pub hotkeys_off: Option<String>,
    // This PC is controlled, so the hotkeys and the sharing settings take
    // no change and no row waits for a key. The panel sets it every frame;
    // Save checks again for itself.
    pub locked: bool,
    // As listed when the screen opened, less what Remove and Unblock took
    // off. Those take effect on Save.
    pub devices: Vec<KnownDevice>,
    pub blocked: Vec<BlockedKey>,
    pub removed: Vec<[u8; 32]>,
    pub unblocked: Vec<[u8; 32]>,
    // What an empty name means, shown in the field.
    user_name: String,
    pub refused: Refused,
    // Why settings.txt could not be written.
    pub error: Option<String>,
    // Why a Remove or Unblock could not be written, or the known devices
    // could not be read. Shown with them, not under a field.
    pub list_error: Option<String>,
}

// Which fields the last Save found wrong, each with its own line.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Refused {
    pub port: bool,
    // Counted from 1, as the lines in the field.
    pub stun_line: Option<usize>,
    pub address_name: bool,
}

impl Refused {
    fn any(&self) -> bool {
        self.port || self.stun_line.is_some() || self.address_name
    }
}

pub enum Press {
    Save,
    Cancel,
}

impl Draft {
    pub fn new(saved: &Settings, known: KnownDevices, user_name: String) -> Draft {
        Draft {
            name: saved.name().unwrap_or_default().to_owned(),
            port: saved.port().to_string(),
            stun_servers: saved.stun_servers().join("\n"),
            address_name: saved.address_name().unwrap_or_default().to_owned(),
            input: saved.device(Direction::Input),
            output: saved.device(Direction::Output),
            talk: saved.talk_mode(),
            constant_rate: saved.constant_rate(),
            upload_mbits: saved.upload_mbits(),
            vsync: saved.vsync(),
            hide_strip: saved.hide_strip(),
            check_for_new_versions: saved.check_for_new_versions(),
            hotkeys: saved.hotkeys(),
            waiting: None,
            key_refused: None,
            hotkeys_off: None,
            locked: false,
            devices: known.devices,
            blocked: known.blocked,
            removed: Vec::new(),
            unblocked: Vec::new(),
            user_name,
            refused: Refused::default(),
            error: None,
            list_error: None,
        }
    }

    // What Save would keep, built on the saved settings so anything this
    // screen does not show is carried over as it was.
    pub fn settings(&self, saved: &Settings) -> Result<Settings, Refused> {
        let mut next = saved.clone();
        next.set_name(&self.name);
        next.set_device(Direction::Input, &self.input);
        next.set_device(Direction::Output, &self.output);
        next.set_talk_mode(self.talk);
        next.set_constant_rate(self.constant_rate);
        next.set_upload_mbits(self.upload_mbits);
        next.set_vsync(self.vsync);
        next.set_hide_strip(self.hide_strip);
        next.set_check_for_new_versions(self.check_for_new_versions);
        next.set_hotkeys(self.hotkeys);
        let refused = Refused {
            port: next.set_port(&self.port).is_err(),
            stun_line: next.set_stun_servers(&self.stun_servers).err(),
            address_name: next.set_address_name(&self.address_name).is_err(),
        };
        if refused.any() {
            return Err(refused);
        }
        Ok(next)
    }

    fn remove(&mut self, key: [u8; 32]) {
        self.devices.retain(|device| device.key != key);
        // retain leaves a copy of the last device, secret and all, past the
        // end.
        self.devices.spare_capacity_mut().zeroize();
        self.removed.push(key);
    }

    fn unblock(&mut self, key: [u8; 32]) {
        self.blocked.retain(|blocked| blocked.key != key);
        self.unblocked.push(key);
    }

    // Change was pressed on a row. Another row's wait ends; nothing waits
    // when no key can be read.
    pub fn change(&mut self, action: Action) {
        if self.hotkeys_off.is_some() || self.locked {
            return;
        }
        self.waiting = Some(action);
        self.key_refused = None;
    }

    // The chord the hotkeys read while a row waits. A key another action has
    // is refused and the row keeps waiting for another. One Windows keeps is
    // passed over: it usually takes focus away, which ends the wait anyway,
    // and its press reaches the hotkeys before the panel knows focus went.
    pub fn take_chord(&mut self, chord: Chord) {
        if self.locked {
            self.stop_waiting();
            return;
        }
        let Some(action) = self.waiting else {
            return;
        };
        if chord.taken_by_windows() {
            return;
        }
        if let Some(other) = self.hotkeys.conflict(action, chord) {
            self.key_refused = Some(other);
            return;
        }
        self.hotkeys.set(action, chord);
        self.waiting = None;
        self.key_refused = None;
    }

    // Esc, Save or Cancel: the row keeps the key it had.
    pub fn stop_waiting(&mut self) {
        self.waiting = None;
        self.key_refused = None;
    }
}

// One column in the panel, label above field, sections told apart by space
// and their head alone. Save and Cancel sit at the bottom edge on a bar in
// panel tone rather than after the last setting, so they stay in reach
// however long the list above them grows; the list scrolls.
pub fn show(ui: &mut Ui, draft: &mut Draft, audio: &SettingsAudio) -> Option<Press> {
    controls::title_row(ui, Lead::Title("Settings"), &[]);
    let rest = ui.available_rect_before_wrap();
    let (list, bar) = rest.split_top_bottom_at_y(rest.bottom() - BAR_HEIGHT);

    let mut fields = Vec::new();
    ui.scope_builder(UiBuilder::new().max_rect(list), |ui| {
        let scroll = ScrollArea::vertical()
            .id_salt("settings")
            .auto_shrink(false);
        scroll.show(ui, |ui| {
            controls::page(ui, |ui| {
                if draft.locked {
                    controls::prose(ui, messages::SETTINGS_LOCKED, theme::body(), WARN);
                    ui.add_space(SECTION_GAP);
                }
                fields.push(name(ui, draft));
                ui.add_space(SECTION_GAP);
                fields.extend(network(ui, draft));
                hotkeys(ui, draft);
                voice(ui, draft, audio);
                sharing(ui, draft);
                devices(ui, draft);
                blocked(ui, draft);
                if let Some(error) = &draft.list_error {
                    ui.add_space(FIELD_GAP);
                    controls::prose(ui, error.as_str(), theme::body(), BAD);
                }
            });
        });
    });
    // Enter in any one-line field is Save; the STUN list takes Enter for a
    // new line and is not among them.
    let field_focused = fields.iter().any(Response::has_focus);
    let entered = fields
        .iter()
        .any(|field| field.lost_focus() && ui.input(|input| input.key_pressed(Key::Enter)));

    let mut press = None;
    ui.scope_builder(UiBuilder::new().max_rect(bar), |ui| {
        ui.painter().rect_filled(bar, 0, PANEL);
        let row = UiBuilder::new()
            .max_rect(bar.shrink2(vec2(SIDE, 0.0)))
            .layout(Layout::left_to_right(Align::Center));
        ui.scope_builder(row, |ui| {
            let save = Button::new("Save")
                .role(Role::Primary)
                .enter_target(field_focused)
                .show(ui);
            if save.clicked() || entered {
                press = Some(Press::Save);
            }
            if Button::new("Cancel").show(ui).clicked() {
                press = Some(Press::Cancel);
            }
        });
    });
    press
}

fn head(ui: &mut Ui, text: &str) {
    controls::text(ui, text, theme::section(), CHALK);
    ui.add_space(STEP);
}

// A field's name above it, in body ash, 8 px over the field.
fn label(ui: &mut Ui, text: &str) {
    controls::text(ui, text, theme::body(), ASH);
    ui.add_space(STEP);
}

fn name(ui: &mut Ui, draft: &mut Draft) -> Response {
    head(ui, "Name");
    // Empty means the Windows user name, which the field shows until a
    // name is typed.
    let field = controls::field(
        ui,
        "name",
        &mut draft.name,
        &draft.user_name,
        theme::body(),
        NAME_CHARS,
    );
    ui.ctx()
        .accesskit_node_builder(field.id, |node| node.set_label("Name"));
    field
}

fn network(ui: &mut Ui, draft: &mut Draft) -> [Response; 2] {
    head(ui, "Network");

    label(ui, "Port");
    let port = labelled(ui, "port", &mut draft.port, "Port", PORT_CHARS);
    if port.changed() {
        draft.refused.port = false;
    }
    if draft.refused.port {
        line(ui, messages::PORT_REFUSED, BAD);
    }

    ui.add_space(FIELD_GAP);
    label(ui, "STUN servers");
    let stun = controls::lines_field(
        ui,
        "stun servers",
        &mut draft.stun_servers,
        "STUN servers",
        STUN_CHARS,
    );
    if stun.changed() {
        draft.refused.stun_line = None;
    }
    if let Some(at) = draft.refused.stun_line {
        line(ui, &messages::stun_refused(at), BAD);
    } else if draft.stun_servers.trim().is_empty() {
        line(ui, messages::NO_STUN, ASH);
    }

    ui.add_space(FIELD_GAP);
    label(ui, "Address name");
    // The name is already written above the field, so no placeholder
    // repeats it.
    let address = labelled(
        ui,
        "address name",
        &mut draft.address_name,
        "Address name",
        ADDRESS_NAME_CHARS,
    );
    if address.changed() {
        draft.refused.address_name = false;
    }
    if draft.refused.address_name {
        line(ui, messages::ADDRESS_NAME_REFUSED, BAD);
    } else {
        line(ui, messages::ADDRESS_NAME_ABOUT, ASH);
    }

    // The update check talks to GitHub, so it sits with the other settings
    // that decide who this PC talks to. Its sentence shows while it is on,
    // the choice that costs something.
    ui.add_space(FIELD_GAP);
    let id = ui.id().with("new versions");
    let on = draft.check_for_new_versions;
    let kind = WidgetType::Checkbox;
    if controls::choice_row(ui, id, on, "Check for new versions", None, kind).clicked() {
        draft.check_for_new_versions = !on;
    }
    if draft.check_for_new_versions {
        line(ui, update::CHECK_ABOUT, ASH);
    }
    if let Some(error) = &draft.error {
        line(ui, error.as_str(), BAD);
    }
    [port, address]
}

// Each action on two lines, its name and Change, then its key in words,
// since a 360 px column does not hold "Show or hide the panel",
// "Ctrl+Shift+Space" and a button side by side. The name is the label, in
// ash; the key is the value, in chalk. While a row waits, its Change keeps
// the control fill and the key's line asks for the new one.
fn hotkeys(ui: &mut Ui, draft: &mut Draft) {
    ui.add_space(SECTION_GAP);
    head(ui, "Hotkeys");
    if let Some(off) = &draft.hotkeys_off {
        controls::prose(ui, off.as_str(), theme::body(), BAD);
        ui.add_space(STEP);
    }
    let mut changed = None;
    for (i, action) in Action::ALL.into_iter().enumerate() {
        if i > 0 {
            ui.add_space(FIELD_GAP);
        }
        let rect = row(ui, ROW_HEIGHT);
        let label = messages::hotkey_label(action);
        let (text, waiting) = key_line(draft, action);
        if draft.hotkeys_off.is_some() {
            let mut left = ui.new_child(
                UiBuilder::new()
                    .max_rect(rect)
                    .layout(Layout::left_to_right(Align::Center)),
            );
            controls::one_line(&mut left, label, theme::body(), ASH);
        } else {
            let button = Button::new("Change").on(waiting).height(rect.height());
            let width = button.width(ui);
            controls::split_row(
                ui,
                rect,
                width,
                |ui| {
                    controls::one_line(ui, label, theme::body(), ASH);
                },
                |ui| {
                    let response = button.show(ui);
                    let name = format!("Change the key for {}", action.name());
                    ui.ctx()
                        .accesskit_node_builder(response.id, |node| node.set_label(name));
                    if response.clicked() {
                        changed = Some(action);
                    }
                },
            );
        }
        let rect = row(ui, DETAIL_HEIGHT);
        let mut key = ui.new_child(
            UiBuilder::new()
                .max_rect(rect)
                .layout(Layout::left_to_right(Align::Center)),
        );
        controls::one_line(&mut key, &text, theme::body(), CHALK);
        if let Some(refused) = refused_line(draft, action) {
            line(ui, &refused, BAD);
        }
    }
    if let Some(action) = changed {
        draft.change(action);
    }
}

// The key in words, or while the row waits the line that asks for one; true
// while it waits.
fn key_line(draft: &Draft, action: Action) -> (String, bool) {
    if draft.waiting == Some(action) {
        (String::from(messages::PRESS_NEW_KEY), true)
    } else {
        (draft.hotkeys.chord(action).to_string(), false)
    }
}

fn refused_line(draft: &Draft, action: Action) -> Option<String> {
    let other = draft
        .key_refused
        .filter(|_| draft.waiting == Some(action))?;
    Some(messages::key_used(other))
}

// The input and output device, each a list with Windows default first.
// Under the input list, the warning for a thin or hands-free microphone,
// then the meter of the microphone this screen has open. Then how you talk
// and constant-rate voice, each with its one sentence when it is the choice
// that costs something.
fn voice(ui: &mut Ui, draft: &mut Draft, audio: &SettingsAudio) {
    ui.add_space(SECTION_GAP);
    head(ui, "Voice");
    let lists = audio.lists();
    let (inputs, outputs) = match lists {
        Some(Ok(lists)) => (Some(&lists.inputs), Some(&lists.outputs)),
        Some(Err(problem)) => {
            controls::prose(ui, problem.as_str(), theme::body(), BAD);
            ui.add_space(STEP);
            (None, None)
        }
        None => (None, None),
    };

    label(ui, "Input device");
    device_list(ui, Direction::Input, &mut draft.input, inputs);
    if let Some(warning) = audio.microphone().and_then(messages::microphone_warning) {
        line(ui, &warning, WARN);
    }
    ui.add_space(STEP);
    let level = audio
        .level()
        .map_or(0.0, |level| meter_fraction(level.peak));
    controls::meter(ui, level, "Microphone level");
    match audio.problem() {
        Some(problem) => line(ui, problem, BAD),
        None => line(ui, messages::MICROPHONE_OPEN, ASH),
    };

    ui.add_space(FIELD_GAP);
    label(ui, "Output device");
    device_list(ui, Direction::Output, &mut draft.output, outputs);

    ui.add_space(FIELD_GAP);
    label(ui, "How you talk");
    for (mode, name) in [
        (TalkMode::PushToTalk, "Push to talk"),
        (TalkMode::OpenMic, "Open mic"),
    ] {
        let id = ui.id().with(("talk", name));
        let kind = WidgetType::RadioButton;
        if controls::choice_row(ui, id, draft.talk == mode, name, None, kind).clicked() {
            draft.talk = mode;
        }
    }
    if draft.talk == TalkMode::OpenMic {
        line(ui, messages::OPEN_MIC, ASH);
    }

    ui.add_space(FIELD_GAP);
    let id = ui.id().with("constant rate");
    let kind = WidgetType::Checkbox;
    let name = "Constant-rate voice";
    if controls::choice_row(ui, id, draft.constant_rate, name, None, kind).clicked() {
        draft.constant_rate = !draft.constant_rate;
    }
    if !draft.constant_rate {
        line(ui, messages::CONSTANT_RATE_OFF, ASH);
    }
}

// The video upload, vsync in the viewer and the strip hidden in fullscreen,
// each with its one sentence when it is the choice that needs one. Like the
// rest, they apply to the next room.
fn sharing(ui: &mut Ui, draft: &mut Draft) {
    ui.add_space(SECTION_GAP);
    head(ui, "Sharing");
    controls::text(ui, "Video upload", theme::body(), ASH);
    let id = ui.id().with("video upload");
    let range = LEAST_UPLOAD_MBITS..=MOST_UPLOAD_MBITS;
    controls::slider(
        ui,
        id,
        &mut draft.upload_mbits,
        range,
        "Mbit/s",
        "Video upload",
    );
    controls::prose(ui, messages::UPLOAD_ABOUT, theme::caption(), ASH);

    ui.add_space(FIELD_GAP);
    let id = ui.id().with("vsync");
    let kind = WidgetType::Checkbox;
    if controls::choice_row(ui, id, draft.vsync, "Vsync in the viewer", None, kind).clicked() {
        draft.vsync = !draft.vsync;
    }
    if draft.vsync {
        line(ui, messages::VSYNC_ON, ASH);
    }

    ui.add_space(STEP);
    let id = ui.id().with("hide strip");
    let name = "Hide the strip in fullscreen";
    if controls::choice_row(ui, id, draft.hide_strip, name, None, kind).clicked() {
        draft.hide_strip = !draft.hide_strip;
    }
    if let Some(note) = hide_strip_line(draft.hide_strip, win::animations_on()) {
        line(ui, note, ASH);
    }
}

// With Windows' animation effects off the strip stays, setting or not, and
// the setting says so rather than seem broken.
fn hide_strip_line(on: bool, animations: bool) -> Option<&'static str> {
    match (on, animations) {
        (false, _) => None,
        (true, true) => Some(messages::HIDE_STRIP_ON),
        (true, false) => Some(messages::HIDE_STRIP_STAYS),
    }
}

// One row per choice. `list` is None until Windows has answered, which is a
// few milliseconds after the screen opens.
fn device_list(ui: &mut Ui, direction: Direction, choice: &mut Choice, list: Option<&DeviceList>) {
    let mut rows = vec![(Choice::Default, "Windows default", default_note(list))];
    if let Some(list) = list {
        rows.extend(list.devices.iter().map(|device| {
            (
                Choice::Device(device.id.clone()),
                device.name.as_str(),
                None,
            )
        }));
    }
    // A device chosen before and not plugged in now keeps its row, so the
    // screen does not look as if the choice were lost.
    if let Choice::Device(id) = &*choice
        && list.is_none_or(|list| list.find(id).is_none())
    {
        let note = list.map(|_| String::from("not connected"));
        rows.push((choice.clone(), "Chosen device", note));
    }
    let mut picked = None;
    for (row, name, note) in &rows {
        // Keyed by the device, so focus stays on it when the list changes.
        let id = ui.id().with((direction, row));
        let selected = row == choice;
        let kind = WidgetType::RadioButton;
        if controls::choice_row(ui, id, selected, name, note.as_deref(), kind).clicked() {
            picked = Some(row.clone());
        }
    }
    if let Some(row) = picked {
        *choice = row;
    }
}

fn default_note(list: Option<&DeviceList>) -> Option<String> {
    let list = list?;
    Some(match list.default_device() {
        Some(device) => device.name.clone(),
        None => String::from("none connected"),
    })
}

// 0 at METER_FLOOR_DB and below, 1 at full scale.
fn meter_fraction(peak: f32) -> f32 {
    if peak.is_nan() || peak <= 0.0 {
        return 0.0;
    }
    let db = 20.0 * peak.log10();
    ((db - METER_FLOOR_DB) / -METER_FLOOR_DB).clamp(0.0, 1.0)
}

// A field for an address or a port, typed in Plex Mono, with its name above
// it, which a screen reader gets as its label.
fn labelled(ui: &mut Ui, id: &str, text: &mut String, label: &str, chars: usize) -> Response {
    let field = controls::field(ui, id, text, "", theme::mono(), chars);
    let label = label.to_owned();
    ui.ctx()
        .accesskit_node_builder(field.id, |node| node.set_label(label));
    field
}

// A help line or an error, half a step under the control it belongs to.
fn line(ui: &mut Ui, text: &str, color: eframe::egui::Color32) {
    ui.add_space(HALF_STEP);
    controls::prose(ui, text, theme::caption(), color);
}

// Only on a PC that has hosted and has any. Each one on two lines, since a
// 360 px column holds a name and a button or a fingerprint and a date, not
// all four.
fn devices(ui: &mut Ui, draft: &mut Draft) {
    if draft.devices.is_empty() {
        return;
    }
    ui.add_space(SECTION_GAP);
    head(ui, "Known devices");
    let mut removed = None;
    for (i, device) in draft.devices.iter().enumerate() {
        if i > 0 {
            ui.add_space(STEP);
        }
        let rect = row(ui, ROW_HEIGHT);
        let remove = Button::new("Remove").role(Role::Destructive);
        if with_button(ui, rect, remove, |ui| {
            controls::one_line(ui, &device.name, theme::body(), CHALK);
        }) {
            removed = Some(device.key);
        }
        let rect = row(ui, DETAIL_HEIGHT);
        let seen = messages::last_seen(device.last_seen);
        let seen_width = controls::text_width(ui, &seen, theme::caption());
        controls::split_row(
            ui,
            rect,
            seen_width,
            |ui| {
                let print = keys::fingerprint(&device.key);
                controls::one_line(ui, &print, theme::mono_caption(), ASH);
            },
            |ui| {
                controls::one_line(ui, &seen, theme::caption(), ASH);
            },
        );
    }
    if let Some(key) = removed {
        draft.remove(key);
    }
}

fn blocked(ui: &mut Ui, draft: &mut Draft) {
    if draft.blocked.is_empty() {
        return;
    }
    ui.add_space(SECTION_GAP);
    head(ui, "Blocked");
    let mut unblocked = None;
    for blocked in &draft.blocked {
        let rect = row(ui, ROW_HEIGHT);
        if with_button(ui, rect, Button::new("Unblock"), |ui| {
            controls::one_line(ui, &keys::fingerprint(&blocked.key), theme::mono(), CHALK);
        }) {
            unblocked = Some(blocked.key);
        }
    }
    if let Some(key) = unblocked {
        draft.unblock(key);
    }
}

fn row(ui: &mut Ui, height: f32) -> Rect {
    ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::hover())
        .0
}

// True when the button was pressed.
fn with_button(ui: &mut Ui, rect: Rect, button: Button, left: impl FnOnce(&mut Ui)) -> bool {
    let button = button.height(rect.height());
    let width = button.width(ui);
    let mut pressed = false;
    controls::split_row(ui, rect, width, left, |ui| {
        pressed = button.show(ui).clicked();
    });
    pressed
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::{Context, RawInput, ViewportId, ViewportInfo, pos2};

    fn typed(address_name: &str) -> Draft {
        Draft {
            address_name: address_name.to_owned(),
            ..Draft::new(
                &Settings::default(),
                KnownDevices::default(),
                String::from("Shadi"),
            )
        }
    }

    fn refused(draft: &Draft) -> Refused {
        draft.settings(&Settings::default()).unwrap_err()
    }

    #[test]
    fn a_name_booth_cannot_use_gets_the_one_sentence() {
        let long_part = format!("{}.example.net", "a".repeat(64));
        let too_long = format!("{}example.net", "a.".repeat(125));
        for bad in [
            "localhost",
            "LocalHost",
            "192.168.1.20",
            "127.1",
            "0x7f000001",
            "my room.example.net",
            "https://myroom.example.net",
            "myroom.example.net.",
            "-myroom.example.net",
            long_part.as_str(),
            too_long.as_str(),
        ] {
            assert_eq!(
                refused(&typed(bad)),
                Refused {
                    address_name: true,
                    ..Refused::default()
                },
                "{bad}"
            );
        }
    }

    #[test]
    fn name_trimmed() {
        let next = typed("  myroom.example.net ")
            .settings(&Settings::default())
            .unwrap();
        assert_eq!(next.address_name(), Some("myroom.example.net"));
    }

    #[test]
    fn an_empty_field_means_no_name() {
        let mut saved = Settings::default();
        saved.set_address_name("myroom.example.net").unwrap();
        let draft = Draft::new(&saved, KnownDevices::default(), String::new());
        assert_eq!(draft.address_name, "myroom.example.net");
        for empty in ["", "   "] {
            assert_eq!(typed(empty).settings(&saved).unwrap().address_name(), None);
        }
        assert_eq!(typed("").address_name, "");
    }

    #[test]
    fn the_screen_opens_with_what_is_in_use() {
        let draft = typed("");
        assert_eq!(draft.name, "");
        assert_eq!(draft.port, "41000");
        assert_eq!(
            draft.stun_servers,
            "stun.cloudflare.com:3478\nstun.l.google.com:19302"
        );
        // Saved unchanged, it is the settings it opened with.
        assert_eq!(
            draft.settings(&Settings::default()),
            Ok(Settings::default())
        );
    }

    #[test]
    fn all_fields_refused_at_once() {
        let mut draft = typed("localhost");
        draft.port = String::from("80");
        draft.stun_servers = String::from("stun.example.org:3478\nstun.example.org");
        assert_eq!(
            refused(&draft),
            Refused {
                port: true,
                stun_line: Some(2),
                address_name: true,
            }
        );
        draft.port = String::from("41010");
        draft.stun_servers.clear();
        draft.address_name.clear();
        draft.name = String::from(" Mara ");
        let next = draft.settings(&Settings::default()).unwrap();
        assert_eq!(next.port(), 41010);
        assert!(next.stun_servers().is_empty());
        assert_eq!(next.name(), Some("Mara"));
    }

    fn device(n: u8) -> KnownDevices {
        KnownDevices {
            devices: Vec::new(),
            blocked: vec![BlockedKey {
                key: [n; 32],
                since: 1,
            }],
        }
    }

    // Remove and Unblock take effect on Save; the screen only notes them.
    #[test]
    fn unblock_waits_for_save() {
        let mut draft = Draft::new(&Settings::default(), device(7), String::new());
        draft.unblock([7; 32]);
        assert!(draft.blocked.is_empty());
        assert_eq!(draft.unblocked, [[7; 32]]);
        assert!(draft.removed.is_empty());
    }

    #[test]
    fn device_choices_saved() {
        let mic = Choice::Device(String::from("{0.0.1.00000000}.{1ebb}"));
        let mut saved = Settings::default();
        saved.set_device(Direction::Input, &mic);
        let mut draft = Draft::new(&saved, KnownDevices::default(), String::new());
        assert_eq!(draft.input, mic);
        assert_eq!(draft.output, Choice::Default);
        draft.input = Choice::Default;
        draft.output = Choice::Device(String::from("{0.0.0.00000000}.{a740}"));
        let next = draft.settings(&saved).unwrap();
        assert_eq!(next.device(Direction::Input), Choice::Default);
        assert_eq!(next.device(Direction::Output), draft.output);
    }

    #[test]
    fn voice_choices_saved() {
        let mut saved = Settings::default();
        saved.set_talk_mode(TalkMode::OpenMic);
        let mut draft = Draft::new(&saved, KnownDevices::default(), String::new());
        assert_eq!((draft.talk, draft.constant_rate), (TalkMode::OpenMic, true));
        draft.talk = TalkMode::PushToTalk;
        draft.constant_rate = false;
        let next = draft.settings(&saved).unwrap();
        assert_eq!(next.talk_mode(), TalkMode::PushToTalk);
        assert!(!next.constant_rate());
    }

    fn chord(text: &str) -> Chord {
        text.parse().unwrap()
    }

    // A row per action with its key in words; Change waits for the next
    // chord, and a key another action has is refused with the sentence while
    // the row keeps waiting.
    #[test]
    fn new_key_unless_taken() {
        let mut draft = typed("");
        assert_eq!(
            key_line(&draft, Action::PushToTalk),
            (String::from("Right Ctrl"), false)
        );
        assert_eq!(
            key_line(&draft, Action::ShowPanel),
            (String::from("Ctrl+Shift+Space"), false)
        );
        draft.change(Action::Mute);
        assert_eq!(
            key_line(&draft, Action::Mute),
            (String::from(messages::PRESS_NEW_KEY), true)
        );
        draft.take_chord(chord("Ctrl+Shift+D"));
        assert_eq!(draft.waiting, Some(Action::Mute));
        assert_eq!(
            refused_line(&draft, Action::Mute).as_deref(),
            Some("That key is already used for deafen.")
        );
        assert_eq!(refused_line(&draft, Action::Deafen), None);
        draft.take_chord(chord("F9"));
        assert_eq!(draft.waiting, None);
        assert_eq!(refused_line(&draft, Action::Mute), None);
        assert_eq!(key_line(&draft, Action::Mute).0, "F9");
        let next = draft.settings(&Settings::default()).unwrap();
        assert_eq!(next.hotkeys().chord(Action::Mute), chord("F9"));
        // Its own key again is no conflict.
        draft.change(Action::Mute);
        draft.take_chord(chord("F9"));
        assert_eq!((draft.waiting, draft.key_refused), (None, None));
    }

    #[test]
    fn esc_keeps_the_old_key() {
        let mut draft = typed("");
        draft.change(Action::Deafen);
        draft.take_chord(chord("Right Ctrl"));
        assert_eq!(draft.key_refused, Some(Action::PushToTalk));
        draft.stop_waiting();
        assert_eq!((draft.waiting, draft.key_refused), (None, None));
        assert_eq!(draft.hotkeys, Bindings::default());
    }

    // Change on Mute, then Alt+Tab to the game or Win for Start: neither
    // becomes mute's key, and the row still waits until focus goes.
    #[test]
    fn a_key_windows_keeps_is_not_taken() {
        let mut draft = typed("");
        draft.change(Action::Mute);
        for text in ["Alt+Tab", "Left Win", "Win+G", "Ctrl+Esc"] {
            draft.take_chord(chord(text));
            assert_eq!(
                (draft.waiting, draft.key_refused),
                (Some(Action::Mute), None),
                "{text}"
            );
        }
        assert_eq!(draft.hotkeys, Bindings::default());
        draft.take_chord(chord("F9"));
        assert_eq!(draft.hotkeys.chord(Action::Mute), chord("F9"));
    }

    #[test]
    fn no_row_waits_when_the_hotkeys_did_not_start() {
        let mut draft = typed("");
        draft.hotkeys_off = Some(String::from("Hotkeys could not start."));
        draft.change(Action::Mute);
        assert_eq!(draft.waiting, None);
    }

    // While this PC is controlled no row waits for a key, and a key already
    // on its way when control began is not taken.
    #[test]
    fn locked_while_controlled() {
        let mut draft = typed("");
        draft.locked = true;
        draft.change(Action::Panic);
        assert_eq!(draft.waiting, None);
        draft.locked = false;
        draft.change(Action::Panic);
        assert_eq!(draft.waiting, Some(Action::Panic));
        draft.locked = true;
        draft.take_chord(chord("F9"));
        assert_eq!(draft.waiting, None);
        assert_eq!(draft.hotkeys, Bindings::default());
    }

    // The panic key has its settings row, and its key is refused for
    // another action as any other would be.
    #[test]
    fn panic_key_row() {
        let mut draft = typed("");
        assert_eq!(key_line(&draft, Action::Panic).0, "Ctrl+Shift+End");
        draft.change(Action::Mute);
        draft.take_chord(chord("Ctrl+Shift+End"));
        assert_eq!(draft.waiting, Some(Action::Mute));
        assert_eq!(
            refused_line(&draft, Action::Mute).as_deref(),
            Some("That key is already used for the panic key.")
        );
        draft.stop_waiting();
        draft.change(Action::Panic);
        draft.take_chord(chord("Ctrl+Alt+End"));
        assert_eq!(draft.waiting, None);
        let next = draft.settings(&Settings::default()).unwrap();
        assert_eq!(next.hotkeys().chord(Action::Panic), chord("Ctrl+Alt+End"));
    }

    // The narrowest window is 320 px at every Windows scale, and there the
    // settings list is always long enough for its scroll bar. A hotkey row
    // cuts its name with no tooltip, so a name longer than the room beside
    // Change would lose its end without anything saying so.
    #[test]
    fn hotkey_names_fit() {
        let size = vec2(320.0, 640.0);
        for scale in [1.0, 1.25, 1.5, 1.75, 2.0] {
            let ctx = Context::default();
            theme::apply(&ctx);
            ctx.enable_accesskit();
            let mut input = RawInput {
                screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), size)),
                ..RawInput::default()
            };
            input.viewports.insert(
                ViewportId::ROOT,
                ViewportInfo {
                    native_pixels_per_point: Some(scale),
                    ..ViewportInfo::default()
                },
            );
            let mut whole = Vec::new();
            let mut output = ctx.run_ui(input, |ui| {
                let column = size.x - 2.0 * SIDE - ui.spacing().scroll.allocated_width();
                whole = Action::ALL
                    .map(|action| {
                        let label = messages::hotkey_label(action);
                        let galley =
                            ui.painter()
                                .layout_no_wrap(label.to_owned(), theme::body(), CHALK);
                        (label, galley.size().x)
                    })
                    .to_vec();
                let rect = Rect::from_min_size(pos2(SIDE, 0.0), vec2(column, size.y));
                ui.scope_builder(UiBuilder::new().max_rect(rect), |ui| {
                    hotkeys(ui, &mut typed(""));
                });
            });
            let nodes = std::mem::take(&mut output.platform_output)
                .accesskit_update
                .expect("accesskit is on")
                .nodes;
            output.drop_without_applying_deltas();
            for (label, width) in whole {
                let shown = nodes
                    .iter()
                    .find(|(_, node)| node.value() == Some(label))
                    .and_then(|(_, node)| node.bounds())
                    .unwrap_or_else(|| panic!("no row for {label}"))
                    .width() as f32;
                assert!(
                    shown >= width - 0.5,
                    "{label} at {scale}: {shown} of {width} px shown"
                );
            }
        }
    }

    // 15 Mbit/s, vsync off and the strip shown until changed, and Save keeps
    // what was picked.
    #[test]
    fn sharing_choices_saved() {
        let mut draft = typed("");
        assert_eq!(
            (draft.upload_mbits, draft.vsync, draft.hide_strip),
            (15, false, false)
        );
        draft.upload_mbits = 30;
        draft.vsync = true;
        draft.hide_strip = true;
        let next = draft.settings(&Settings::default()).unwrap();
        assert_eq!(next.upload_mbits(), 30);
        assert!(next.vsync() && next.hide_strip());
        let again = Draft::new(&next, KnownDevices::default(), String::new());
        assert_eq!(
            (again.upload_mbits, again.vsync, again.hide_strip),
            (30, true, true)
        );
        // The monitor picked from inside a room is carried over as it was.
        let mut picked = Settings::default();
        picked.set_share_monitor(Some(r"\\.\DISPLAY2"));
        let kept = typed("").settings(&picked).unwrap();
        assert_eq!(kept.share_monitor(), Some(r"\\.\DISPLAY2"));
    }

    // Off until turned on, and Save keeps what was picked.
    #[test]
    fn update_check_saved() {
        let mut draft = typed("");
        assert!(!draft.check_for_new_versions);
        draft.check_for_new_versions = true;
        let next = draft.settings(&Settings::default()).unwrap();
        assert!(next.check_for_new_versions());
        let again = Draft::new(&next, KnownDevices::default(), String::new());
        assert!(again.check_for_new_versions);
    }

    #[test]
    fn hide_strip_lines() {
        assert_eq!(hide_strip_line(false, true), None);
        assert_eq!(hide_strip_line(false, false), None);
        assert_eq!(
            hide_strip_line(true, true),
            Some("Moving the mouse brings it back.")
        );
        assert_eq!(
            hide_strip_line(true, false),
            Some("Animation effects are off in Windows, so the strip stays.")
        );
    }

    #[test]
    fn the_meter_is_a_decibel_scale_from_minus_60() {
        assert_eq!(meter_fraction(1.0), 1.0);
        assert_eq!(meter_fraction(2.0), 1.0);
        assert!((meter_fraction(10f32.powf(-30.0 / 20.0)) - 0.5).abs() < 1e-5);
        assert!((meter_fraction(0.1) - 2.0 / 3.0).abs() < 1e-5);
        assert_eq!(meter_fraction(0.001), 0.0);
        assert_eq!(meter_fraction(0.0), 0.0);
        assert_eq!(meter_fraction(f32::NAN), 0.0);
    }
}
