// A client's part in sharing: it asks the host to share and waits for the
// answer, says what it watches, takes the video the host passes on to it,
// and sends back what the viewer needs the sharer to hear. The host is only
// a friend's PC too, so what it sends is checked as closely as the host
// checks a friend's.

use std::sync::Arc;
use std::time::Instant;

use super::Client;
use crate::control::{Entry, EntryShare, Message, Roster, ShareAnswer};
use crate::log::log;
use crate::remote::ControlEnd;
use crate::screen::wire::{self, PREFIX, RELAYED};
use crate::screen::{
    self, Answer, Back, OwnShare, Refusal, ShareNews, WatchEvent, WatchNews, percent, permille,
    recover_span,
};
use crate::socket::Socket;
use crate::talk::Outlet;
use crate::view::{ChatLine, CurrentShare, LineKind, LinkState, ShareView};

// Who shares in a roster, and their share.
fn sharer_of(roster: &Roster) -> Option<(&Entry, EntryShare)> {
    roster
        .entries
        .iter()
        .find_map(|entry| Some((entry, entry.share?)))
}

impl Client {
    // The panel's Share: an ask to the host, and nothing captured before the
    // answer, or refused here while the roster shows someone else sharing.
    // While this PC shares, the frame rate changes.
    pub(crate) fn share(&mut self, fps: u8, now: Instant, socket: &Socket) -> bool {
        if self.state == LinkState::Closed {
            return false;
        }
        self.screen.problem = None;
        if self.state != LinkState::Live || self.sessions.current.is_none() {
            self.share_refused(Refusal::NotLive);
            return true;
        }
        let state = self.screen.sharing.state();
        let asks = match state {
            // The host hears the new rate from the share's thread, as the
            // rate it runs at (ShareNews::Fps), which stepped down is not
            // the one asked.
            OwnShare::Sharing { number, fps: was } => {
                if was == fps {
                    return false;
                }
                self.screen
                    .sharing
                    .set_state(OwnShare::Sharing { number, fps });
                false
            }
            OwnShare::Asking { .. } => false,
            OwnShare::Off | OwnShare::Refused(_) => {
                // The host would refuse it with the same line, a round trip
                // later. A share that started since the last roster is the
                // host's to refuse.
                let own = *self.identity.public();
                if let Some((entry, _)) =
                    sharer_of(&self.roster).filter(|(entry, _)| entry.key != own)
                {
                    let (key, name) = (entry.key, entry.name.clone());
                    log!(
                        self.log,
                        "share: not asked, the roster says {} shares already",
                        keys::fingerprint(&key)
                    );
                    self.refused_busy(key, name);
                    return true;
                }
                self.screen.sharing.set_state(OwnShare::Asking { fps });
                true
            }
        };
        if asks {
            self.link.queue(&Message::ShareStart { fps });
            self.flush(now, socket);
        }
        true
    }

    pub(crate) fn stop_sharing(&mut self, now: Instant, socket: &Socket) -> bool {
        let state = self.screen.sharing.state();
        if matches!(state, OwnShare::Sharing { .. } | OwnShare::Asking { .. })
            && self.sessions.current.is_some()
        {
            self.link.queue(&Message::ShareStop);
            self.flush(now, socket);
        }
        self.control_share_ended(now);
        self.screen.sharing.set_state(OwnShare::Off);
        true
    }

    // The panel's Watch and Stop watching, for the share it showed.
    pub(crate) fn watch(&mut self, share: u32, on: bool, now: Instant, socket: &Socket) -> bool {
        if self.state == LinkState::Closed {
            return false;
        }
        if !on {
            if self.screen.watched != Some(share) {
                return false;
            }
            self.control_stopped_watching(now, socket);
            self.link.queue(&Message::Watch {
                share,
                on: false,
                hevc: false,
            });
            self.flush(now, socket);
            self.stop_watching();
            return true;
        }
        let own = *self.identity.public();
        let Some((entry, current)) = sharer_of(&self.roster) else {
            return false;
        };
        if current.number != share || entry.key == own || self.screen.watched == Some(share) {
            return false;
        }
        let name = entry.name.clone();
        self.screen.watched = Some(share);
        self.screen.problem = None;
        self.screen.watching.event(WatchEvent::Started {
            share,
            fps: current.fps,
            name,
        });
        self.link.queue(&Message::Watch {
            share,
            on: true,
            hevc: self.screen.watching.takes_hevc(),
        });
        self.flush(now, socket);
        self.share_step();
        true
    }

