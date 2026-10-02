// The host's part in sharing: it grants one share at a time, keeps who
// watches, passes each video packet on as it arrives to the watchers only,
// and carries what goes back to the sharer. Everything here came from a
// friend's PC, which is hostile input: it is checked before it goes any
// further, and is held to the limits in screen.rs.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::remote::{Grant, PeerControl};
use super::{Host, Peer, who};
use crate::control::{Facts, Message, ShareAnswer};
use crate::limit::Bucket;
use crate::log::log;
use crate::remote::ControlEnd;
use crate::screen::wire::{self, RELAYED, SENT, ShapeChunk};
use crate::screen::{
    self, Answer, Back, FACTS_EVERY, IDR_ASK_EVERY, OwnShare, Refusal, ShareNews, WatchEvent,
    WatchNews, is_after, percent, permille, recover_span,
};
use crate::socket::Socket;
use crate::talk::{Outlet, Sent};
use crate::view::{ChatLine, CurrentShare, LineKind, ShareView};

// A watcher's loss report counts for this long, as a listener's voice
// report does.
const LOSS_KEPT: Duration = Duration::from_secs(2);
// Recover ranges already passed on to the sharer, so the same loss seen by
// several watchers, which is what a loss on the sharer's own link is, goes
// once. Frame numbers only move forward, so a range stays good; these only
// bound the memory.
const RECOVERED_KEPT: usize = 16;
const RECOVERED_FOR: Duration = Duration::from_secs(2);
// Video and pointer updates the sharer sent just before its Stop sharing
// reach the host after it, over a path of their own. For this long after a
// friend's share ends they are let go quietly, as packets for a session
// that just ended are, and not counted as bad.
const ENDED_GRACE: Duration = Duration::from_secs(1);
// A friend's limits, and when they last made the sharer produce an IDR, are
// kept this long after they leave: longer than the slowest bucket takes to
// fill from empty, so leaving and joining again in a loop gets nothing that
// waiting would not. Only a full handshake adds one; the cap is for a
// program that joins and leaves in a loop.
const KEPT_FOR: Duration = Duration::from_secs(10);
const MOST_KEPT: usize = 16;

// The share in the room now.
pub(super) struct Live {
    pub number: u32,
    pub sharer: [u8; 32],
    // Their name when the share began, for the line when it ends after
    // they left.
    name: String,
    pub fps: u8,
    recovered: VecDeque<(u32, u32, Instant)>,
    // The newest frame passed on from a friend's share: a recover request
    // can only be about a frame a watcher could have had.
    newest: Option<u32>,
    // What the sharer was last told of how it is watched, when, and when
    // facts that changed since may go (FACTS_EVERY).
    told: Option<Facts>,
    told_at: Option<Instant>,
    facts_due: Option<Instant>,
    shape: Option<ShapeUnderWay>,
    // Remote control of this share, asked for or allowed: it ends with it.
    pub control: Option<Grant>,
}

// The shape the sharer is sending, as its first chunk gave it.
struct ShapeUnderWay {
    id: u32,
    total: u32,
    next: u16,
    // It fit the limit and goes on.
    pass: bool,
}

// What the host keeps about one watcher: a friend, or the host itself.
#[derive(Default)]
pub(super) struct Watcher {
    // Its latest loss report, in tenths of a percent, and when it came.
    loss: Option<(Option<u16>, Instant)>,
    last_idr: Option<Instant>,
    // It started watching within IDR_ASK_EVERY of its last IDR ask, and its
    // IDR waits for the gap to end rather than being dropped: on a still
    // screen nothing else would bring its first picture, and without the
    // IDR ask the sharer sends no pointer shape either. Other asks are
    // dropped, since a viewer asks again while frames keep failing.
    join_idr_waits: bool,
    // The sharer's clock offset it was last told.
    told_clock: Option<(i64, bool)>,
    // Its viewer decodes HEVC, as its Watch said.
    hevc: bool,
}

// A friend's side of sharing, on the host.
pub(super) struct PeerShare {
    // The share this friend watches.
    pub watching: Option<u32>,
    watcher: Watcher,
    limits: Limits,
    // What the host's own share sent on this link, from the sharer's thread.
    pub video_out: Arc<Sent>,
}

// What a friend who left takes back if they join again soon.
pub(super) struct Kept {
    key: [u8; 32],
    limits: Limits,
    last_idr: Option<Instant>,
    // Asks for control put a request in front of the sharer, and input,
    // with its own limits.
    control: PeerControl,
    until: Instant,
}

struct Limits {
    video: Bucket,
    pointer: Bucket,
    shape_bytes: Bucket,
    asks: Bucket,
    watches: Bucket,
    recovers: Bucket,
    losses: Bucket,
}

impl PeerShare {
    pub(super) fn new(now: Instant) -> PeerShare {
        PeerShare {
            watching: None,
            watcher: Watcher::default(),
            limits: Limits {
                video: Bucket::full(now, screen::VIDEO_BURST),
                pointer: Bucket::full(now, screen::POINTER_BURST),
                shape_bytes: Bucket::full(now, screen::SHAPE_BYTES_BURST),
                asks: Bucket::full(now, screen::SHARE_ASK_BURST),
                watches: Bucket::full(now, screen::WATCH_BURST),
                recovers: Bucket::full(now, screen::RECOVER_BURST),
                losses: Bucket::full(now, screen::LOSS_BURST),
            },
            video_out: Arc::default(),
        }
    }

    // The link started over, or the share ended: whatever it watched it
    // asks for again.
    pub(super) fn restart(&mut self) {
        self.watching = None;
        self.watcher.stopped();
    }
}

impl Watcher {
    // It stopped watching. The time of its last IDR ask stays, so Watch and
    // Stop watching in a loop ask for no more IDRs than asking does.
    fn stopped(&mut self) {
        self.loss = None;
        self.told_clock = None;
        self.join_idr_waits = false;
    }

