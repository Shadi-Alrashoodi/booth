// The host's part in remote control: it keeps the one grant a share can
// have, carries the ask, the answer and every end between the controller and
// the sharer, and passes each input packet on to the sharer the moment it
// arrives, from the controller the sharer allowed and nobody else.
// All of it came from a friend's PC, the controller's included, and is
// hostile input: it is checked here, and the sharer checks again what
// reaches it.

use std::time::Instant;

use zeroize::Zeroize;

use super::{Host, who};
use crate::control::{ControlAnswer, Message};
use crate::limit::Bucket;
use crate::log::log;
use crate::remote::wire::{self, RELAYED, SENT};
use crate::remote::{
    self, ASK_BURST, ASKS_PER_SECOND, Arrival, ControlEnd, ControlLink, ENDED_GRACE, INPUT_BURST,
    INPUT_PER_SECOND, NO_INJECTOR_LINE, Remote, Seat,
};
use crate::socket::Socket;
use crate::talk::{self, HOST_SLOT};
use crate::view::ControlView;

// The share's one control session, asked for or allowed. It lives in the
// share, so it ends with it.
pub(super) struct Grant {
    // This host's number for it, which the sharer's answer and ends name.
    pub number: u32,
    pub controller: [u8; 32],
    // The controller's name when it asked, for the lines after it left.
    name: String,
    // The controller's own number for its ask, which it hears back.
    ask: u32,
    pub allowed: bool,
}

// What a friend may send of remote control.
pub(super) struct PeerControl {
    input: Bucket,
    asks: Bucket,
}

impl PeerControl {
    pub(super) fn new(now: Instant) -> PeerControl {
        PeerControl {
            input: Bucket::full(now, INPUT_BURST),
            asks: Bucket::full(now, ASK_BURST),
        }
    }
}

impl Host {
    pub(crate) fn remote(&self) -> &Remote {
        &self.screen.remote
    }

    // The panel's Control, on a friend's share this host watches.
    pub(crate) fn ask_control(&mut self, share: u32, now: Instant, socket: &Socket) -> bool {
        if self.closed {
            return false;
        }
        let own = *self.identity.public();
        let Some(live) = self.live.as_ref().filter(|live| live.number == share) else {
            return false;
        };
        if live.sharer == own || self.screen.watched != Some(share) {
            return false;
        }
        let sharer = live.sharer;
        if let Some(grant) = &live.control {
            if grant.controller == own {
                return false;
            }
            let (key, first) = (grant.controller, grant.name.clone());
            self.system_line(key, first.clone(), remote::busy_line(&first));
            return true;
        }
        let sharer_name = self.name_of(&sharer);
        let ask = self.screen.remote.ask(share, sharer, sharer_name);
        let number = self.grant(own, self.name.clone(), ask);
        log!(
            self.log,
            "control {number}: this host asks to control share {share} of {}",
            keys::fingerprint(&sharer)
        );
        let name = self.name.clone();
        if let Some(peer) = self.peers.iter_mut().find(|peer| peer.key == sharer) {
            peer.link.queue(&Message::ControlAsked {
                share,
                control: number,
                slot: HOST_SLOT,
                name,
            });
            peer.flush(socket, now);
        }
        true
    }

