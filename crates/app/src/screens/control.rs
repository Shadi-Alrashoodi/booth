// Remote control in the panel: the Control button on the sharer's row, the
// request block above the people list, Stop control in the title row, the
// word "controlling" after a name, and the host's row menu with End
// control. The room decides everything; these only say what it decided and
// hand the clicks back.
//
// Each one also takes `offered`, the panel's one switch for remote control,
// which is on only while the rooms here get an injector. Without one none
// of this shows, whatever the view says: the room refuses a request for the
// host's own share, Decline turns down one for a client's, and it all comes
// back once the injector is there.

use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{Event, Frame, Key, Margin, Modifiers, Response, Ui, WidgetInfo, WidgetType};
use room::view::{LinkState, Person, Role, View};

use crate::controls::{self, Button};
use crate::messages;
use crate::remote::Flags;
use crate::theme::{self, ASH, CHALK, CONTROL_HEIGHT, HALF_STEP, PANEL, ROW_HEIGHT, SIDE, STEP};

// Allow waits this long after a request first shows, and again after
// another takes its place. The room already refuses an answer to a request
// that is no longer on show, but the panel can draw the new one just before
// a click aimed at the old one lands, and that click would allow whoever
// asked since.
pub const ALLOW_AFTER: Duration = Duration::from_millis(500);

// The panic key as the request block uses it: its words, which cannot
// change inside a room since settings cannot be opened there, and the cut
// that Allow clears just before the room hears of it.
pub struct PanicKey {
    pub words: String,
    pub flags: Arc<Flags>,
}

// The button on the sharer's row, after Watch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowControl {
    Control,
    Asked,
}

impl RowControl {
    pub fn button(self) -> Button<'static> {
        match self {
            RowControl::Control => Button::new("Control").height(ROW_HEIGHT),
            RowControl::Asked => Button::new("Asked")
                .color(ASH)
                .enabled(false)
                .height(ROW_HEIGHT),
        }
    }
}

// Control on the sharer's row, reading Asked in ash until the answer. Only
// while this PC watches the share, since the room asks only for a share it
// watches and control without the picture is blind. None while this PC
// controls it, when Stop control is in the title row, and while the host is
// lost.
pub fn row_control(view: &View, person: &Person, offered: bool) -> Option<RowControl> {
    if !offered {
        return None;
    }
    let current = view.share.current.as_ref()?;
    if person.is_you || current.yours || current.key != person.key || !view.share.watching {
        return None;
    }
    let control = &view.share.control;
    if host_lost(view) || control.controlling.is_some() {
        return None;
    }
    Some(if control.asking == Some(current.number) {
        RowControl::Asked
    } else {
        RowControl::Control
    })
}

// The word in ash after a name: controlling for whoever controls the
// current share, on every panel, and sharing for the sharer.
pub fn state_word(view: &View, person: &Person, offered: bool) -> Option<&'static str> {
    let control = &view.share.control;
    let controls =
        control.controller == Some(person.key) || (person.is_you && control.controlling.is_some());
    if offered && controls {
        Some("controlling")
    } else {
        person.sharing.then_some("sharing")
    }
}

// While you control someone, Stop control replaces Share.
pub fn stop_control_in_title(view: &View, offered: bool) -> bool {
    offered && view.share.control.controlling.is_some()
}

fn host_lost(view: &View) -> bool {
    view.role == Role::Client && matches!(view.strip.state, LinkState::Lost | LinkState::Closed)
}

// What the request block shows: the request on show, then once allowed who
// controls this PC.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Request<'a> {
    // `key` is the asker's as the roster has it now, all zeros when this PC
    // could not tell: a name alone is whatever that friend chose.
    Asked {
        number: u32,
        name: &'a str,
        key: [u8; 32],
    },
    Controlled {
        name: &'a str,
    },
}