    // True when an IDR ask for this watcher may go now.
    fn idr_due(&mut self, now: Instant) -> bool {
        let due = self
            .last_idr
            .is_none_or(|at| now.saturating_duration_since(at) >= IDR_ASK_EVERY);
        if due {
            self.last_idr = Some(now);
        }
        due
    }

    // When a new watcher's IDR held back by the gap may go.
    fn join_idr_due(&self) -> Option<Instant> {
        self.last_idr
            .filter(|_| self.join_idr_waits)
            .map(|at| at + IDR_ASK_EVERY)
    }
}

// Where an answer for the sharer comes from: a friend, or this host.
#[derive(Clone, Copy, PartialEq, Eq)]
enum By {
    Peer(usize),
    Host,
}

impl Host {
    // The panel's Share. Refused, with the sentence, while someone else
    // shares; while this host shares, the frame rate changes.
    pub(crate) fn share(&mut self, fps: u8, now: Instant, socket: &Socket) -> bool {
        if self.closed {
            return false;
        }
        self.screen.problem = None;
        let own = *self.identity.public();
        match self.live.as_ref().map(|live| (live.sharer, live.number)) {
            // Compared with the rate asked, not the roster's: stepped down,
            // the share runs slower than asked, and the share's thread says
            // what it runs at (ShareNews::Fps), which the roster shows.
            Some((sharer, number)) if sharer == own => {
                let asked = match self.screen.sharing.state() {
                    OwnShare::Sharing {
                        number: own_share,
                        fps,
                    } if own_share == number => Some(fps),
                    _ => None,
                };
                if asked != Some(fps) {
                    self.screen
                        .sharing
                        .set_state(OwnShare::Sharing { number, fps });
                }
            }
            Some((key, _)) => {
                let name = self.name_of(&key);
                log!(
                    self.log,
                    "share: this host asked to share while {} shares, refused",
                    keys::fingerprint(&key)
                );
                let refusal = Refusal::Busy { name: name.clone() };
                self.system_line(key, name, refusal.sentence());
                self.screen.sharing.set_state(OwnShare::Refused(refusal));
            }
            None => {
                let number = self.start_share(own, fps, now, socket);
                self.screen
                    .sharing
                    .set_state(OwnShare::Sharing { number, fps });
                self.tell_facts(now, socket);
            }
        }
        true
    }

    pub(crate) fn stop_sharing(&mut self, now: Instant, socket: &Socket) -> bool {
        let own = *self.identity.public();
        if self.live.as_ref().is_some_and(|live| live.sharer == own) {
            self.end_share(now, socket);
        }
        self.screen.sharing.set_state(OwnShare::Off);
        true
    }

    // The panel's Watch and Stop watching, for the share it showed.
    pub(crate) fn watch(&mut self, share: u32, on: bool, now: Instant, socket: &Socket) -> bool {
        if self.closed {
            return false;
        }
        let own = *self.identity.public();
        if !on {
            if self.screen.watched == Some(share) {
                self.stopped_watching(&own, now, socket);
                self.screen.stop_watching();
                self.own_watcher.stopped();
                self.tell_facts(now, socket);
            }
            return true;
        }
        let Some(live) = self.live.as_ref().filter(|live| live.number == share) else {
            return false;
        };
        if live.sharer == own || self.screen.watched == Some(share) {
            return false;
        }
        let (fps, sharer) = (live.fps, live.sharer);
        self.screen.watched = Some(share);
        self.screen.problem = None;
        self.screen.watching.event(WatchEvent::Started {
            share,
            fps,
            name: self.name_of(&sharer),
        });
        self.own_watcher.hevc = self.screen.watching.takes_hevc();
        self.started_watching(By::Host, now, socket);
        true
    }

    // What the two hooks left for the timer thread: a shape from this host's
    // own share, what its viewer hands back, and what the share's and the
    // viewer's threads have to say. True when the view changed.
    pub(crate) fn screen_work(&mut self, now: Instant, socket: &Socket) -> bool {
        if self.closed {
            return false;
        }
        let mut changed = self.screen.sharing.take_fresh() | self.screen.watching.take_fresh();
        for news in self.screen.sharing.take_news() {
            changed |= self.share_news(news, now, socket);
        }
        for news in self.screen.watching.take_news() {
            changed |= self.watch_news(news, now, socket);
        }
        // This host's viewer found out what it decodes after it started
        // watching.
        if self.screen.watching.take_hevc_changed() && self.screen.watched.is_some() {
            self.own_watcher.hevc = self.screen.watching.takes_hevc();
            log!(
                self.log,
                "this host's viewer decodes hevc: {}",
                crate::log::yes_no(self.own_watcher.hevc)
            );
            self.tell_facts(now, socket);
        }
        self.own_shape(now, socket);
        for back in self.screen.watching.take_backs() {
            let Some(number) = self.screen.watched else {
                continue;
            };
            match back {
                Back::Recover { share, first, last } if share == number => {
                    let (first, last) = recover_span(first, last);
                    self.recover_from(first, last, now, socket);
                }
                Back::Idr { share, seen } if share == number => {
                    self.idr_for(By::Host, Some(seen), now, socket);
                }
                Back::Loss { share, loss } if share == number => {
                    self.loss_from(By::Host, permille(loss), now, socket);
                }
                _ => {}
            }
        }
        changed
    }