    // What the two hooks left for the timer thread: a shape from this PC's
    // share, what its viewer hands back, and what the share's and the
    // viewer's threads have to say. True when the view changed.
    pub(crate) fn screen_work(&mut self, now: Instant, socket: &Socket) -> bool {
        if self.state == LinkState::Closed {
            return false;
        }
        let mut changed = self.screen.sharing.take_fresh() | self.screen.watching.take_fresh();
        for news in self.screen.sharing.take_news() {
            changed |= self.share_news(news, now, socket);
        }
        for news in self.screen.watching.take_news() {
            changed |= self.watch_news(news, now, socket);
        }
        // The viewer found out what it decodes after the Watch went: the
        // host hears it again, as it may change the share's codec.
        if self.screen.watching.take_hevc_changed()
            && let Some(share) = self.screen.watched
        {
            let hevc = self.screen.watching.takes_hevc();
            log!(
                self.log,
                "watch {share}: decodes hevc: {}",
                crate::log::yes_no(hevc)
            );
            self.link.queue(&Message::Watch {
                share,
                on: true,
                hevc,
            });
        }
        self.own_shape(now);
        for back in self.screen.watching.take_backs() {
            let Some(number) = self.screen.watched else {
                continue;
            };
            match back {
                Back::Recover { share, first, last } if share == number => {
                    let (first, last) = recover_span(first, last);
                    self.link
                        .queue_feedback(&Message::Recover { share, first, last });
                }
                Back::Idr { share, seen } if share == number => {
                    self.link.queue_feedback(&Message::Idr {
                        share,
                        seen: Some(seen),
                    });
                }
                // The viewer already paces these, once a second and its
                // first nonzero loss at once, and the host holds each to its
                // own rule, so each goes as it comes.
                Back::Loss { share, loss } if share == number => {
                    self.link.queue_feedback(&Message::VideoLoss {
                        share,
                        loss: permille(loss),
                    });
                }
                _ => {}
            }
        }
        self.flush(now, socket);
        changed
    }

    // From this PC's own share's thread. True when the view changed.
    fn share_news(&mut self, news: ShareNews, now: Instant, socket: &Socket) -> bool {
        let ours = |share: u32| self.screen.sharing.number() == Some(share);
        match news {
            ShareNews::Failed { share, ran, why } => {
                if !ours(share) {
                    return false;
                }
                self.problem_line(screen::share_failed(&why, ran));
                self.stop_sharing(now, socket);
            }
            // The host keeps the roster's rate; a share already granted
            // that asks again with another rate changes it there.
            ShareNews::Fps { share, fps } => {
                if !ours(share) || self.told_fps == Some((share, fps)) {
                    return false;
                }
                log!(self.log, "share {share}: runs at {fps} fps");
                self.told_fps = Some((share, fps));
                self.link.queue(&Message::ShareStart { fps });
                self.flush(now, socket);
                return false;
            }
            ShareNews::Software { share, sentence } => {
                if !ours(share) {
                    return false;
                }
                self.problem_line(String::from(sentence));
            }
            ShareNews::Paused { share, paused } => {
                if !ours(share) {
                    return false;
                }
                self.problem_line(String::from(screen::paused_sentence(paused)));
            }
        }
        true
    }

    // From this PC's viewer's thread.
    fn watch_news(&mut self, news: WatchNews, now: Instant, socket: &Socket) -> bool {
        let share = match news {
            WatchNews::Failed { share, name, why } => {
                self.problem_line(screen::watch_failed(&name, &why));
                share
            }
            WatchNews::Closed { share } => share,
        };
        self.watch(share, false, now, socket);
        true
    }

    // This PC's newest shape, to the host, once its budget and the control
    // channel have room for it; the caller flushes. Tried when a shape is
    // set and on every timer pass, and shape_due brings a pass while one
    // waits.
    pub(super) fn own_shape(&mut self, now: Instant) {
        let Some(number) = self
            .screen
            .sharing
            .number()
            .filter(|_| self.sessions.current.is_some())
        else {
            return;
        };
        let queued = self.link.waiting();
        if let Some((id, shape)) = self.screen.shape_out(now, queued) {
            for chunk in wire::chunks(number, id, &shape) {
                self.link.queue(&Message::Shape(chunk));
            }
        }
    }

