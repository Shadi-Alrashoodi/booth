use eframe::egui::{Label, RichText, Ui};

use crate::controls::{self, Button, Lead};
use crate::messages;
use crate::theme::{self, ASH, CHALK, FIELD_GAP, Role, STEP};

pub struct Ask {
    // Windows already has a Block rule for this exe, which Allow removes.
    pub blocked: bool,
    // This account cannot become administrator, so the prompt needs someone
    // else's password.
    pub standard_user: bool,
    // The firewall's block-all switch is on, which no rule gets past, so
    // there is nothing to ask an administrator for.
    pub blocking_all: bool,
}

pub enum Answer {
    Allow,
    NotNow,
    Continue,
}

// The mark alone in the title row, the reason, then the two verbs, and
// nothing else. While the administrator prompt is open its line takes
// Allow's place. Not now stays, for a prompt that went behind another window
// or a firewall that never answers the helper.
pub fn show(ui: &mut Ui, ask: &Ask, waiting: bool) -> Option<Answer> {
    controls::title_row(ui, Lead::Mark, &[]);
    controls::page(ui, |ui| {
        if ask.blocking_all {
            controls::prose(ui, messages::FIREWALL_BLOCKING_ALL, theme::body(), CHALK);
            ui.add_space(FIELD_GAP);
            return Button::new("Continue")
                .role(Role::Primary)
                .show(ui)
                .clicked()
                .then_some(Answer::Continue);
        }
        let reason = if ask.standard_user {
            messages::FIREWALL_STANDARD_USER
        } else {
            messages::FIREWALL_ASK
        };
        controls::prose(ui, reason, theme::body(), CHALK);
        if ask.blocked {
            ui.add_space(STEP);
            controls::prose(ui, messages::FIREWALL_BLOCKED, theme::caption(), ASH);
        }
        ui.add_space(FIELD_GAP);
        let mut answer = None;
        ui.horizontal(|ui| {
            if waiting {
                let line = RichText::new(messages::FIREWALL_WAITING)
                    .font(theme::body())
                    .color(ASH);
                ui.add(Label::new(line).extend());
            } else if Button::new("Allow").role(Role::Primary).show(ui).clicked() {
                answer = Some(Answer::Allow);
            }
            if Button::new("Not now").show(ui).clicked() {
                answer = Some(Answer::NotNow);
            }
        });
        answer
    })
}