    // From this host's own share's thread. True when the view changed.
    fn share_news(&mut self, news: ShareNews, now: Instant, socket: &Socket) -> bool {
        let own = *self.identity.public();
        let ours = |live: &Live, share: u32| live.sharer == own && live.number == share;
        match news {
            ShareNews::Failed { share, ran, why } => {
                if !self.live.as_ref().is_some_and(|live| ours(live, share)) {
                    return false;
                }
                self.problem_line(screen::share_failed(&why, ran));
                self.end_share(now, socket);
            }
            ShareNews::Fps { share, fps } => {
                let Some(live) = self.live.as_mut().filter(|live| ours(live, share)) else {
                    return false;
                };
                if live.fps == fps {
                    return false;
                }
                live.fps = fps;
                log!(self.log, "share {share}: runs at {fps} fps");
                self.roster_changed(now, socket);
            }
            ShareNews::Software { share, sentence } => {
                if self.live.as_ref().is_some_and(|live| ours(live, share)) {
                    self.problem_line(String::from(sentence));
                }
            }
            ShareNews::Paused { share, paused } => {
                if self.live.as_ref().is_some_and(|live| ours(live, share)) {
                    self.problem_line(String::from(screen::paused_sentence(paused)));
                }
            }
        }
        true
    }

    // From this host's viewer's thread.
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

    // This host's own newest shape, to everyone watching its share, once its
    // budget and their control channels have room for it.
    fn own_shape(&mut self, now: Instant, socket: &Socket) {
        let own = *self.identity.public();
        let Some(number) = self
            .live
            .as_ref()
            .filter(|live| live.sharer == own)
            .map(|live| live.number)
        else {
            return;
        };
        let watching = |peer: &&mut Peer| peer.share.watching == Some(number);
        let queued = self
            .peers
            .iter_mut()
            .filter(watching)
            .map(|peer| peer.link.waiting())
            .max()
            .unwrap_or(0);
        let Some((id, shape)) = self.screen.shape_out(now, queued) else {
            return;
        };
        for peer in self.peers.iter_mut().filter(watching) {
            for chunk in wire::chunks(number, id, &shape) {
                peer.link.queue(&Message::Shape(chunk));
            }
            peer.flush(socket, now);
        }
    }

    // The links this host's own share goes out on: every friend watching it,
    // at the address media goes to.
    pub(crate) fn publish_share(&mut self) {
        let own = *self.identity.public();
        let number = self
            .live
            .as_ref()
            .filter(|live| live.sharer == own && !self.closed)
            .map(|live| live.number);
        let watching = |peer: &&Peer| number.is_some() && peer.share.watching == number;
        let peers = &self.peers;
        let links = peers.iter().filter(watching).filter_map(|peer| {
            let session = peer.sessions.current.as_ref()?;
            session
                .is_confirmed()
                .then(|| (session.remote_index(), peer.media.to()))
        });
        self.screen.publish(links, || {
            peers
                .iter()
                .filter(watching)
                .filter_map(|peer| {
                    Some(Outlet {
                        sealer: peer.sessions.current.as_ref()?.sealer()?,
                        to: peer.media.to(),
                        sent: Arc::clone(&peer.share.video_out),
                    })
                })
                .collect()
        });
    }

    // Each packet goes on the moment it arrives, which keeps the sharer's
    // spacing, sealed once for each watcher. `plain` is the whole decrypted
    // packet, channel byte first; its prefix is turned from Sent into Relayed
    // in place.
    pub(super) fn on_video(
        &mut self,
        i: usize,
        plain: &mut [u8],
        now: Instant,
        socket: &Socket,
    ) -> bool {
        let Some(number) = self.shared_by(i) else {
            if self.ended_lately(i, now) {
                return false;
            }
            return self.refused_media(i, "video", "they are not sharing", now);
        };
        let peer = &mut self.peers[i];
        // Over the rate is not malformed, so it is counted apart.
        if !peer
            .share
            .limits
            .video
            .take(now, screen::VIDEO_PER_SECOND, screen::VIDEO_BURST)
        {
            self.screen.dropped += 1;
            return false;
        }
        let (key, slot) = (peer.key, peer.slot);
        let payload = plain.get(1..).unwrap_or_default();
        let arrived_us = self.clock.micros(now);
        let packet = match wire::read_video(payload, SENT) {
            Ok((_, packet)) => packet,
            Err(why) => return self.refused_media(i, "video", &why.to_string(), now),
        };
        if let Some(live) = self.live.as_mut()
            && live
                .newest
                .is_none_or(|newest| is_after(packet.frame, newest))
        {
            live.newest = Some(packet.frame);
        }
        self.screen
            .video_in(key, number, &packet, true, arrived_us, now);
        plain[1] = RELAYED;
        plain[2] = slot;
        let timers = self.timers;
        self.peers[i].link.media_passed(now, &timers);
        self.relay(number, plain, now, socket);
        if self.screen.watched == Some(number) {
            self.screen
                .watching
                .video(number, plain.get(1 + wire::PREFIX..).unwrap_or_default());
        }
        false
    }

    // A pointer update goes the same way, newest wins on the far side.
    pub(super) fn on_cursor(
        &mut self,
        i: usize,
        plain: &mut [u8],
        now: Instant,
        socket: &Socket,
    ) -> bool {
        let Some(number) = self.shared_by(i) else {
            if self.ended_lately(i, now) {
                return false;
            }
            return self.refused_media(i, "a pointer update", "they are not sharing", now);
        };
        let peer = &mut self.peers[i];
        if !peer
            .share
            .limits
            .pointer
            .take(now, screen::POINTER_PER_SECOND, screen::POINTER_BURST)
        {
            self.screen.dropped += 1;
            return false;
        }
        let slot = peer.slot;
        let pointer = match wire::read_pointer(plain.get(1..).unwrap_or_default(), SENT) {
            Ok((_, pointer)) => pointer,
            Err(why) => return self.refused_media(i, "a pointer update", &why.to_string(), now),
        };
        plain[1] = RELAYED;
        plain[2] = slot;
        self.relay(number, plain, now, socket);
        if self.screen.watched == Some(number) {
            self.screen.watching.pointer(number, pointer);
        }
        false
    }

