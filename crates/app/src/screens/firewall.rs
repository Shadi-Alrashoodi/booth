use eframe::egui::Ui;

use crate::controls::{self, Button};
use crate::messages;
use crate::theme::{self, ASH, CHALK};

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

// Laid out like the start screen: the reason, then the two verbs. While the
// administrator prompt is open Allow gives way to a line saying so. Not now
// stays, for a prompt that went behind another window or a firewall that
// never answers the helper.
pub fn show(ui: &mut Ui, ask: &Ask, waiting: bool) -> Option<Answer> {
    controls::title_row(ui, "Booth", &[]);
    controls::page(ui, |ui| {
        if ask.blocking_all {
            controls::text(ui, messages::FIREWALL_BLOCKING_ALL, theme::body(), CHALK);
            ui.add_space(16.0);
            return Button::new("Continue")
                .show(ui)
                .clicked()
                .then_some(Answer::Continue);
        }
        let reason = if ask.standard_user {
            messages::FIREWALL_STANDARD_USER
        } else {
            messages::FIREWALL_ASK
        };
        controls::text(ui, reason, theme::body(), CHALK);
        if ask.blocked {
            ui.add_space(8.0);
            controls::text(ui, messages::FIREWALL_BLOCKED, theme::small(), ASH);
        }
        ui.add_space(16.0);
        if waiting {
            controls::text(ui, messages::FIREWALL_WAITING, theme::body(), ASH);
            ui.add_space(16.0);
        }
        let mut answer = None;
        ui.horizontal(|ui| {
            if !waiting && Button::new("Allow").show(ui).clicked() {
                answer = Some(Answer::Allow);
            }
            if Button::new("Not now").show(ui).clicked() {
                answer = Some(Answer::NotNow);
            }
        });
        answer
    })
}
