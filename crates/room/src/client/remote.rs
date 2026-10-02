// A client's part in remote control: it asks the host for control of the
// share it watches and hears the answer, and when it shares, it shows the
// request, answers it, and puts the input the host passes on through the
// app's injector. The host is only a friend's PC too: input is taken only
// from the slot this PC's owner allowed, for this PC's own share.

use std::time::Instant;

use super::Client;
use crate::control::{ControlAnswer, Message};
use crate::log::log;
use crate::remote::wire::{self, RELAYED, SENT};
use crate::remote::{
    self, Arrival, ControlEnd, ControlLink, NO_INJECTOR_LINE, NotTaken, Remote, Seat,
};
use crate::socket::Socket;
use crate::talk;
use crate::view::{ControlView, LinkState};

impl Client {
    pub(crate) fn remote(&self) -> &Remote {
        &self.screen.remote
    }

    // The panel's Control, on the share this PC watches. Refused here while
    // the roster shows someone else controlling it; the host refuses a race
    // it sees.
    pub(crate) fn ask_control(&mut self, share: u32, now: Instant, socket: &Socket) -> bool {
        if self.state != LinkState::Live || self.sessions.current.is_none() {
            return false;
        }
        if self.screen.watched != Some(share) || self.screen.remote.there().is_some() {
            return false;
        }
        let own = *self.identity.public();
        let Some(sharer) = self
            .roster
            .entries
            .iter()
            .find(|entry| entry.share.is_some_and(|live| live.number == share))
            .filter(|entry| entry.key != own)
        else {
            return false;
        };
        let (sharer, sharer_name) = (sharer.key, sharer.name.clone());
        // A roster that still names this PC is from before it let go.
        if let Some(controller) = self
            .roster
            .entries
            .iter()
            .find(|entry| entry.controlling && entry.key != own)
        {
            let (key, name) = (controller.key, controller.name.clone());
            self.system_line(key, name.clone(), remote::busy_line(&name));
            return true;
        }
        let ask = self.screen.remote.ask(share, sharer, sharer_name);
        log!(
            self.log,
            "control: asked to control share {share} of {}, ask {ask}",
            keys::fingerprint(&sharer)
        );
        self.link.queue(&Message::ControlAsk { share, ask });
        self.flush(now, socket);
        true
    }

    // The panel's Allow and Don't allow, on this PC's own share, for the
    // request numbered `number`.
    pub(crate) fn answer_control(
        &mut self,
        number: u32,
        allow: bool,
        now: Instant,
        socket: &Socket,
    ) -> bool {
        let Some(here) = self.screen.remote.request(number) else {
            log!(
                self.log,
                "control {number}: answered once that request was no longer on show, nothing done"
            );
            return false;
        };
        let (share, controller, name) = (here.share, here.controller, here.name.clone());
        let can = self.screen.remote.gate.can_inject();
        if allow && !can {
            self.problem_line(String::from(NO_INJECTOR_LINE));
        }
        let answer = if allow && can {
            let area = self.screen.sharing.area();
            self.screen.remote.allow(area);
            log!(
                self.log,
                "control {number}: allowed {} to control this pc",
                keys::fingerprint(&controller)
            );
            let own = self.name.clone();
            self.system_line(
                controller,
                name.clone(),
                remote::started_line(Seat::Controlled, &name, &own),
            );
            ControlAnswer::Allow
        } else {
            self.screen.remote.end_here(ControlEnd::Stopped, now);
            log!(self.log, "control {number}: not allowed");
            ControlAnswer::DontAllow
        };
        self.link.queue(&Message::ControlAnswer {
            share,
            number,
            answer,
        });
        self.flush(now, socket);
        true
    }

    // The panel's Stop control and the panic key, whichever side of a
    // control this PC is on. A request on show is declined.
    pub(crate) fn stop_control(&mut self, panic: bool, now: Instant, socket: &Socket) -> bool {
        if let Some(here) = self.screen.remote.here() {
            let (share, number) = (here.share, here.number);
            if !here.allowed {
                return self.answer_control(number, false, now, socket);
            }
            let why = if panic {
                ControlEnd::Panic
            } else {
                ControlEnd::Stopped
            };
            self.end_here(why, now);
            self.link.queue(&Message::ControlEnd { share, number, why });
            self.flush(now, socket);
            return true;
        }
        self.release(now, socket)
    }

    // This PC lets go of the share it controls, or takes back its ask.
    fn release(&mut self, now: Instant, socket: &Socket) -> bool {
        let Some(there) = self.screen.remote.there() else {
            return false;
        };
        let (share, ask) = (there.share, there.ask);
        self.end_there(ControlEnd::Released);
        if self.sessions.current.is_some() {
            self.link.queue(&Message::ControlEnd {
                share,
                number: ask,
                why: ControlEnd::Released,
            });
            self.flush(now, socket);
        }
        true
    }