    // Seals `plain` once for each friend who watches `number` and sends it
    // where their media goes, with buffers kept for it.
    fn relay(&mut self, number: u32, plain: &[u8], now: Instant, socket: &Socket) {
        let timers = self.timers;
        for other in &mut self.peers {
            if other.share.watching != Some(number) {
                continue;
            }
            let Some(session) = other.sessions.current.as_mut() else {
                continue;
            };
            if session.encrypt(plain, &mut self.video_sealed).is_err() {
                continue;
            }
            other
                .traffic
                .send(socket, &self.video_sealed, other.media.to());
            other.link.media_passed(now, &timers);
            self.screen.relayed += 1;
        }
    }

    // The share friend `i` shares now, if any.
    fn shared_by(&self, i: usize) -> Option<u32> {
        let live = self.live.as_ref()?;
        (live.sharer == self.peers[i].key).then_some(live.number)
    }

    // Friend `i`'s share ended within ENDED_GRACE.
    fn ended_lately(&self, i: usize, now: Instant) -> bool {
        self.share_ended
            .is_some_and(|(key, until)| key == self.peers[i].key && now < until)
    }

    // Video or a pointer from someone who does not share, or that does not
    // parse, goes nowhere and counts as bad.
    fn refused_media(&mut self, i: usize, what: &str, why: &str, now: Instant) -> bool {
        let peer = &self.peers[i];
        let (key, ip) = (peer.key, peer.addr.ip());
        if self.log.is_on() && self.booth_lines.allow(ip, now, &self.log) {
            self.log.line(format!(
                "{what} from {}, dropped: {why}",
                keys::fingerprint(&key)
            ));
        }
        self.drops.bad(now)
    }

    // The control messages sharing adds, from friend `i`. True when the view
    // changed.
    pub(super) fn on_share_message(
        &mut self,
        i: usize,
        message: Message,
        now: Instant,
        socket: &Socket,
    ) -> bool {
        match message {
            Message::ShareStart { fps } => self.took_share_start(i, fps, now, socket),
            Message::ShareStop => {
                if self.shared_by(i).is_some() {
                    log!(self.log, "{}: stopped sharing", who(&self.peers[i]));
                    self.end_share(now, socket);
                    return true;
                }
                false
            }
            Message::Watch { share, on, hevc } => self.took_watch(i, share, on, hevc, now, socket),
            Message::Recover { share, first, last } => {
                let peer = &mut self.peers[i];
                let allowed = peer.share.limits.recovers.take(
                    now,
                    screen::RECOVERS_PER_SECOND,
                    screen::RECOVER_BURST,
                );
                if !allowed {
                    self.screen.dropped += 1;
                } else if peer.share.watching == Some(share)
                    && self.is_live(share)
                    && !self.recover_from(first, last, now, socket)
                {
                    self.drops.bad(now);
                }
                false
            }
            Message::Idr { share, seen } => {
                if self.peers[i].share.watching == Some(share) && self.is_live(share) {
                    self.idr_for(By::Peer(i), seen, now, socket);
                }
                false
            }
            Message::VideoLoss { share, loss } => {
                let peer = &mut self.peers[i];
                let allowed = peer.share.limits.losses.take(
                    now,
                    screen::LOSSES_PER_SECOND,
                    screen::LOSS_BURST,
                );
                if !allowed {
                    self.screen.dropped += 1;
                } else if peer.share.watching == Some(share) && self.is_live(share) {
                    self.loss_from(By::Peer(i), loss, now, socket);
                }
                false
            }
            Message::Shape(chunk) => {
                if self.shared_by(i).is_none() && self.ended_lately(i, now) {
                    return false;
                }
                self.took_shape(i, chunk, now, socket);
                false
            }
            // Only a host sends these.
            _ => {
                self.drops.bad(now);
                false
            }
        }
    }

    fn is_live(&self, share: u32) -> bool {
        self.live.as_ref().is_some_and(|live| live.number == share)
    }

    fn took_share_start(&mut self, i: usize, fps: u8, now: Instant, socket: &Socket) -> bool {
        let key = self.peers[i].key;
        let allowed = self.peers[i].share.limits.asks.take(
            now,
            screen::SHARE_ASKS_PER_SECOND,
            screen::SHARE_ASK_BURST,
        );
        let (answer, changed) = match self
            .live
            .as_ref()
            .map(|live| (live.sharer, live.number, live.fps))
        {
            // A frame rate that changed needs no ask of its own.
            Some((sharer, number, was)) if sharer == key => {
                let changed = was != fps;
                if let Some(live) = self.live.as_mut() {
                    live.fps = fps;
                }
                if changed {
                    self.roster_changed(now, socket);
                    // A client watching hears it from the roster; this
                    // host's own viewer from here.
                    if self.screen.watched == Some(number) {
                        self.screen
                            .watching
                            .event(WatchEvent::Fps { share: number, fps });
                    }
                }
                (ShareAnswer::Granted { share: number }, changed)
            }
            _ if !allowed => {
                self.screen.dropped += 1;
                (ShareAnswer::TooSoon, false)
            }
            Some((sharer, _, _)) => {
                let name = self.name_of(&sharer);
                log!(
                    self.log,
                    "{}: asked to share while {} shares, refused",
                    who(&self.peers[i]),
                    keys::fingerprint(&sharer)
                );
                (ShareAnswer::Busy { key: sharer, name }, false)
            }
            None => {
                let share = self.start_share(key, fps, now, socket);
                let name = self.peers[i].name.clone();
                self.system_line(key, name.clone(), format!("{name} started sharing"));
                (ShareAnswer::Granted { share }, true)
            }
        };
        let peer = &mut self.peers[i];
        peer.link.queue(&Message::ShareAnswer(answer));
        peer.flush(socket, now);
        if changed {
            self.tell_facts(now, socket);
        }
        changed
    }