pub fn request(view: &View, offered: bool) -> Option<Request<'_>> {
    if !offered {
        return None;
    }
    let control = &view.share.control;
    if let Some(party) = &control.controlled_by {
        return Some(Request::Controlled { name: &party.name });
    }
    control.asked_by.as_ref().map(|asked| Request::Asked {
        number: asked.number,
        name: &asked.name,
        key: asked.key,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    // Each names the request it answers, as drawn.
    Allow(u32),
    DontAllow(u32),
    Stop,
}

// When the request on show first showed, for ALLOW_AFTER.
#[derive(Debug, Default)]
pub struct AllowWait {
    shown: Option<(u32, Instant)>,
}

impl AllowWait {
    // Every frame in a room. How much longer Allow waits, or None when it
    // can be pressed or there is nothing to allow.
    pub fn follow(&mut self, request: Option<Request<'_>>, now: Instant) -> Option<Duration> {
        let number = match request {
            Some(Request::Asked { number, .. }) => number,
            Some(Request::Controlled { .. }) | None => {
                self.shown = None;
                return None;
            }
        };
        let since = match self.shown {
            Some((shown, since)) if shown == number => since,
            _ => {
                self.shown = Some((number, now));
                now
            }
        };
        ALLOW_AFTER
            .checked_sub(now.saturating_duration_since(since))
            .filter(|left| !left.is_zero())
    }
}

// With the switch off no request block shows, so nobody here can answer a
// request. The room refuses one for the host's own share by itself, but one
// for a client's share waits for an answer: the asker would read Asked until
// the share ends, and the host would hold it and turn every other ask away
// as busy. So each is declined once, as Don't allow would, which says
// nothing on this PC.
#[derive(Debug, Default)]
pub struct Decline {
    // The one declined already, until the view stops showing it.
    sent: Option<u32>,
}

impl Decline {
    // Every pass in a room, minimized too. The request to decline now.
    pub fn follow(&mut self, view: &View, offered: bool) -> Option<u32> {
        let asked = view.share.control.asked_by.as_ref().filter(|_| !offered);
        let Some(number) = asked.map(|asked| asked.number) else {
            self.sent = None;
            return None;
        };
        (self.sent.replace(number) != Some(number)).then_some(number)
    }
}

// The control request block, above the people list and over the stats
// panel too, since it is about who may use this PC: the request with
// Allow and Don't allow, and once allowed who controls it with Stop
// control. Nothing here is the Enter target: consent is never one stray
// key away. The panic key's line stays under both, as the quickest way out.
pub fn request_block(
    ui: &mut Ui,
    request: Request<'_>,
    panic_key: &str,
    allow_ready: bool,
) -> Option<Answer> {
    let mut answer = None;
    controls::region(ui, |ui| {
        match request {
            Request::Asked { number, name, key } => {
                controls::text(ui, messages::wants_control(name), theme::body(), CHALK);
                // Anyone in the room can take any name, so the fingerprint
                // says who asks, as on the people rows.
                if key != [0; 32] {
                    controls::text(ui, keys::fingerprint(&key), theme::mono(), ASH);
                }
                ui.add_space(STEP);
                // Both secondary and alike: consent to being controlled is
                // the one choice the panel must not lean on.
                let allow = Button::new("Allow").enabled(allow_ready);
                let buttons = [allow, Button::new("Don't allow")];
                answer = match controls::buttons(ui, &buttons, CONTROL_HEIGHT) {
                    Some(0) => Some(Answer::Allow(number)),
                    Some(_) => Some(Answer::DontAllow(number)),
                    None => None,
                };
            }
            Request::Controlled { name } => {
                controls::text(
                    ui,
                    messages::controlling_this_pc(name),
                    theme::body(),
                    CHALK,
                );
                ui.add_space(STEP);
                let stop = [Button::new("Stop control").role(theme::Role::Primary)];
                if controls::buttons(ui, &stop, CONTROL_HEIGHT).is_some() {
                    answer = Some(Answer::Stop);
                }
            }
        }
        ui.add_space(HALF_STEP);
        controls::text(ui, messages::panic_stops(panic_key), theme::caption(), ASH);
    });
    answer
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuItem {
    EndControl,
}

impl MenuItem {
    fn text(self) -> &'static str {
        match self {
            MenuItem::EndControl => "End control",
        }
    }
}

// The host's row menu, on the row of whoever controls the current share and
// of the one controlled. End control is all it has. A row with nothing in it
// has no menu, and is no Tab stop.
pub fn menu_items(view: &View, person: &Person, offered: bool) -> Vec<MenuItem> {
    if !offered || view.role != Role::Host || person.is_you {
        return Vec::new();
    }
    let Some(controller) = view.share.control.controller else {
        return Vec::new();
    };
    let sharer = view.share.current.as_ref().map(|share| share.key);
    if person.key == controller || sharer == Some(person.key) {
        vec![MenuItem::EndControl]
    } else {
        Vec::new()
    }
}

// Which row's menu is open. It opens under its row rather than floating
// over the list, so nothing covers the names, and it reads in Tab order
// right after the row it belongs to.
#[derive(Debug, Default)]
pub struct RowMenu {
    open: Option<[u8; 32]>,
    // Opened from the keyboard: its first item takes the focus.
    focus: bool,
}

impl RowMenu {
    // The row, once drawn with `row` sensing clicks: right-click, or Enter
    // or Shift+F10 while it has focus, opens its menu, and again closes it.
    // Windows' Menu key does not reach the panel (egui has no key for it),
    // so Shift+F10, which Windows treats as the same, stands in.
    pub fn follow_row(&mut self, ui: &Ui, row: &Response, key: [u8; 32], name: &str) {
        row.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, messages::row_menu(name)));
        if row.has_focus() && controls::keyboard_focus(ui) {
            // Inside the row, which is the list's full width.
            controls::ring_within(ui.painter(), row.rect, row.rect);
        }
        let keyboard = row.has_focus()
            && ui.input(|input| {
                input.events.iter().any(|event| match event {
                    Event::Key {
                        key: Key::Enter,
                        pressed: true,
                        ..
                    } => true,
                    Event::Key {
                        key: Key::F10,
                        pressed: true,
                        modifiers,
                        ..
                    } => modifiers.shift_only(),
                    _ => false,
                })
            });
        if !row.secondary_clicked() && !keyboard {
            return;
        }
        if self.open == Some(key) {
            self.open = None;
        } else {
            self.open = Some(key);
            self.focus = keyboard;
        }
    }

    // Each frame, with the rows that have a menu: one that left the room,
    // or has nothing left to offer, is not waiting open for its return.
    pub fn keep_only(&mut self, rows: &[[u8; 32]]) {
        if self.open.is_some_and(|open| !rows.contains(&open)) {
            self.open = None;
        }
    }

    // Under the open row. Esc closes it, and so does picking an item, or
    // the row losing every item (the control it would end has ended).
    pub fn show(&mut self, ui: &mut Ui, key: [u8; 32], items: &[MenuItem]) -> Option<MenuItem> {
        if self.open != Some(key) {
            return None;
        }
        if items.is_empty() || ui.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Escape))
        {
            self.open = None;
            return None;
        }
        let mut picked = None;
        let margin = Margin {
            left: SIDE as i8,
            right: SIDE as i8,
            top: 0,
            bottom: STEP as i8,
        };
        Frame::new()
            .fill(PANEL)
            .inner_margin(margin)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    for item in items {
                        let response = Button::new(item.text()).height(ROW_HEIGHT).show(ui);
                        if std::mem::take(&mut self.focus) {
                            response.request_focus();
                        }
                        if response.clicked() {
                            picked = Some(*item);
                        }
                    }
                });
            });
        if picked.is_some() {
            self.open = None;
        }
        picked
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controls::tests::{frame, press, release};
    use eframe::egui::{Context, PointerButton, Pos2, Rect, Sense, pos2, vec2};
    use room::view::{ControlRequest, ControlView, CurrentShare, Party, ShareView, Strip};

    const MARA: [u8; 32] = [7; 32];
    const TOM: [u8; 32] = [9; 32];
    const YOU: [u8; 32] = [1; 32];

    // The switch: on once the app has an injector, off before.
    const ON: bool = true;
    const OFF: bool = false;

    fn person(key: [u8; 32], name: &str) -> Person {
        Person {
            key,
            name: name.to_owned(),
            fingerprint: String::from("a7f3 9c21 0d4e"),
            rtt_ms: None,
            rtt_level: Default::default(),
            is_you: key == YOU,
            is_host: false,
            joined_by_invite: false,
            reconnecting: false,
            talking: false,
            sharing: key == MARA,
        }
    }

    // Mara shares, share number 7, and this PC is a client watching it
    // over a live link, unless the control view says more.
    fn view(role: Role, watching: bool, control: ControlView) -> View {
        View {
            role,
            room_name: String::from("Tuesday night"),
            strip: Strip {
                state: LinkState::Live,
                ..Strip::default()
            },
            people: vec![person(MARA, "Mara"), person(TOM, "Tom"), person(YOU, "You")],
            invite: None,
            numbers: Default::default(),
            chat: Default::default(),
            notice: None,
            reply: None,
            paste: None,
            address_changed: None,
            list_problem: None,
            voice: Default::default(),
            share: ShareView {
                current: Some(CurrentShare {
                    key: MARA,
                    name: String::from("Mara"),
                    number: 7,
                    fps: 120,
                    yours: false,
                    watchers: None,
                }),
                watching,
                control,
                ..ShareView::default()
            },
        }
    }

    fn party(key: [u8; 32], name: &str) -> Party {
        Party {
            key,
            name: name.to_owned(),
        }
    }

    // This PC's own share, as the one asked or controlled.
    fn own_share(control: ControlView) -> View {
        let mut view = view(Role::Client, false, control);
        if let Some(current) = &mut view.share.current {
            current.key = YOU;
            current.name = String::from("You");
            current.yours = true;
        }
        view
    }

    fn asked_by(number: u32, key: [u8; 32], name: &str) -> ControlView {
        ControlView {
            asked_by: Some(ControlRequest {
                number,
                key,
                name: name.to_owned(),
            }),
            ..ControlView::default()
        }
    }

    // The sharer's row reads Asked in ash until answered, and Control again
    // after Don't allow.
    #[test]
    fn control_on_the_sharer_s_row() {
        let mara = person(MARA, "Mara");
        let idle = view(Role::Client, true, ControlView::default());
        assert_eq!(row_control(&idle, &mara, ON), Some(RowControl::Control));
        let asking = view(
            Role::Client,
            true,
            ControlView {
                asking: Some(7),
                ..ControlView::default()
            },
        );
        assert_eq!(row_control(&asking, &mara, ON), Some(RowControl::Asked));
        // An ask for a share that has since ended is not this one's.
        let stale = view(
            Role::Client,
            true,
            ControlView {
                asking: Some(6),
                ..ControlView::default()
            },
        );
        assert_eq!(row_control(&stale, &mara, ON), Some(RowControl::Control));
        // Not watching: the room would not send the ask.
        let not_watching = view(Role::Client, false, ControlView::default());
        assert_eq!(row_control(&not_watching, &mara, ON), None);
        // Only on the sharer's row, and never on your own.
        assert_eq!(row_control(&idle, &person(TOM, "Tom"), ON), None);
        assert_eq!(row_control(&idle, &person(YOU, "You"), ON), None);
        // A lost host could not pass the ask on.
        let lost = View {
            strip: Strip {
                state: LinkState::Lost,
                ..Strip::default()
            },
            ..idle.clone()
        };
        assert_eq!(row_control(&lost, &mara, ON), None);
    }

    // Your row says controlling and Stop control replaces Share; the row's
    // button goes, and every panel shows the word on the controller's row.
    #[test]
    fn controlling_someone() {
        let controlling = view(
            Role::Client,
            true,
            ControlView {
                controller: Some(YOU),
                controlling: Some(party(MARA, "Mara")),
                ..ControlView::default()
            },
        );
        let (mara, you) = (person(MARA, "Mara"), person(YOU, "You"));
        assert!(stop_control_in_title(&controlling, ON));
        assert_eq!(row_control(&controlling, &mara, ON), None);
        assert_eq!(state_word(&controlling, &you, ON), Some("controlling"));
        assert_eq!(state_word(&controlling, &mara, ON), Some("sharing"));
        assert_eq!(state_word(&controlling, &person(TOM, "Tom"), ON), None);

        // Seen from Tom's panel: Tom controls, and nobody here does.
        let tom_controls = view(
            Role::Client,
            true,
            ControlView {
                controller: Some(TOM),
                ..ControlView::default()
            },
        );
        assert!(!stop_control_in_title(&tom_controls, ON));
        assert_eq!(
            state_word(&tom_controls, &person(TOM, "Tom"), ON),
            Some("controlling")
        );
        assert_eq!(state_word(&tom_controls, &you, ON), None);

        let idle = view(Role::Client, true, ControlView::default());
        assert!(!stop_control_in_title(&idle, ON));
        assert_eq!(state_word(&idle, &mara, ON), Some("sharing"));
    }

    #[test]
    fn request_then_controlled() {
        assert_eq!(request(&own_share(ControlView::default()), ON), None);
        let asked = own_share(asked_by(3, TOM, "Tom"));
        assert_eq!(
            request(&asked, ON),
            Some(Request::Asked {
                number: 3,
                name: "Tom",
                key: TOM,
            })
        );
        let controlled = own_share(ControlView {
            controller: Some(TOM),
            controlled_by: Some(party(TOM, "Tom")),
            ..ControlView::default()
        });
        assert_eq!(
            request(&controlled, ON),
            Some(Request::Controlled { name: "Tom" })
        );
        // An administrator window in front changes nothing here.
        let paused = own_share(ControlView {
            admin_here: true,
            ..controlled.share.control.clone()
        });
        assert_eq!(
            request(&paused, ON),
            Some(Request::Controlled { name: "Tom" })
        );
    }

    #[test]
    fn allow_waits() {
        let t = Instant::now();
        let at = |ms| t + Duration::from_millis(ms);
        let tom = Request::Asked {
            number: 3,
            name: "Tom",
            key: TOM,
        };
        let jonas = Request::Asked {
            number: 4,
            name: "Jonas",
            key: [5; 32],
        };
        let mut wait = AllowWait::default();
        assert_eq!(wait.follow(None, t), None);
        assert_eq!(wait.follow(Some(tom), t), Some(ALLOW_AFTER));
        assert_eq!(
            wait.follow(Some(tom), at(300)),
            Some(Duration::from_millis(200))
        );
        assert_eq!(wait.follow(Some(tom), at(500)), None);
        assert_eq!(wait.follow(Some(tom), at(2_000)), None);
        // Another request in its place: the wait starts again.
        assert_eq!(wait.follow(Some(jonas), at(2_100)), Some(ALLOW_AFTER));
        assert_eq!(
            wait.follow(Some(jonas), at(2_599)),
            Some(Duration::from_millis(1))
        );
        assert_eq!(wait.follow(Some(jonas), at(2_600)), None);
        // Gone and back with the same number is a fresh showing.
        assert_eq!(wait.follow(None, at(3_000)), None);
        assert_eq!(wait.follow(Some(jonas), at(3_100)), Some(ALLOW_AFTER));
        // Allowed: nothing waits.
        let controlled = Request::Controlled { name: "Jonas" };
        assert_eq!(wait.follow(Some(controlled), at(3_200)), None);
    }

    // With the switch off a request nobody can see is declined once, and so
    // is each one that takes its place. With it on, the block shows it and
    // the owner answers.
    #[test]
    fn with_the_switch_off_each_request_is_declined_once() {
        let none = own_share(ControlView::default());
        let tom = own_share(asked_by(3, TOM, "Tom"));
        let jonas = own_share(asked_by(4, [5; 32], "Jonas"));
        let mut decline = Decline::default();
        assert_eq!(decline.follow(&none, OFF), None);
        assert_eq!(decline.follow(&tom, OFF), Some(3));
        // Still on show until the room hears the answer.
        assert_eq!(decline.follow(&tom, OFF), None);
        assert_eq!(decline.follow(&jonas, OFF), Some(4));
        assert_eq!(decline.follow(&jonas, OFF), None);
        // Gone and back with the same number is another request.
        assert_eq!(decline.follow(&none, OFF), None);
        assert_eq!(decline.follow(&jonas, OFF), Some(4));

        let mut decline = Decline::default();
        for view in [&none, &tom, &jonas, &tom] {
            assert_eq!(decline.follow(view, ON), None);
        }
    }

    // Where a widget with this accessible name was drawn, from a frame with
    // accesskit on.
    fn place(output: &eframe::egui::PlatformOutput, label: &str) -> Pos2 {
        let bounds = output
            .accesskit_update
            .as_ref()
            .expect("accesskit is on")
            .nodes
            .iter()
            .find(|(_, node)| node.label() == Some(label))
            .and_then(|(_, node)| node.bounds())
            .unwrap_or_else(|| panic!("nothing called {label}"));
        pos2(
            ((bounds.x0 + bounds.x1) / 2.0) as f32,
            ((bounds.y0 + bounds.y1) / 2.0) as f32,
        )
    }

    fn click(
        ctx: &Context,
        at: Pos2,
        mut draw: impl FnMut(&mut Ui) -> Option<Answer>,
    ) -> Option<Answer> {
        let (pressed, _) = frame(ctx, press(at), &mut draw);
        let (released, _) = frame(ctx, release(at), &mut draw);
        pressed.or(released)
    }

    fn context() -> Context {
        let ctx = Context::default();
        theme::apply(&ctx);
        ctx.enable_accesskit();
        ctx
    }

    // Allow and Don't allow answer the request as drawn, by its number, and
    // Allow does nothing while it waits out ALLOW_AFTER.
    #[test]
    fn the_request_block_answers_the_request_it_shows() {
        let ctx = context();
        let tom = Request::Asked {
            number: 3,
            name: "Tom",
            key: TOM,
        };
        let draw = |ready| move |ui: &mut Ui| request_block(ui, tom, "Ctrl+Shift+End", ready);
        let (_, output) = frame(&ctx, Vec::new(), draw(false));
        let (allow, dont) = (place(&output, "Allow"), place(&output, "Don't allow"));
        assert!(allow.x < dont.x && (allow.y - dont.y).abs() < 0.5);
        let nodes = &output.accesskit_update.as_ref().unwrap().nodes;
        let values: Vec<String> = nodes
            .iter()
            .filter_map(|(_, node)| node.value().map(str::to_owned))
            .collect();
        assert!(
            values
                .iter()
                .any(|v| v == "\u{2068}Tom\u{2069} wants to control your screen."),
            "{values:?}"
        );
        assert!(
            values
                .iter()
                .any(|v| v == "Ctrl+Shift+End stops it at any time."),
            "{values:?}"
        );
        assert_eq!(click(&ctx, allow, draw(false)), None, "Allow still waits");
        assert_eq!(click(&ctx, allow, draw(true)), Some(Answer::Allow(3)));
        assert_eq!(click(&ctx, dont, draw(false)), Some(Answer::DontAllow(3)));
        assert_eq!(click(&ctx, dont, draw(true)), Some(Answer::DontAllow(3)));
    }

    #[test]
    fn block_once_allowed() {
        let ctx = context();
        let draw =
            |ui: &mut Ui| request_block(ui, Request::Controlled { name: "Tom" }, "F9", false);
        let (_, output) = frame(&ctx, Vec::new(), draw);
        let stop = place(&output, "Stop control");
        let nodes = &output.accesskit_update.as_ref().unwrap().nodes;
        assert!(
            nodes.iter().any(
                |(_, node)| node.value() == Some("\u{2068}Tom\u{2069} is controlling this PC.")
            )
        );
        assert!(
            nodes
                .iter()
                .any(|(_, node)| node.value() == Some("F9 stops it at any time."))
        );
        assert!(nodes.iter().all(|(_, node)| node.label() != Some("Allow")));
        // Stop control never waits.
        assert_eq!(click(&ctx, stop, draw), Some(Answer::Stop));
    }

    // End control is in the host's row menu, while that person is
    // controlling or being controlled.
    #[test]
    fn end_control_on_both_rows() {
        let control = ControlView {
            controller: Some(TOM),
            ..ControlView::default()
        };
        let host = view(Role::Host, false, control.clone());
        assert_eq!(
            menu_items(&host, &person(TOM, "Tom"), ON),
            [MenuItem::EndControl]
        );
        assert_eq!(
            menu_items(&host, &person(MARA, "Mara"), ON),
            [MenuItem::EndControl]
        );
        assert!(menu_items(&host, &person(YOU, "You"), ON).is_empty());
        let jonas = person([5; 32], "Jonas");
        assert!(menu_items(&host, &jonas, ON).is_empty());
        // Nobody controls: nothing to end.
        let idle = view(Role::Host, false, ControlView::default());
        assert!(menu_items(&idle, &person(MARA, "Mara"), ON).is_empty());
        // A client's panel has no row menu yet.
        let client = view(Role::Client, false, control);
        assert!(menu_items(&client, &person(TOM, "Tom"), ON).is_empty());
    }

    // With the switch off the panel offers no control, even where the view
    // says someone asks, controls or is controlled. With it on, each of
    // these views shows something of it.
    #[test]
    fn nothing_shows_with_the_switch_off() {
        let people = [person(MARA, "Mara"), person(TOM, "Tom"), person(YOU, "You")];
        let controlling = ControlView {
            controller: Some(YOU),
            controlling: Some(party(MARA, "Mara")),
            ..ControlView::default()
        };
        let views = [
            view(Role::Client, true, ControlView::default()),
            view(
                Role::Client,
                true,
                ControlView {
                    asking: Some(7),
                    ..ControlView::default()
                },
            ),
            view(Role::Client, true, controlling.clone()),
            view(
                Role::Host,
                false,
                ControlView {
                    controller: Some(TOM),
                    ..ControlView::default()
                },
            ),
            own_share(asked_by(3, TOM, "Tom")),
            own_share(ControlView {
                controller: Some(TOM),
                controlled_by: Some(party(TOM, "Tom")),
                ..ControlView::default()
            }),
        ];
        let shows = |view: &View, offered| {
            stop_control_in_title(view, offered)
                || request(view, offered).is_some()
                || people.iter().any(|person| {
                    row_control(view, person, offered).is_some()
                        || !menu_items(view, person, offered).is_empty()
                        || state_word(view, person, offered) == Some("controlling")
                })
        };
        for view in &views {
            assert!(shows(view, ON), "{:?}", view.share.control);
            assert!(!shows(view, OFF), "{:?}", view.share.control);
        }
        // The sharer's word is not control's, and stays.
        let controlling = view(Role::Client, true, controlling);
        assert_eq!(state_word(&controlling, &people[0], OFF), Some("sharing"));
        assert_eq!(state_word(&controlling, &people[2], OFF), None);
    }

    const ROW: Rect = Rect {
        min: Pos2 { x: 0.0, y: 0.0 },
        max: Pos2 {
            x: 360.0,
            y: ROW_HEIGHT,
        },
    };

    // One person's row and, under it, its menu when open.
    fn row_and_menu(ui: &mut Ui, menu: &mut RowMenu) -> Option<MenuItem> {
        let (rect, _) = ui.allocate_exact_size(vec2(360.0, ROW_HEIGHT), Sense::hover());
        let row = ui.interact(rect, ui.id().with("tom's row"), Sense::click());
        menu.follow_row(ui, &row, TOM, "Tom");
        menu.show(ui, TOM, &[MenuItem::EndControl])
    }

    fn secondary(at: Pos2, pressed: bool) -> Event {
        Event::PointerButton {
            pos: at,
            button: PointerButton::Secondary,
            pressed,
            modifiers: Modifiers::NONE,
        }
    }

    fn key(key: Key, modifiers: Modifiers) -> Event {
        Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        }
    }

    #[test]
    fn row_menu_right_click() {
        let ctx = context();
        let mut menu = RowMenu::default();
        let on_row = ROW.center();
        frame(&ctx, Vec::new(), |ui| row_and_menu(ui, &mut menu));
        assert_ne!(menu.open, Some(TOM));
        let right_click = vec![Event::PointerMoved(on_row), secondary(on_row, true)];
        frame(&ctx, right_click, |ui| row_and_menu(ui, &mut menu));
        let (_, output) = frame(&ctx, vec![secondary(on_row, false)], |ui| {
            row_and_menu(ui, &mut menu)
        });
        assert_eq!(menu.open, Some(TOM));
        let end = place(&output, "End control");
        assert!(end.y > ROW.bottom(), "{end:?}");
        frame(&ctx, press(end), |ui| row_and_menu(ui, &mut menu));
        let (picked, _) = frame(&ctx, release(end), |ui| row_and_menu(ui, &mut menu));
        assert_eq!(picked, Some(MenuItem::EndControl));
        assert_ne!(menu.open, Some(TOM));
    }

    #[test]
    fn row_menu_keys() {
        for opener in [
            key(Key::Enter, Modifiers::NONE),
            key(Key::F10, Modifiers::SHIFT),
        ] {
            let ctx = context();
            let mut menu = RowMenu::default();
            frame(&ctx, Vec::new(), |ui| row_and_menu(ui, &mut menu));
            // Tab reaches the row: it is the only focusable thing here.
            frame(&ctx, vec![key(Key::Tab, Modifiers::NONE)], |ui| {
                row_and_menu(ui, &mut menu)
            });
            frame(&ctx, vec![opener.clone()], |ui| row_and_menu(ui, &mut menu));
            assert_eq!(menu.open, Some(TOM), "{opener:?}");
            // The first item takes the focus once drawn.
            frame(&ctx, Vec::new(), |ui| row_and_menu(ui, &mut menu));
            assert!(ctx.memory(|memory| memory.focused().is_some()));
            frame(&ctx, vec![key(Key::Escape, Modifiers::NONE)], |ui| {
                row_and_menu(ui, &mut menu)
            });
            assert_ne!(menu.open, Some(TOM), "{opener:?}");
        }
    }

    #[test]
    fn a_menu_with_nothing_left_in_it_closes() {
        let ctx = context();
        let mut menu = RowMenu {
            open: Some(TOM),
            focus: false,
        };
        let (picked, _) = frame(&ctx, Vec::new(), |ui| menu.show(ui, TOM, &[]));
        assert_eq!(picked, None);
        assert_ne!(menu.open, Some(TOM));
    }
}