    // What this PC's injector found: an administrator window came to the
    // front or went, and the controller hears of it through the host.
    pub(crate) fn control_work(&mut self, now: Instant, socket: &Socket) -> bool {
        let Some((share, number, paused)) = self.screen.remote.admin_news() else {
            return false;
        };
        log!(
            self.log,
            "control {number}: an administrator window in front on this pc: {}",
            crate::log::yes_no(paused)
        );
        self.link.queue(&Message::ControlPaused {
            share,
            number,
            paused,
        });
        self.flush(now, socket);
        true
    }

    // Input the host passed on, for this PC's own share, from the slot its
    // owner allowed. It never changes the view.
    pub(super) fn on_input(&mut self, payload: &[u8], now: Instant) -> bool {
        let head = match wire::read_input(payload, RELAYED, &mut self.input_events) {
            Ok(head) => head,
            Err(why) => return self.refused_input(&why.to_string(), now),
        };
        let Some(share) = self.screen.sharing.number() else {
            remote::wipe(&mut self.input_events);
            // Passed on before the host heard the share end.
            if self.screen.remote.just_ended(None, head.slot, now) {
                return false;
            }
            return self.refused_input("this pc shares nothing", now);
        };
        // Its time came through the host, so it rests on two clock offsets:
        // "about" if either link is jittery, as for voice.
        let offset = self.link.offset.best().map(|sample| sample.offset_us);
        let captured = head.captured.and_then(|at| {
            let (at, jittery) = talk::our_time(at, offset, self.jittery);
            at.map(|at| (at, head.about || jittery))
        });
        let arrival = Arrival {
            share,
            slot: head.slot,
            captured,
            area: self.screen.sharing.area(),
            now,
        };
        match self
            .screen
            .remote
            .input(arrival, &head, &mut self.input_events)
        {
            Ok(()) | Err(NotTaken::Late | NotTaken::JustEnded) => false,
            Err(NotTaken::NotAllowed) => self.refused_input("nobody here allowed it", now),
        }
    }

    fn refused_input(&mut self, why: &str, now: Instant) -> bool {
        if self.log.is_on()
            && let Some(from) = self.host_addr
            && self.strays.allow(from.ip(), now, &self.log)
        {
            self.log
                .line(format!("input from the host, dropped: {why}"));
        }
        self.drops.bad(now);
        false
    }

    // The control messages from the host. True when the view changed.
    pub(super) fn on_control_message(&mut self, message: Message, now: Instant) -> bool {
        match message {
            Message::ControlAsked {
                share,
                control,
                slot,
                name,
            } => {
                if self.screen.sharing.number() != Some(share) {
                    return false;
                }
                // A friend who joined a moment ago may not be in this PC's
                // roster yet; the slot is what their input is checked by.
                let own = *self.identity.public();
                let entry = self.roster.entries.iter().find(|entry| entry.slot == slot);
                if entry.is_some_and(|entry| entry.key == own) {
                    self.drops.bad(now);
                    return false;
                }
                let controller = entry.map_or([0; 32], |entry| entry.key);
                log!(
                    self.log,
                    "control {control}: {} asks to control this pc's share {share}",
                    keys::fingerprint(&controller)
                );
                self.screen
                    .remote
                    .asked(control, share, controller, slot, name, now);
                true
            }
            Message::ControlAnswer {
                share,
                number,
                answer,
            } => self.control_answered(share, number, answer),
            Message::ControlEnd { share, number, why } => {
                self.control_ended(share, number, why, now)
            }
            Message::ControlPaused {
                share,
                number,
                paused,
            } => self.screen.remote.set_paused(share, number, paused),
            // Only a client asks.
            _ => {
                self.drops.bad(now);
                false
            }
        }
    }

    fn control_answered(&mut self, share: u32, ask: u32, answer: ControlAnswer) -> bool {
        let Some(there) = self
            .screen
            .remote
            .there()
            .filter(|there| there.share == share && there.ask == ask && !there.allowed)
        else {
            return false;
        };
        let (sharer, sharer_name) = (there.sharer, there.name.clone());
        let line = match answer {
            ControlAnswer::Allow => {
                self.screen.remote.allowed(share, ask);
                log!(self.log, "control: ask {ask} allowed");
                let own = self.name.clone();
                remote::started_line(Seat::Controller, &own, &sharer_name)
            }
            ControlAnswer::DontAllow => {
                self.screen.remote.end_there();
                log!(self.log, "control: ask {ask} not allowed");
                remote::not_allowed_line(&sharer_name)
            }
            ControlAnswer::Busy { key, name } => {
                self.screen.remote.end_there();
                log!(
                    self.log,
                    "control: ask {ask} refused, {} asked first",
                    keys::fingerprint(&key)
                );
                remote::busy_line(&name)
            }
            ControlAnswer::TooSoon => {
                self.screen.remote.end_there();
                log!(self.log, "control: ask {ask} refused, asked too often");
                String::from(remote::TOO_SOON_LINE)
            }
        };
        self.system_line(sharer, sharer_name, line);
        true
    }