    fn took_watch(
        &mut self,
        i: usize,
        share: u32,
        on: bool,
        hevc: bool,
        now: Instant,
        socket: &Socket,
    ) -> bool {
        let key = self.peers[i].key;
        let Some(live) = self.live.as_ref().filter(|live| live.number == share) else {
            // A share that ended while the word was on its way.
            return false;
        };
        if live.sharer == key {
            self.drops.bad(now);
            return false;
        }
        let peer = &mut self.peers[i];
        let was = peer.share.watching == Some(share);
        // Said again while watching: its viewer found out what it decodes.
        // That can change the codec, never the IDR the watch began with.
        // Held to the same limit as a press, for the log line only.
        if on && was && peer.share.watcher.hevc != hevc {
            peer.share.watcher.hevc = hevc;
            let logged = peer.share.limits.watches.take(
                now,
                screen::WATCHES_PER_SECOND,
                screen::WATCH_BURST,
            );
            if logged {
                log!(
                    self.log,
                    "{}: decodes hevc: {}",
                    who(peer),
                    crate::log::yes_no(hevc)
                );
            }
            self.tell_facts(now, socket);
            return logged;
        }
        if on == was {
            return false;
        }
        // Past the limit a press still takes effect, or the host and the
        // watcher would disagree about who watches until the next one. What
        // it causes is held back elsewhere: the IDR by IDR_ASK_EVERY, the
        // facts by FACTS_EVERY. Only the log line is left out, and the view
        // waits for its next refresh.
        let logged =
            peer.share
                .limits
                .watches
                .take(now, screen::WATCHES_PER_SECOND, screen::WATCH_BURST);
        if on {
            peer.share.watching = Some(share);
            peer.share.watcher.hevc = hevc;
            if logged {
                log!(
                    self.log,
                    "{}: watches share {share}, decodes hevc: {}",
                    who(peer),
                    crate::log::yes_no(hevc)
                );
            }
            self.started_watching(By::Peer(i), now, socket);
        } else {
            peer.share.watching = None;
            peer.share.watcher.stopped();
            if logged {
                log!(self.log, "{}: stopped watching share {share}", who(peer));
            }
            self.stopped_watching(&key, now, socket);
            self.tell_facts(now, socket);
        }
        logged
    }

    // A shape chunk from the sharer, checked by decode, goes to everyone
    // watching. A shape is let through or held back whole, on its first
    // chunk, by the bytes its total says. Decode checks each chunk against
    // the total the chunk itself carries, so a later chunk must carry the
    // first one's, in turn: then no more bytes go on than were charged.
    // Anything else is no chunk a sharer following the rules sends.
    fn took_shape(&mut self, i: usize, chunk: ShapeChunk, now: Instant, socket: &Socket) {
        let key = self.peers[i].key;
        let Some(live) = self.live.as_mut().filter(|live| live.sharer == key) else {
            self.drops.bad(now);
            return;
        };
        if chunk.share != live.number {
            return;
        }
        let pass = if chunk.index == 0 {
            let fits = self.peers[i].share.limits.shape_bytes.take_many(
                now,
                f64::from(chunk.total),
                screen::SHAPE_BYTES_PER_SECOND,
                screen::SHAPE_BYTES_BURST,
            );
            live.shape = Some(ShapeUnderWay {
                id: chunk.id,
                total: chunk.total,
                next: 1,
                pass: fits,
            });
            fits
        } else {
            match &mut live.shape {
                Some(under_way)
                    if under_way.id == chunk.id
                        && under_way.total == chunk.total
                        && under_way.next == chunk.index =>
                {
                    under_way.next += 1;
                    under_way.pass
                }
                _ => {
                    // The shape under way is over too: nothing of it that
                    // comes after this goes on.
                    live.shape = None;
                    self.drops.bad(now);
                    return;
                }
            }
        };
        if !pass {
            self.screen.dropped += 1;
            return;
        }
        let number = live.number;
        if self.screen.watched == Some(number)
            && let Some((share, id, shape)) = self.screen.shape_in(chunk.clone())
        {
            self.screen.watching.shape(share, id, shape);
        }
        self.tell_watchers(&Message::Shape(chunk), now, socket);
    }

    // To every friend who watches the share now.
    fn tell_watchers(&mut self, message: &Message, now: Instant, socket: &Socket) {
        let Some(number) = self.live.as_ref().map(|live| live.number) else {
            return;
        };
        for peer in &mut self.peers {
            if peer.share.watching == Some(number) {
                peer.link.queue(message);
                peer.flush(socket, now);
            }
        }
    }

    fn start_share(&mut self, sharer: [u8; 32], fps: u8, now: Instant, socket: &Socket) -> u32 {
        let number = self.next_share;
        self.next_share = self.next_share.wrapping_add(1).max(1);
        let name = self.name_of(&sharer);
        self.live = Some(Live {
            number,
            sharer,
            name,
            fps,
            recovered: VecDeque::new(),
            newest: None,
            told: None,
            told_at: None,
            facts_due: None,
            shape: None,
            control: None,
        });
        log!(
            self.log,
            "share {number}: {} shares at {fps} fps",
            keys::fingerprint(&sharer)
        );
        self.roster_changed(now, socket);
        number
    }