    pub(super) fn share_deadline(&self) -> Option<Instant> {
        self.screen.shape_due()
    }

    // The link this PC's share goes out on: the host's current session, at
    // the address media goes to, while the share lasts.
    pub(crate) fn publish_share(&mut self) {
        let live = self.state != LinkState::Closed && self.screen.sharing.number().is_some();
        let session = self
            .sessions
            .current
            .as_ref()
            .filter(|session| live && session.is_confirmed());
        let to = self.media.map(|media| media.to());
        let links = session
            .zip(to)
            .map(|(session, to)| (session.remote_index(), to))
            .into_iter();
        let sent = &self.video_out;
        self.screen.publish(links, || {
            session
                .zip(to)
                .and_then(|(session, to)| {
                    Some(Outlet {
                        sealer: session.sealer()?,
                        to,
                        sent: Arc::clone(sent),
                    })
                })
                .into_iter()
                .collect()
        });
    }

    // Video the host passed on: only from the share this PC watches, under
    // the slot the roster gives its sharer.
    pub(super) fn on_video(&mut self, payload: &[u8], now: Instant) -> bool {
        let packet = match wire::read_video(payload, RELAYED) {
            Ok((slot, packet)) => (slot, packet),
            Err(why) => return self.refused_media("video", &why.to_string(), now),
        };
        let (slot, packet) = packet;
        let Some((key, number, far_end)) = self.watched_from(slot) else {
            return false;
        };
        self.link.media_passed(now, &self.timers);
        let arrived_us = self.clock.micros(now);
        self.screen
            .video_in(key, number, &packet, far_end, arrived_us, now);
        self.screen
            .watching
            .video(number, payload.get(PREFIX..).unwrap_or_default());
        false
    }

    pub(super) fn on_cursor(&mut self, payload: &[u8], now: Instant) -> bool {
        let (slot, pointer) = match wire::read_pointer(payload, RELAYED) {
            Ok(read) => read,
            Err(why) => return self.refused_media("a pointer update", &why.to_string(), now),
        };
        if let Some((_, number, _)) = self.watched_from(slot) {
            self.screen.watching.pointer(number, pointer);
        }
        false
    }

    // The sharer's key, the share, and whether the host is the sharer, when
    // `slot` is the sharer of the share this PC watches.
    fn watched_from(&self, slot: u8) -> Option<([u8; 32], u32, bool)> {
        let number = self.screen.watched?;
        let (entry, share) = sharer_of(&self.roster)?;
        let own = self.identity.public();
        (share.number == number && entry.slot == slot && entry.key != *own).then_some((
            entry.key,
            number,
            entry.is_host,
        ))
    }

    fn refused_media(&mut self, what: &str, why: &str, now: Instant) -> bool {
        if self.log.is_on()
            && let Some(from) = self.host_addr
            && self.strays.allow(from.ip(), now, &self.log)
        {
            self.log
                .line(format!("{what} from the host, dropped: {why}"));
        }
        self.drops.bad(now);
        false
    }

    // The control messages sharing adds, from the host.
    pub(super) fn on_share_message(&mut self, message: Message, now: Instant) {
        let own_share = self.screen.sharing.number();
        match message {
            Message::ShareAnswer(answer) => self.answered(answer),
            Message::Recover { share, first, last } if own_share == Some(share) => {
                self.screen.sharing.answer(Answer::Recover { first, last });
            }
            Message::Idr { share, seen } if own_share == Some(share) => {
                self.screen.sharing.answer(Answer::Idr { seen });
            }
            Message::VideoLoss { share, loss } if own_share == Some(share) => {
                self.screen.sharing.answer(Answer::Loss(loss.map(percent)));
            }
            Message::ShareFacts(facts) if own_share == Some(facts.share) => {
                self.screen.sharing.set_facts(&facts);
            }
            Message::SharerClock {
                share,
                offset_us,
                about,
            } => {
                if self.screen.watched == Some(share) {
                    self.sharer_clock = Some((share, offset_us, about));
                    self.share_step();
                }
            }
            Message::Shape(chunk) => {
                if self.screen.watched == Some(chunk.share)
                    && let Some((share, id, shape)) = self.screen.shape_in(chunk)
                {
                    self.screen.watching.shape(share, id, shape);
                }
            }
            // Answers about a share that is over, which were on their way.
            Message::Recover { .. }
            | Message::Idr { .. }
            | Message::VideoLoss { .. }
            | Message::ShareFacts(_) => {}
            // Only a client sends these.
            _ => {
                self.drops.bad(now);
            }
        }
    }