    fn control_ended(&mut self, share: u32, number: u32, why: ControlEnd, now: Instant) -> bool {
        let to_here = matches!(
            why,
            ControlEnd::Released
                | ControlEnd::EndedByHost
                | ControlEnd::ShareEnded
                | ControlEnd::SessionLost
        );
        let to_there = matches!(
            why,
            ControlEnd::Stopped
                | ControlEnd::Panic
                | ControlEnd::EndedByHost
                | ControlEnd::ShareEnded
                | ControlEnd::SessionLost
        );
        let here = self
            .screen
            .remote
            .here()
            .is_some_and(|here| here.share == share && here.number == number);
        let there = self
            .screen
            .remote
            .there()
            .is_some_and(|there| there.share == share && there.ask == number);
        if here && to_here {
            log!(self.log, "control {number}: over, {why:?}");
            self.end_here(why, now);
            return true;
        }
        if there && to_there {
            log!(self.log, "control: ask {number} over, {why:?}");
            self.end_there(why);
            return true;
        }
        // An end the host never sends to this side.
        if (here && !to_here) || (there && !to_there) {
            self.drops.bad(now);
        }
        false
    }

    // Control of this PC ends, with its line when it had started.
    fn end_here(&mut self, why: ControlEnd, now: Instant) {
        let Some(here) = self.screen.remote.end_here(why, now) else {
            return;
        };
        if here.allowed {
            let own = self.name.clone();
            let line = remote::ended_line(why, Seat::Controlled, &here.name, &own);
            self.system_line(here.controller, here.name, line);
        }
    }

    fn end_there(&mut self, why: ControlEnd) {
        let Some(there) = self.screen.remote.end_there() else {
            return;
        };
        if there.allowed {
            let own = self.name.clone();
            let line = remote::ended_line(why, Seat::Controller, &own, &there.name);
            self.system_line(there.sharer, there.name, line);
        }
    }

    // Every timer pass: CUTOFF without input lets go of what this PC's
    // injector holds.
    pub(super) fn control_step(&mut self, now: Instant) {
        if self.screen.remote.step(now) {
            log!(
                self.log,
                "control: no input for {} ms, everything held let go",
                remote::CUTOFF.as_millis()
            );
        }
    }

    // A new roster: the share this PC controls or asked about may be over.
    pub(super) fn control_roster(&mut self) {
        let Some(share) = self.screen.remote.there().map(|there| there.share) else {
            return;
        };
        let live = self
            .roster
            .entries
            .iter()
            .any(|entry| entry.share.is_some_and(|live| live.number == share));
        if !live {
            self.end_there(ControlEnd::ShareEnded);
        }
    }

    // This PC stopped sharing: control of its share is over. The host hears
    // the share end and tells the controller.
    pub(super) fn control_share_ended(&mut self, now: Instant) {
        self.end_here(ControlEnd::ShareEnded, now);
    }

    // This PC stopped watching the share it controls: it lets go.
    pub(super) fn control_stopped_watching(&mut self, now: Instant, socket: &Socket) {
        self.release(now, socket);
    }

    // The session ended, or the room: control either way ends with it.
    pub(super) fn control_over(&mut self, why: ControlEnd) {
        self.end_here(why, Instant::now());
        self.end_there(why);
    }

    // Where this PC's controller packets go: the host's current session,
    // while it controls someone's share.
    pub(crate) fn publish_control(&mut self) {
        let session = self
            .sessions
            .current
            .as_ref()
            .filter(|session| self.state != LinkState::Closed && session.is_confirmed());
        let to = self.media.map(|media| media.to());
        let link = session
            .zip(to)
            .map(|(session, to)| (session.remote_index(), to, 0));
        self.screen.remote.publish(link, || {
            let (session, to) = session.zip(to)?;
            Some(ControlLink {
                sealer: session.sealer()?,
                to,
                kind: SENT,
                slot: 0,
            })
        });
    }

    pub(super) fn control_view(&self) -> ControlView {
        let controller = self
            .roster
            .entries
            .iter()
            .find(|entry| entry.controlling)
            .map(|entry| entry.key);
        self.screen.remote.view(controller)
    }
}