    // The share is over for everyone. Its watchers stop, and the roster says
    // so, which closes their viewers.
    pub(super) fn end_share(&mut self, now: Instant, socket: &Socket) {
        self.end_grant(ControlEnd::ShareEnded, now, socket);
        let Some(live) = self.live.take() else {
            return;
        };
        log!(self.log, "share {}: over", live.number);
        for peer in &mut self.peers {
            if peer.share.watching == Some(live.number) {
                peer.share.restart();
            }
        }
        if self.screen.watched == Some(live.number) {
            self.screen.stop_watching();
            self.own_watcher = Watcher::default();
        }
        let own = *self.identity.public();
        if live.sharer == own {
            self.screen.sharing.set_state(OwnShare::Off);
        } else {
            self.share_ended = Some((live.sharer, now + ENDED_GRACE));
            let name = self
                .peers
                .iter()
                .find(|peer| peer.key == live.sharer)
                .map_or(live.name, |peer| peer.name.clone());
            self.system_line(live.sharer, name.clone(), format!("{name} stopped sharing"));
        }
        self.roster_changed(now, socket);
    }

    // The room is over: nobody is told, since everyone is let go.
    pub(super) fn close_share(&mut self) {
        self.close_control();
        self.live = None;
        self.screen.sharing.set_state(OwnShare::Off);
        self.screen.stop_watching();
    }

    // Someone left the room, or their link started over: a share of theirs
    // ends, and what they watched they ask for again.
    pub(super) fn left_share(&mut self, key: &[u8; 32], now: Instant, socket: &Socket) {
        self.left_control(key, now, socket);
        if self.live.as_ref().is_some_and(|live| live.sharer == *key) {
            self.end_share(now, socket);
        } else {
            self.tell_facts(now, socket);
        }
    }

    // A friend left the room: their limits stay for KEPT_FOR, in case they
    // join again.
    pub(super) fn keep_limits(
        &mut self,
        key: [u8; 32],
        share: PeerShare,
        control: PeerControl,
        now: Instant,
    ) {
        self.left_limits
            .retain(|kept| now < kept.until && kept.key != key);
        if self.left_limits.len() >= MOST_KEPT {
            self.left_limits.remove(0);
        }
        self.left_limits.push(Kept {
            key,
            limits: share.limits,
            last_idr: share.watcher.last_idr,
            control,
            until: now + KEPT_FOR,
        });
    }

    // Friend `i` just joined: the limits they left with, if they left
    // lately.
    pub(super) fn limits_back(&mut self, i: usize, now: Instant) {
        let key = self.peers[i].key;
        self.left_limits.retain(|kept| now < kept.until);
        if let Some(at) = self.left_limits.iter().position(|kept| kept.key == key) {
            let kept = self.left_limits.swap_remove(at);
            let peer = &mut self.peers[i];
            peer.share.limits = kept.limits;
            peer.share.watcher.last_idr = kept.last_idr;
            peer.control = kept.control;
        }
    }

    // Someone started watching: the facts that count them, and the IDR for
    // their first picture. When both go now the ask goes in the facts
    // (Facts::idr), so that if this watcher cannot take the share's HEVC, the
    // new encoder's first frame is the one IDR for both. Facts that do not go
    // now, held back by FACTS_EVERY or no different, cannot be moving the
    // share to H.264, which never waits, so the ask then goes on its own.
    fn started_watching(&mut self, by: By, now: Instant, socket: &Socket) {
        let asks = self.idr_goes(by, None, now).is_some();
        if !self.tell_facts_with(asks, now, socket) && asks {
            self.ask_idr(None, now, socket);
        }
    }

    // `seen` is the frame the watcher had when nothing more decoded; a
    // watcher that just started asks with none.
    fn idr_for(&mut self, by: By, seen: Option<u32>, now: Instant, socket: &Socket) {
        if let Some(seen) = self.idr_goes(by, seen, now) {
            self.ask_idr(seen, now, socket);
        }
    }

    // Whether an IDR ask for this watcher goes now, and with what `seen`.
    // One within IDR_ASK_EVERY of the last is dropped, or waits for the gap
    // when it is a new watcher's.
    fn idr_goes(&mut self, by: By, seen: Option<u32>, now: Instant) -> Option<Option<u32>> {
        self.live.as_ref()?;
        let watcher = match by {
            By::Peer(i) => &mut self.peers[i].share.watcher,
            By::Host => &mut self.own_watcher,
        };
        if !watcher.idr_due(now) {
            if seen.is_none() {
                watcher.join_idr_waits = true;
            } else {
                self.screen.dropped += 1;
            }
            return None;
        }
        // A new watcher's ask held back goes with this one, and needs an
        // IDR whatever came before.
        let joined = std::mem::take(&mut watcher.join_idr_waits);
        Some(seen.filter(|_| !joined))
    }

    fn ask_idr(&mut self, seen: Option<u32>, now: Instant, socket: &Socket) {
        let Some(number) = self.live.as_ref().map(|live| live.number) else {
            return;
        };
        self.tell_sharer(
            Answer::Idr { seen },
            Message::Idr {
                share: number,
                seen,
            },
            now,
            socket,
        );
    }

    // A range that another watcher's report already covers is trimmed or
    // goes no further: the sharer acts on each lost frame once. False for a
    // range that names a frame not sent yet, which no watcher could have
    // lost and which NVENC would answer with an IDR, so a hostile watcher
    // cannot buy IDRs with frames that never went out.
    fn recover_from(&mut self, first: u32, last: u32, now: Instant, socket: &Socket) -> bool {
        let own = *self.identity.public();
        let own_newest = self.screen.sharing.newest_frame();
        let Some(live) = self.live.as_mut() else {
            return true;
        };
        let newest = if live.sharer == own {
            own_newest
        } else {
            live.newest
        };
        if newest.is_none_or(|newest| is_after(last, newest)) {
            return false;
        }
        live.recovered
            .retain(|(_, _, at)| now.saturating_duration_since(*at) < RECOVERED_FOR);
        let (mut first, mut last) = (first, last);
        let covered = |frame: u32, (from, to, _): &(u32, u32, Instant)| {
            frame.wrapping_sub(*from) <= to.wrapping_sub(*from)
        };
        while let Some(range) = live.recovered.iter().find(|range| covered(first, range)) {
            if covered(last, range) {
                return true;
            }
            first = range.1.wrapping_add(1);
        }
        while let Some(range) = live.recovered.iter().find(|range| covered(last, range)) {
            last = range.0.wrapping_sub(1);
        }
        if live.recovered.len() == RECOVERED_KEPT {
            live.recovered.pop_front();
        }
        live.recovered.push_back((first, last, now));
        let number = live.number;
        self.tell_sharer(
            Answer::Recover { first, last },
            Message::Recover {
                share: number,
                first,
                last,
            },
            now,
            socket,
        );
        true
    }