    fn answered(&mut self, answer: ShareAnswer) {
        let (asked, asking) = match self.screen.sharing.state() {
            OwnShare::Asking { fps } => (fps, true),
            OwnShare::Sharing { fps, .. } => (fps, false),
            // Stopped before the answer came; the host has the stop too.
            _ => return,
        };
        match answer {
            ShareAnswer::Granted { share } => {
                log!(self.log, "share {share}: the host lets this pc share");
                // The ask carried this rate, and the roster has it now. A
                // grant while sharing answers a rate the share's thread
                // told, which told_fps has already.
                if asking {
                    self.told_fps = Some((share, asked));
                }
                self.screen.sharing.set_state(OwnShare::Sharing {
                    number: share,
                    fps: asked,
                });
            }
            ShareAnswer::Busy { key, name } => {
                log!(
                    self.log,
                    "share: refused, {} shares already",
                    keys::fingerprint(&key)
                );
                self.refused_busy(key, name);
            }
            ShareAnswer::TooSoon => {
                log!(self.log, "share: refused, asked too often");
                self.share_refused(Refusal::TooSoon);
            }
        }
    }

    fn refused_busy(&mut self, key: [u8; 32], name: String) {
        let refusal = Refusal::Busy { name: name.clone() };
        self.system_line(key, name, refusal.sentence());
        self.screen.sharing.set_state(OwnShare::Refused(refusal));
    }

    fn share_refused(&mut self, refusal: Refusal) {
        let own = *self.identity.public();
        self.system_line(own, self.name.clone(), refusal.sentence());
        self.screen.sharing.set_state(OwnShare::Refused(refusal));
    }

    // A new roster: who started and stopped sharing, and whether this PC's
    // own share and the share it watches are still on.
    pub(super) fn roster_arrived(&mut self, old: &Roster) {
        let own = *self.identity.public();
        let was = sharer_of(old).map(|(entry, share)| (entry.key, entry.name.clone(), share));
        let now =
            sharer_of(&self.roster).map(|(entry, share)| (entry.key, entry.name.clone(), share));
        let number = |share: &Option<([u8; 32], String, EntryShare)>| {
            share.as_ref().map(|(_, _, share)| share.number)
        };
        // The first roster of the room says who shares already, not who
        // started.
        let first = old.entries.is_empty();
        if number(&was) != number(&now) && !first {
            if let Some((key, name, _)) = was.clone().filter(|(key, _, _)| *key != own) {
                self.system_line(key, name.clone(), format!("{name} stopped sharing"));
            }
            if let Some((key, name, _)) = now.clone().filter(|(key, _, _)| *key != own) {
                self.system_line(key, name.clone(), format!("{name} started sharing"));
            }
        }
        // The host ended this PC's share: it restarted the link, or let it go.
        if let OwnShare::Sharing { number: mine, .. } = self.screen.sharing.state() {
            let still = now
                .as_ref()
                .is_some_and(|(key, _, share)| *key == own && share.number == mine);
            if !still {
                log!(self.log, "share {mine}: the host's roster says it is over");
                self.control_share_ended(Instant::now());
                self.screen.sharing.set_state(OwnShare::Off);
            }
        }
        self.control_roster();
        if let Some(watched) = self.screen.watched {
            match now.as_ref().filter(|(_, _, share)| share.number == watched) {
                None => self.stop_watching(),
                Some((_, _, share)) => {
                    let before = was.as_ref().map(|(_, _, share)| share.fps);
                    if before != Some(share.fps) {
                        self.screen.watching.event(WatchEvent::Fps {
                            share: watched,
                            fps: share.fps,
                        });
                    }
                }
            }
        }
    }