    // The panel's Allow and Don't allow, on this host's own share, for the
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
        let share = here.share;
        let Some(grant) = self.grant_numbered(share, number) else {
            return false;
        };
        let (controller, name, ask) = (grant.controller, grant.name.clone(), grant.ask);
        let can = self.screen.remote.gate.can_inject();
        if allow && !can {
            self.problem_line(String::from(NO_INJECTOR_LINE));
        }
        if !(allow && can) {
            self.screen.remote.end_here(ControlEnd::Stopped, now);
            if let Some(live) = self.live.as_mut() {
                live.control = None;
            }
            log!(self.log, "control {number}: this host did not allow it");
            self.tell_controller(
                controller,
                share,
                ask,
                ControlAnswer::DontAllow,
                now,
                socket,
            );
            return true;
        }
        let area = self.screen.sharing.area();
        self.screen.remote.allow(area);
        if let Some(grant) = self.grant_numbered_mut(share, number) {
            grant.allowed = true;
        }
        log!(
            self.log,
            "control {number}: this host allowed {} to control its share",
            keys::fingerprint(&controller)
        );
        let own_name = self.name.clone();
        self.system_line(
            controller,
            name.clone(),
            remote::started_line(Seat::Controlled, &name, &own_name),
        );
        self.tell_controller(controller, share, ask, ControlAnswer::Allow, now, socket);
        self.roster_changed(now, socket);
        true
    }

    // The panel's Stop control and the panic key: whichever side of a
    // control this host is on. A request on show is declined.
    pub(crate) fn stop_control(&mut self, panic: bool, now: Instant, socket: &Socket) -> bool {
        if let Some(here) = self.screen.remote.here() {
            if !here.allowed {
                let number = here.number;
                return self.answer_control(number, false, now, socket);
            }
            let why = if panic {
                ControlEnd::Panic
            } else {
                ControlEnd::Stopped
            };
            self.end_grant(why, now, socket);
            return true;
        }
        if self.screen.remote.there().is_some() {
            self.end_grant(ControlEnd::Released, now, socket);
            return true;
        }
        false
    }

    // The row menu's End control. When this host is one of the two, it is
    // that side's own stop.
    pub(crate) fn end_control(&mut self, now: Instant, socket: &Socket) -> bool {
        let own = *self.identity.public();
        let Some(live) = self.live.as_ref() else {
            return false;
        };
        let Some(grant) = &live.control else {
            return false;
        };
        let why = if grant.controller == own {
            ControlEnd::Released
        } else if live.sharer == own {
            ControlEnd::Stopped
        } else {
            ControlEnd::EndedByHost
        };
        self.end_grant(why, now, socket);
        true
    }

    // Input from friend `i`, `plain` whole with its channel byte: taken
    // only from the controller the sharer allowed, held to the friend's
    // limit, read, then injected here or sealed once for the sharer. It
    // never changes the view.
    pub(super) fn on_input(
        &mut self,
        i: usize,
        plain: &mut [u8],
        now: Instant,
        socket: &Socket,
    ) -> bool {
        let taken = self.take_input(i, plain, now, socket);
        plain.zeroize();
        remote::wipe(&mut self.input_events);
        match taken {
            Err(why) => self.refused_input(i, why, now),
            Ok(()) => false,
        }
    }

    fn take_input(
        &mut self,
        i: usize,
        plain: &mut [u8],
        now: Instant,
        socket: &Socket,
    ) -> Result<(), String> {
        let key = self.peers[i].key;
        let own = *self.identity.public();
        let allowed = self.live.as_ref().and_then(|live| {
            live.control
                .as_ref()
                .filter(|grant| grant.allowed && grant.controller == key)
                .map(|_| (live.number, live.sharer))
        });
        let Some((share, sharer)) = allowed else {
            if self
                .control_ended
                .is_some_and(|(ended, until)| ended == key && now < until)
            {
                return Ok(());
            }
            return Err(String::from("they control nothing"));
        };
        let peer = &mut self.peers[i];
        if !peer.control.input.take(now, INPUT_PER_SECOND, INPUT_BURST) {
            self.screen.remote.over_rate();
            return Ok(());
        }
        let Some(payload) = plain.get_mut(1..) else {
            return Err(String::from("it is empty"));
        };
        let head = wire::read_input(payload, SENT, &mut self.input_events)
            .map_err(|why| why.to_string())?;
        let offset = peer.link.offset.best().map(|sample| sample.offset_us);
        let (captured, about) = talk::our_time(head.captured.unwrap_or(0), offset, peer.jittery);
        let slot = peer.slot;
        if sharer == own {
            let arrival = Arrival {
                share,
                slot,
                captured: captured.map(|at| (at, about)),
                area: self.screen.sharing.area(),
                now,
            };
            // Late is counted in the remote's numbers, and the sharer here
            // allowed this controller, so there is nothing else it can be.
            let _ = self
                .screen
                .remote
                .input(arrival, &head, &mut self.input_events);
            return Ok(());
        }
        wire::relay_in_place(payload, slot, captured, about);
        if let Some(other) = self.peers.iter_mut().find(|peer| peer.key == sharer)
            && let Some(session) = other.sessions.current.as_mut()
            && session.encrypt(plain, &mut self.input_sealed).is_ok()
        {
            other
                .traffic
                .send(socket, &self.input_sealed, other.media.to());
        }
        Ok(())
    }

    // Input from someone nobody allowed, or that does not parse, goes
    // nowhere and counts as bad.
    fn refused_input(&mut self, i: usize, why: String, now: Instant) -> bool {
        let peer = &self.peers[i];
        let (key, ip) = (peer.key, peer.addr.ip());
        if self.log.is_on() && self.booth_lines.allow(ip, now, &self.log) {
            self.log.line(format!(
                "input from {}, dropped: {why}",
                keys::fingerprint(&key)
            ));
        }
        self.drops.bad(now)
    }

    // The control messages from friend `i`. True when the view changed.
    pub(super) fn on_control_message(
        &mut self,
        i: usize,
        message: Message,
        now: Instant,
        socket: &Socket,
    ) -> bool {
        match message {
            Message::ControlAsk { share, ask } => self.took_ask(i, share, ask, now, socket),
            Message::ControlAnswer {
                share,
                number,
                answer,
            } => self.took_answer(i, share, number, answer, now, socket),
            Message::ControlEnd { share, number, why } => {
                self.took_end(i, share, number, why, now, socket)
            }
            Message::ControlPaused {
                share,
                number,
                paused,
            } => self.took_paused(i, share, number, paused, now, socket),
            // Only a host asks a sharer.
            _ => {
                self.drops.bad(now);
                false
            }
        }
    }

    fn took_ask(&mut self, i: usize, share: u32, ask: u32, now: Instant, socket: &Socket) -> bool {
        let (key, slot, name) = {
            let peer = &self.peers[i];
            (peer.key, peer.slot, peer.name.clone())
        };
        if !self.peers[i]
            .control
            .asks
            .take(now, ASKS_PER_SECOND, ASK_BURST)
        {
            // Counted with the rest of a friend's sharing past its limits.
            self.screen.dropped += 1;
            self.tell_controller(key, share, ask, ControlAnswer::TooSoon, now, socket);
            return false;
        }
        let own = *self.identity.public();
        let Some(live) = self.live.as_ref().filter(|live| live.number == share) else {
            // A share that ended while the ask was on its way.
            return false;
        };
        let sharer = live.sharer;
        if sharer == key || self.peers[i].share.watching != Some(share) {
            self.drops.bad(now);
            return false;
        }
        if let Some(grant) = &live.control {
            if grant.controller != key {
                let (holder, holder_name) = (grant.controller, grant.name.clone());
                log!(
                    self.log,
                    "{}: asked to control share {share} while {} has it, refused",
                    who(&self.peers[i]),
                    keys::fingerprint(&holder)
                );
                let busy = ControlAnswer::Busy {
                    key: holder,
                    name: holder_name,
                };
                self.tell_controller(key, share, ask, busy, now, socket);
                return false;
            }
            // Asked again: that friend has let go of the one before.
            self.end_grant(ControlEnd::Released, now, socket);
        }
        if sharer == own && !self.screen.remote.gate.can_inject() {
            log!(
                self.log,
                "{}: asked to control this host's share, which cannot be controlled here",
                who(&self.peers[i])
            );
            self.tell_controller(key, share, ask, ControlAnswer::DontAllow, now, socket);
            return false;
        }
        let number = self.grant(key, name.clone(), ask);
        log!(
            self.log,
            "control {number}: {} asks to control share {share}",
            who(&self.peers[i])
        );
        if sharer == own {
            self.screen
                .remote
                .asked(number, share, key, slot, name, now);
            return true;
        }
        if let Some(peer) = self.peers.iter_mut().find(|peer| peer.key == sharer) {
            peer.link.queue(&Message::ControlAsked {
                share,
                control: number,
                slot,
                name,
            });
            peer.flush(socket, now);
        }
        false
    }

    fn took_answer(
        &mut self,
        i: usize,
        share: u32,
        number: u32,
        answer: ControlAnswer,
        now: Instant,
        socket: &Socket,
    ) -> bool {
        let key = self.peers[i].key;
        let allow = match answer {
            ControlAnswer::Allow => true,
            ControlAnswer::DontAllow => false,
            // Only a host says these.
            ControlAnswer::Busy { .. } | ControlAnswer::TooSoon => {
                self.drops.bad(now);
                return false;
            }
        };
        // A share that ended while the answer was on its way.
        let Some(live) = self.live.as_ref().filter(|live| live.number == share) else {
            return false;
        };
        if live.sharer != key {
            self.drops.bad(now);
            return false;
        }
        // An answer to an ask that ended on the way is let go.
        let Some(grant) = self
            .grant_numbered(share, number)
            .filter(|grant| !grant.allowed)
        else {
            return false;
        };
        let (controller, name, ask) = (grant.controller, grant.name.clone(), grant.ask);
        let own = *self.identity.public();
        if !allow {
            if let Some(live) = self.live.as_mut() {
                live.control = None;
            }
            log!(
                self.log,
                "control {number}: {} did not allow it",
                who(&self.peers[i])
            );
            if controller == own {
                self.screen.remote.end_there();
                let sharer = self.peers[i].name.clone();
                self.system_line(key, sharer.clone(), remote::not_allowed_line(&sharer));
                return true;
            }
            self.tell_controller(
                controller,
                share,
                ask,
                ControlAnswer::DontAllow,
                now,
                socket,
            );
            return false;
        }
        if let Some(grant) = self.grant_numbered_mut(share, number) {
            grant.allowed = true;
        }
        log!(
            self.log,
            "control {number}: {} allowed {}",
            who(&self.peers[i]),
            keys::fingerprint(&controller)
        );
        let sharer = self.peers[i].name.clone();
        if controller == own {
            self.screen.remote.allowed(share, ask);
            self.system_line(
                key,
                sharer.clone(),
                remote::started_line(Seat::Controller, &name, &sharer),
            );
        } else {
            self.system_line(
                controller,
                name.clone(),
                remote::started_line(Seat::Host, &name, &sharer),
            );
            self.tell_controller(controller, share, ask, ControlAnswer::Allow, now, socket);
        }
        self.roster_changed(now, socket);
        true
    }

    fn took_end(
        &mut self,
        i: usize,
        share: u32,
        number: u32,
        why: ControlEnd,
        now: Instant,
        socket: &Socket,
    ) -> bool {
        let key = self.peers[i].key;
        let Some(live) = self.live.as_ref().filter(|live| live.number == share) else {
            return false;
        };
        let as_controller = why == ControlEnd::Released;
        let as_sharer = matches!(why, ControlEnd::Stopped | ControlEnd::Panic);
        if !as_controller && !as_sharer {
            self.drops.bad(now);
            return false;
        }
        let Some(grant) = &live.control else {
            return false;
        };
        let matches = if as_controller {
            grant.controller == key && grant.ask == number
        } else {
            live.sharer == key && grant.number == number
        };
        if !matches {
            return false;
        }
        log!(
            self.log,
            "control {}: {} ended it, {why:?}",
            grant.number,
            who(&self.peers[i])
        );
        self.end_grant(why, now, socket);
        true
    }

    fn took_paused(
        &mut self,
        i: usize,
        share: u32,
        number: u32,
        paused: bool,
        now: Instant,
        socket: &Socket,
    ) -> bool {
        let key = self.peers[i].key;
        // A share that ended while the word was on its way.
        let Some(live) = self.live.as_ref().filter(|live| live.number == share) else {
            return false;
        };
        if live.sharer != key {
            self.drops.bad(now);
            return false;
        }
        let Some(grant) = live
            .control
            .as_ref()
            .filter(|grant| grant.allowed && grant.number == number)
        else {
            return false;
        };
        let (controller, ask) = (grant.controller, grant.ask);
        log!(
            self.log,
            "control {number}: an administrator window in front on the sharer's pc: {}",
            crate::log::yes_no(paused)
        );
        self.tell_paused(controller, share, ask, paused, now, socket)
    }

    // What this host's own injector found: an administrator window came to
    // the front or went, and the controller hears of it.
    pub(crate) fn control_work(&mut self, now: Instant, socket: &Socket) -> bool {
        let Some((share, number, paused)) = self.screen.remote.admin_news() else {
            return false;
        };
        let Some(grant) = self.grant_numbered(share, number) else {
            return true;
        };
        let (controller, ask) = (grant.controller, grant.ask);
        log!(
            self.log,
            "control {number}: an administrator window in front on this host: {}",
            crate::log::yes_no(paused)
        );
        self.tell_paused(controller, share, ask, paused, now, socket);
        true
    }

    fn tell_paused(
        &mut self,
        controller: [u8; 32],
        share: u32,
        ask: u32,
        paused: bool,
        now: Instant,
        socket: &Socket,
    ) -> bool {
        if controller == *self.identity.public() {
            return self.screen.remote.set_paused(share, ask, paused);
        }
        if let Some(peer) = self.peers.iter_mut().find(|peer| peer.key == controller) {
            peer.link.queue(&Message::ControlPaused {
                share,
                number: ask,
                paused,
            });
            peer.flush(socket, now);
        }
        false
    }

    // Every timer pass: CUTOFF without input lets go of what this host's
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

    pub(super) fn control_deadline(&self) -> Option<Instant> {
        self.screen.remote.deadline()
    }

    // Someone left the room or started their link over: their control, or
    // the one they asked for, ends. A sharer's share ends by itself.
    pub(super) fn left_control(&mut self, key: &[u8; 32], now: Instant, socket: &Socket) {
        let controls = self
            .live
            .as_ref()
            .and_then(|live| live.control.as_ref())
            .is_some_and(|grant| grant.controller == *key);
        if controls {
            self.end_grant(ControlEnd::SessionLost, now, socket);
        }
    }

    // A controller who stopped watching has let go.
    pub(super) fn stopped_watching(&mut self, key: &[u8; 32], now: Instant, socket: &Socket) {
        let controls = self
            .live
            .as_ref()
            .and_then(|live| live.control.as_ref())
            .is_some_and(|grant| grant.controller == *key);
        if controls {
            self.end_grant(ControlEnd::Released, now, socket);
        }
    }

    // The share's control is over: both sides hear why, but not the side
    // whose message ended it, which knows. The injector hears it through
    // the gate, after the lock.
    pub(super) fn end_grant(&mut self, why: ControlEnd, now: Instant, socket: &Socket) {
        let Some(live) = self.live.as_mut() else {
            return;
        };
        let Some(grant) = live.control.take() else {
            return;
        };
        let (share, sharer) = (live.number, live.sharer);
        let own = *self.identity.public();
        let sharer_name = self.name_of(&sharer);
        let controller = grant.name.as_str();
        if grant.controller == own {
            if self
                .screen
                .remote
                .end_there()
                .is_some_and(|there| there.allowed)
            {
                let line = remote::ended_line(why, Seat::Controller, controller, &sharer_name);
                self.system_line(sharer, sharer_name.clone(), line);
            }
        } else if why != ControlEnd::Released
            && let Some(peer) = self.peers.iter_mut().find(|p| p.key == grant.controller)
        {
            peer.link.queue(&Message::ControlEnd {
                share,
                number: grant.ask,
                why,
            });
            peer.flush(socket, now);
        }
        if sharer == own {
            if self
                .screen
                .remote
                .end_here(why, now)
                .is_some_and(|here| here.allowed)
            {
                let line = remote::ended_line(why, Seat::Controlled, controller, &sharer_name);
                self.system_line(grant.controller, grant.name.clone(), line);
            }
        } else if !matches!(why, ControlEnd::Stopped | ControlEnd::Panic)
            && let Some(peer) = self.peers.iter_mut().find(|p| p.key == sharer)
        {
            peer.link.queue(&Message::ControlEnd {
                share,
                number: grant.number,
                why,
            });
            peer.flush(socket, now);
        }
        if grant.allowed && grant.controller != own && sharer != own {
            let line = remote::ended_line(why, Seat::Host, controller, &sharer_name);
            self.system_line(sharer, sharer_name, line);
        }
        log!(self.log, "control {}: over, {why:?}", grant.number);
        self.control_ended = Some((grant.controller, now + ENDED_GRACE));
        if grant.allowed {
            self.roster_changed(now, socket);
        }
    }

    // The room is over: nobody is told, since everyone is let go, but this
    // PC's own injector lets go of what it holds.
    pub(super) fn close_control(&mut self) {
        self.screen
            .remote
            .end_here(ControlEnd::Closed, Instant::now());
        self.screen.remote.end_there();
    }

    // Where this host's own controller packets go: the sharer's link, while
    // it controls a friend's share.
    pub(crate) fn publish_control(&mut self) {
        let own = *self.identity.public();
        let peers = &self.peers;
        let target = self
            .live
            .as_ref()
            .filter(|live| {
                !self.closed
                    && live
                        .control
                        .as_ref()
                        .is_some_and(|grant| grant.allowed && grant.controller == own)
            })
            .and_then(|live| peers.iter().find(|peer| peer.key == live.sharer));
        let link = target.and_then(|peer| {
            let session = peer.sessions.current.as_ref()?;
            session
                .is_confirmed()
                .then(|| (session.remote_index(), peer.media.to(), HOST_SLOT))
        });
        self.screen.remote.publish(link, || {
            let peer = target?;
            Some(ControlLink {
                sealer: peer.sessions.current.as_ref()?.sealer()?,
                to: peer.media.to(),
                kind: RELAYED,
                slot: HOST_SLOT,
            })
        });
    }

    pub(super) fn control_view(&self) -> ControlView {
        let controller = self
            .live
            .as_ref()
            .and_then(|live| live.control.as_ref())
            .filter(|grant| grant.allowed)
            .map(|grant| grant.controller);
        self.screen.remote.view(controller)
    }

    pub(super) fn controls(&self, key: &[u8; 32]) -> bool {
        self.live
            .as_ref()
            .and_then(|live| live.control.as_ref())
            .is_some_and(|grant| grant.allowed && grant.controller == *key)
    }

    fn grant(&mut self, controller: [u8; 32], name: String, ask: u32) -> u32 {
        let number = self.next_control;
        self.next_control = self.next_control.wrapping_add(1).max(1);
        if let Some(live) = self.live.as_mut() {
            live.control = Some(Grant {
                number,
                controller,
                name,
                ask,
                allowed: false,
            });
        }
        number
    }

    fn grant_numbered(&self, share: u32, number: u32) -> Option<&Grant> {
        self.live
            .as_ref()
            .filter(|live| live.number == share)?
            .control
            .as_ref()
            .filter(|grant| grant.number == number)
    }

    fn grant_numbered_mut(&mut self, share: u32, number: u32) -> Option<&mut Grant> {
        self.live
            .as_mut()
            .filter(|live| live.number == share)?
            .control
            .as_mut()
            .filter(|grant| grant.number == number)
    }

    // The answer to a friend's ask, or this host's own, which takes it here.
    fn tell_controller(
        &mut self,
        controller: [u8; 32],
        share: u32,
        ask: u32,
        answer: ControlAnswer,
        now: Instant,
        socket: &Socket,
    ) {
        if controller == *self.identity.public() {
            return;
        }
        if let Some(peer) = self.peers.iter_mut().find(|peer| peer.key == controller) {
            peer.link.queue(&Message::ControlAnswer {
                share,
                number: ask,
                answer,
            });
            peer.flush(socket, now);
        }
    }
}