    // The sharer hears the worst loss of any watcher once a second, and the
    // first nonzero one at once, so on a bad link the parity catches up
    // within the first second instead of after it.
    fn loss_from(&mut self, by: By, loss: Option<u16>, now: Instant, socket: &Socket) {
        let was = self.worst_video_loss(now);
        let watcher = match by {
            By::Peer(i) => &mut self.peers[i].share.watcher,
            By::Host => &mut self.own_watcher,
        };
        watcher.loss = Some((loss, now));
        if was.unwrap_or(0) == 0 && loss.unwrap_or(0) > 0 {
            self.tell_worst(now, socket);
        }
    }

    fn worst_video_loss(&self, now: Instant) -> Option<u16> {
        let number = self.live.as_ref()?.number;
        let fresh = |watcher: &Watcher| {
            watcher
                .loss
                .filter(|(_, at)| now.saturating_duration_since(*at) < LOSS_KEPT)
                .and_then(|(loss, _)| loss)
        };
        let peers = self
            .peers
            .iter()
            .filter(|peer| peer.share.watching == Some(number))
            .map(|peer| fresh(&peer.share.watcher));
        let own = (self.screen.watched == Some(number)).then(|| fresh(&self.own_watcher));
        peers.chain(own).flatten().max()
    }

    fn tell_worst(&mut self, now: Instant, socket: &Socket) {
        let Some(worst) = self.worst_video_loss(now) else {
            return;
        };
        let Some(number) = self.live.as_ref().map(|live| live.number) else {
            return;
        };
        self.tell_sharer(
            Answer::Loss(Some(percent(worst))),
            Message::VideoLoss {
                share: number,
                loss: Some(worst),
            },
            now,
            socket,
        );
    }

    // To the sharer: this host's own share takes the answer at once; a
    // friend's goes over the control channel.
    fn tell_sharer(&mut self, answer: Answer, message: Message, now: Instant, socket: &Socket) {
        let Some(sharer) = self.live.as_ref().map(|live| live.sharer) else {
            return;
        };
        if sharer == *self.identity.public() {
            self.screen.sharing.answer(answer);
            return;
        }
        if let Some(peer) = self.peers.iter_mut().find(|peer| peer.key == sharer) {
            peer.link.queue(&message);
            peer.flush(socket, now);
        }
    }

    // The host's half of the bitrate rule and of the pacer: how many watch
    // and over what paths. The sharer hears it when it changes.
    pub(super) fn tell_facts(&mut self, now: Instant, socket: &Socket) {
        self.tell_facts_with(false, now, socket);
    }

    // `idr` puts a new watcher's IDR ask in the facts if they go now. True
    // when they went.
    fn tell_facts_with(&mut self, idr: bool, now: Instant, socket: &Socket) -> bool {
        let Some(live) = self.live.as_ref() else {
            return false;
        };
        let number = live.number;
        let own = *self.identity.public();
        let watching: Vec<&Peer> = self
            .peers
            .iter()
            .filter(|peer| peer.share.watching == Some(number))
            .collect();
        let host_watches = self.screen.watched == Some(number);
        let internet = watching
            .iter()
            .filter(|peer| peer.path != crate::view::PathWord::Lan)
            .count() as u8;
        let sharer_lan = live.sharer == own
            || self
                .peers
                .iter()
                .any(|peer| peer.key == live.sharer && peer.path == crate::view::PathWord::Lan);
        let facts = Facts {
            share: number,
            watchers: watching.len() as u8 + u8::from(host_watches),
            internet,
            cap_kbps: if internet == 0 {
                0
            } else {
                self.upload_kbps / u32::from(internet)
            },
            lan: sharer_lan
                && watching
                    .iter()
                    .all(|peer| peer.path == crate::view::PathWord::Lan),
            hevc: watching.iter().all(|peer| peer.share.watcher.hevc)
                && (!host_watches || self.own_watcher.hevc),
            idr: false,
        };
        let Some(live) = self.live.as_mut() else {
            return false;
        };
        if live.told == Some(facts) {
            live.facts_due = None;
            return false;
        }
        // Someone who needs H.264 does not wait: the codec change is the
        // IDR they start with (started_watching). HEVC coming back waits
        // like any other change, and the sharer holds codec changes to its
        // own gap (share::SWITCH_GAP), so this sends no more than a Watch
        // does.
        let needs_h264 = live.told.is_some_and(|told| told.hevc) && !facts.hevc;
        if let Some(due) = live
            .told_at
            .map(|at| at + FACTS_EVERY)
            .filter(|due| now < *due && !needs_h264)
        {
            live.facts_due = Some(due);
            return false;
        }
        live.told = Some(facts);
        live.told_at = Some(now);
        live.facts_due = None;
        log!(
            self.log,
            "share {number}: {} watching, {} over the internet, cap {} kbit/s, all lan {}, all decode hevc {}{}",
            facts.watchers,
            facts.internet,
            facts.cap_kbps,
            crate::log::yes_no(facts.lan),
            crate::log::yes_no(facts.hevc),
            if idr {
                ", with a new watcher's idr ask"
            } else {
                ""
            }
        );
        self.tell_sharer_facts(Facts { idr, ..facts }, now, socket);
        true
    }