    // Every timer pass: the offset that turns the sharer's capture times
    // into this PC's. When the host shares its own offset is the whole of
    // it; a friend's share adds the one the host said it has to that friend.
    pub(super) fn share_step(&mut self) {
        let Some(number) = self.screen.watched else {
            return;
        };
        let host_shares = sharer_of(&self.roster)
            .filter(|(_, share)| share.number == number)
            .map(|(entry, _)| entry.is_host);
        let to_host = self.link.offset.best().map(|sample| sample.offset_us);
        let offset = match (host_shares, to_host) {
            (Some(true), Some(to_host)) => Some((to_host, self.jittery)),
            (Some(false), Some(to_host)) => self
                .sharer_clock
                .filter(|(share, _, _)| *share == number)
                .map(|(_, sharer, about)| (sharer.saturating_add(to_host), about || self.jittery)),
            _ => None,
        };
        self.screen
            .watching
            .set_offset(offset.map(|(offset, about)| (number, offset, about)));
    }

    // Once a second, with the voice reports: for this PC's own share's
    // backoff, the round trip to the host, which every packet of it crosses.
    pub(super) fn share_round_trip(&mut self, now: Instant) {
        let sharing = self.screen.sharing.number().is_some();
        let round_trip = self.link.round_trip(now).filter(|_| sharing);
        let unanswered = self.link.unanswered_ms(now).filter(|_| sharing);
        self.screen.sharing.set_round_trip(round_trip, unanswered);
    }

    // A new session, not a rekey: the host started this PC's link over, and
    // with it ended its share and forgot what it watched. What it watched
    // it asks for again, right behind the Hello.
    pub(super) fn share_restarted(&mut self) {
        // A fresh handshake ends control, so an approval never outlives the
        // session it was given in; a rekey does not come through here.
        self.control_over(ControlEnd::SessionLost);
        self.end_own_share();
        if let Some(share) = self.screen.watched {
            self.link.queue(&Message::Watch {
                share,
                on: true,
                hevc: self.screen.watching.takes_hevc(),
            });
        }
    }

    // The host is lost or the room is over.
    pub(super) fn share_over(&mut self) {
        self.control_over(ControlEnd::SessionLost);
        self.end_own_share();
        self.stop_watching();
    }

    fn end_own_share(&mut self) {
        match self.screen.sharing.state() {
            OwnShare::Asking { .. } => self.share_refused(Refusal::NotLive),
            OwnShare::Sharing { .. } => self.screen.sharing.set_state(OwnShare::Off),
            OwnShare::Off | OwnShare::Refused(_) => {}
        }
    }

    fn stop_watching(&mut self) {
        self.screen.stop_watching();
        self.sharer_clock = None;
    }

    pub(super) fn share_view(&self) -> ShareView {
        let own = *self.identity.public();
        let current = sharer_of(&self.roster).map(|(entry, share)| {
            let yours = entry.key == own;
            CurrentShare {
                key: entry.key,
                name: entry.name.clone(),
                number: share.number,
                fps: share.fps,
                yours,
                watchers: self
                    .screen
                    .sharing
                    .facts()
                    .filter(|_| yours)
                    .map(|facts| facts.watchers),
            }
        });
        ShareView {
            watching: current
                .as_ref()
                .is_some_and(|share| self.screen.watched == Some(share.number)),
            current,
            own: self.screen.sharing.state(),
            running: self.screen.sharing.running(),
            viewer_open: self.screen.watching.showing(),
            problem: self.screen.problem.clone(),
            control: self.control_view(),
        }
    }

    pub(super) fn system_line(&mut self, author: [u8; 32], name: String, text: String) {
        self.history.push(ChatLine {
            author,
            name,
            text,
            at_unix_ms: crate::unix_now_ms(),
            mine: false,
            kind: LineKind::System,
        });
    }

    // A system line in warn about this PC's own share or viewer, which the
    // view also keeps as the latest problem.
    pub(super) fn problem_line(&mut self, text: String) {
        self.screen.problem = Some(text.clone());
        self.history.push(ChatLine {
            author: *self.identity.public(),
            name: self.name.clone(),
            text,
            at_unix_ms: crate::unix_now_ms(),
            mine: false,
            kind: LineKind::Problem,
        });
    }
}