    fn tell_sharer_facts(&mut self, facts: Facts, now: Instant, socket: &Socket) {
        let Some(sharer) = self.live.as_ref().map(|live| live.sharer) else {
            return;
        };
        if sharer == *self.identity.public() {
            self.screen.sharing.set_facts(&facts);
            return;
        }
        if let Some(peer) = self.peers.iter_mut().find(|peer| peer.key == sharer) {
            peer.link.queue(&Message::ShareFacts(facts));
            peer.flush(socket, now);
        }
    }

    // Every timer pass: facts held back by FACTS_EVERY, a shape of this
    // host's that waited for room, and the sharer's clock offset to anyone
    // watching whose copy is out of date, and this host's own viewer's.
    pub(super) fn share_step(&mut self, now: Instant, socket: &Socket) {
        self.tell_facts(now, socket);
        self.own_shape(now, socket);
        self.tell_clocks(false, now, socket);
        self.join_idrs(now, socket);
    }

    // New watchers' IDR asks the gap held back, once it is over.
    fn join_idrs(&mut self, now: Instant, socket: &Socket) {
        let Some(number) = self.live.as_ref().map(|live| live.number) else {
            return;
        };
        let due = |watcher: &Watcher| watcher.join_idr_due().is_some_and(|at| at <= now);
        for i in 0..self.peers.len() {
            let share = &self.peers[i].share;
            if share.watching == Some(number) && due(&share.watcher) {
                self.idr_for(By::Peer(i), None, now, socket);
            }
        }
        if self.screen.watched == Some(number) && due(&self.own_watcher) {
            self.idr_for(By::Host, None, now, socket);
        }
    }

    // When the timer thread next has sharing work.
    pub(super) fn share_deadline(&self) -> Option<Instant> {
        let number = self.live.as_ref().map(|live| live.number);
        let peers = self
            .peers
            .iter()
            .filter(|peer| number.is_some() && peer.share.watching == number)
            .map(|peer| &peer.share.watcher);
        let own = (number.is_some() && self.screen.watched == number).then_some(&self.own_watcher);
        [
            self.live.as_ref().and_then(|live| live.facts_due),
            self.screen.shape_due(),
        ]
        .into_iter()
        .chain(peers.chain(own).map(Watcher::join_idr_due))
        .flatten()
        .min()
    }

    // Once a second, with the voice reports.
    pub(super) fn share_reports(&mut self, now: Instant, socket: &Socket) {
        self.share_round_trip(now);
        let worst = self.worst_video_loss(now);
        if worst.is_some() {
            self.tell_worst(now, socket);
        }
        self.tell_clocks(true, now, socket);
    }

    // For the rate's backoff (share::rate): the round trip of the link to
    // whoever watches this host's own share that rose the most past its
    // floor and margin, since the host's uplink carries every one of them.
    fn share_round_trip(&mut self, now: Instant) {
        let own = *self.identity.public();
        let number = self
            .live
            .as_ref()
            .filter(|live| live.sharer == own)
            .map(|live| live.number);
        let worst = self
            .peers
            .iter()
            .filter(|peer| number.is_some() && peer.share.watching == number)
            .filter_map(|peer| peer.link.round_trip(now))
            .max_by(|a, b| a.past_margin_ms().total_cmp(&b.past_margin_ms()));
        self.screen.sharing.set_round_trip(worst);
    }

    // A frame's capture time sits inside its parity, where this host cannot
    // move it onto its own clock as it does voice. So each watcher of a
    // friend's share is told the sharer's offset to this host, once a second
    // and when it changes, and adds its own offset to this host.
    fn tell_clocks(&mut self, every: bool, now: Instant, socket: &Socket) {
        let Some(live) = self.live.as_ref() else {
            return;
        };
        let number = live.number;
        let own = *self.identity.public();
        if live.sharer == own {
            return;
        }
        let Some(sharer) = self.peers.iter().find(|peer| peer.key == live.sharer) else {
            return;
        };
        let Some(offset) = sharer.link.offset.best().map(|sample| sample.offset_us) else {
            return;
        };
        let clock = (offset, sharer.jittery);
        if self.screen.watched == Some(number) {
            self.screen
                .watching
                .set_offset(Some((number, clock.0, clock.1)));
        }
        let message = Message::SharerClock {
            share: number,
            offset_us: clock.0,
            about: clock.1,
        };
        for peer in &mut self.peers {
            if peer.share.watching != Some(number) {
                continue;
            }
            if every || peer.share.watcher.told_clock != Some(clock) {
                peer.share.watcher.told_clock = Some(clock);
                peer.link.queue(&message);
                peer.flush(socket, now);
            }
        }
    }

    pub(super) fn share_view(&self) -> ShareView {
        let own = *self.identity.public();
        let number = self.live.as_ref().map(|live| live.number);
        let watchers = number.map(|number| {
            let peers = self
                .peers
                .iter()
                .filter(|peer| peer.share.watching == Some(number))
                .count();
            (peers + usize::from(self.screen.watched == Some(number))) as u8
        });
        ShareView {
            current: self.live.as_ref().map(|live| CurrentShare {
                key: live.sharer,
                name: self.name_of(&live.sharer),
                number: live.number,
                fps: live.fps,
                yours: live.sharer == own,
                watchers,
            }),
            watching: number.is_some() && self.screen.watched == number,
            own: self.screen.sharing.state(),
            running: self.screen.sharing.running(),
            viewer_open: self.screen.watching.showing(),
            problem: self.screen.problem.clone(),
            control: self.control_view(),
        }
    }

    pub(super) fn name_of(&self, key: &[u8; 32]) -> String {
        if key == self.identity.public() {
            return self.name.clone();
        }
        self.peers.iter().find(|peer| peer.key == *key).map_or_else(
            || crate::control::PERSON_FALLBACK.to_owned(),
            |peer| peer.name.clone(),
        )
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
