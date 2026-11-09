// Sound cues: a short sound in this PC's own speakers when the room changes,
// so nobody goes on talking to a friend who has gone. They are decided from
// each view the room builds, so what is heard matches the people list, and
// on the room's threads, so they play while the panel is minimized beside a
// game. Nothing here goes near the microphone or the socket: a cue goes into
// a slot the render thread plays from (talk/chime.rs).

use std::ops::RangeInclusive;
use std::time::Instant;

use crate::limit::Bucket;
use crate::talk::Chime;
use crate::view::{LinkState, Notice, Role, View};

pub const CUE_VOLUME: RangeInclusive<u8> = 10..=150;
pub const DEFAULT_CUE_VOLUME: u8 = 100;
// Room cues follow what the host sends, so seven people joining at once, or
// a host sending rosters as fast as it may, play three and then one a
// second. A cue over that is dropped, never held: late, it would mark a
// moment long past.
const ROOM_CUES_PER_SECOND: f64 = 1.0;
const ROOM_CUE_BURST: f64 = 3.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SoundCues {
    pub on: bool,
    pub volume: u8,
    pub joins: bool,
    pub own_mute: bool,
    pub friend_connection: bool,
    pub own_connection: bool,
    pub shares: bool,
}

impl Default for SoundCues {
    fn default() -> SoundCues {
        SoundCues {
            on: true,
            volume: DEFAULT_CUE_VOLUME,
            joins: true,
            own_mute: true,
            friend_connection: true,
            own_connection: true,
            shares: true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OwnVoice {
    Open,
    Muted,
    Deafened,
}

#[derive(Clone)]
struct Seen {
    friends: Vec<([u8; 32], bool)>,
    voice: OwnVoice,
    role: Role,
    link: LinkState,
    closed: bool,
    failed: bool,
    other_share: Option<([u8; 32], u32)>,
}

impl Seen {
    fn of(view: &View) -> Seen {
        Seen {
            friends: view
                .people
                .iter()
                .filter(|person| !person.is_you)
                .map(|person| (person.key, person.reconnecting))
                .collect(),
            voice: if view.voice.deafened {
                OwnVoice::Deafened
            } else if view.voice.muted {
                OwnVoice::Muted
            } else {
                OwnVoice::Open
            },
            role: view.role,
            link: view.strip.state,
            closed: view.strip.state == LinkState::Closed
                && view.notice == Some(Notice::RoomClosed),
            failed: view.notice == Some(Notice::SocketFailed),
            other_share: view
                .share
                .current
                .as_ref()
                .filter(|current| !current.yours)
                .map(|current| (current.key, current.number)),
        }
    }
}

pub(crate) struct Cues {
    seen: Seen,
    epoch: u32,
    settings: SoundCues,
    room: Bucket,
}

impl Cues {
    pub(crate) fn new(view: &View, epoch: u32, settings: SoundCues, now: Instant) -> Cues {
        Cues {
            seen: Seen::of(view),
            epoch,
            settings,
            room: Bucket::full(now, ROOM_CUE_BURST),
        }
    }

    pub(crate) fn set(&mut self, settings: SoundCues) {
        self.settings = settings;
    }

    pub(crate) fn follow(
        &mut self,
        view: &View,
        epoch: u32,
        now: Instant,
    ) -> (Option<Chime>, Option<Chime>) {
        self.step(Seen::of(view), epoch, now)
    }

    fn step(&mut self, next: Seen, epoch: u32, now: Instant) -> (Option<Chime>, Option<Chime>) {
        let old = std::mem::replace(&mut self.seen, next);
        let next = &self.seen;
        // The first roster on a new link, at join or after a new handshake,
        // is where the room stands rather than what changed in it, so it
        // becomes the baseline and plays no room cue.
        let fresh = self.epoch != epoch;
        self.epoch = epoch;
        let settings = self.settings;
        if !settings.on {
            return (None, None);
        }
        let closed = next.role == Role::Client && next.closed && !old.closed;
        // A host's failed socket empties its people list at once, though
        // nobody left.
        let room = if fresh || next.failed && !old.failed {
            None
        } else {
            [
                (
                    settings.joins
                        && (closed
                            || old.friends.iter().any(|(key, _)| {
                                !next.friends.iter().any(|(other, _)| other == key)
                            })),
                    Chime::FriendLeaves,
                ),
                (
                    settings.joins
                        && next
                            .friends
                            .iter()
                            .any(|(key, _)| !old.friends.iter().any(|(other, _)| other == key)),
                    Chime::FriendJoins,
                ),
                (
                    settings.friend_connection
                        && next.friends.iter().any(|(key, reconnecting)| {
                            *reconnecting && old.friends.contains(&(*key, false))
                        }),
                    Chime::FriendDrops,
                ),
                (
                    settings.friend_connection
                        && next.friends.iter().any(|(key, reconnecting)| {
                            !*reconnecting && old.friends.contains(&(*key, true))
                        }),
                    Chime::FriendBack,
                ),
                (
                    settings.shares && old.other_share.is_some() && next.other_share.is_none(),
                    Chime::ShareStops,
                ),
                (
                    settings.shares
                        && next.other_share.is_some()
                        && next.other_share != old.other_share,
                    Chime::ShareStarts,
                ),
            ]
            .into_iter()
            .find_map(|(changed, chime)| changed.then_some(chime))
            // The host closing the room happens once, so it skips the limit.
            // Own cues skip it too: this PC's keys make the mute and deafen
            // ones, and the link ones wait on a timer of seconds however a
            // host behaves.
            .filter(|chime| {
                closed && *chime == Chime::FriendLeaves
                    || self.room.take(now, ROOM_CUES_PER_SECOND, ROOM_CUE_BURST)
            })
        };
        let link = if next.failed && !old.failed
            || next.role == Role::Client
                && old.link == LinkState::Live
                && matches!(next.link, LinkState::Reconnecting | LinkState::Lost)
        {
            Some(Chime::YouDrop)
        } else if next.role == Role::Client
            && matches!(old.link, LinkState::Reconnecting | LinkState::Lost)
            && next.link == LinkState::Live
        {
            Some(Chime::YouBack)
        } else {
            None
        };
        let voice = match (old.voice, next.voice) {
            (OwnVoice::Open | OwnVoice::Muted, OwnVoice::Deafened) => Some(Chime::YouDeafen),
            (OwnVoice::Deafened, OwnVoice::Open | OwnVoice::Muted) => Some(Chime::YouUndeafen),
            (OwnVoice::Open, OwnVoice::Muted) => Some(Chime::YouMute),
            (OwnVoice::Muted, OwnVoice::Open) => Some(Chime::YouUnmute),
            _ => None,
        };
        let own = link
            .filter(|_| settings.own_connection)
            .or(voice.filter(|_| settings.own_mute));
        (room, own)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn seen() -> Seen {
        Seen {
            friends: vec![([1; 32], false)],
            voice: OwnVoice::Open,
            role: Role::Client,
            link: LinkState::Live,
            closed: false,
            failed: false,
            other_share: None,
        }
    }

    fn decider(old: Seen, settings: SoundCues, now: Instant) -> Cues {
        Cues {
            seen: old,
            epoch: 0,
            settings,
            room: Bucket::full(now, ROOM_CUE_BURST),
        }
    }

    const EVENTS: [Chime; 12] = [
        Chime::FriendJoins,
        Chime::FriendLeaves,
        Chime::FriendDrops,
        Chime::FriendBack,
        Chime::ShareStarts,
        Chime::ShareStops,
        Chime::YouMute,
        Chime::YouUnmute,
        Chime::YouDeafen,
        Chime::YouUndeafen,
        Chime::YouDrop,
        Chime::YouBack,
    ];

    type Played = (Option<Chime>, Option<Chime>);

    fn event(chime: Chime) -> (Seen, Seen, Played) {
        let (mut old, mut next) = (seen(), seen());
        let own = match chime {
            Chime::FriendJoins => {
                next.friends.push(([2; 32], false));
                false
            }
            Chime::FriendLeaves => {
                next.friends.clear();
                false
            }
            Chime::FriendDrops => {
                next.friends[0].1 = true;
                false
            }
            Chime::FriendBack => {
                old.friends[0].1 = true;
                false
            }
            Chime::ShareStarts => {
                next.other_share = Some(([1; 32], 1));
                false
            }
            Chime::ShareStops => {
                old.other_share = Some(([1; 32], 1));
                false
            }
            Chime::YouMute => {
                next.voice = OwnVoice::Muted;
                true
            }
            Chime::YouUnmute => {
                old.voice = OwnVoice::Muted;
                true
            }
            Chime::YouDeafen => {
                next.voice = OwnVoice::Deafened;
                true
            }
            Chime::YouUndeafen => {
                old.voice = OwnVoice::Deafened;
                true
            }
            Chime::YouDrop => {
                next.link = LinkState::Reconnecting;
                true
            }
            Chime::YouBack => {
                old.link = LinkState::Lost;
                true
            }
        };
        (
            old,
            next,
            if own {
                (None, Some(chime))
            } else {
                (Some(chime), None)
            },
        )
    }

    #[test]
    fn each_event_plays_once() {
        let now = Instant::now();
        for chime in EVENTS {
            let (old, next, want) = event(chime);
            let mut cues = decider(old, SoundCues::default(), now);
            assert_eq!(cues.step(next.clone(), 0, now), want, "{chime:?}");
            assert_eq!(cues.step(next, 0, now), (None, None), "{chime:?}");
        }
    }

    #[test]
    fn nothing_while_off() {
        let now = Instant::now();
        for chime in EVENTS {
            for whole in [false, true] {
                let mut settings = SoundCues::default();
                if whole {
                    settings.on = false;
                } else {
                    match chime {
                        Chime::FriendJoins | Chime::FriendLeaves => settings.joins = false,
                        Chime::FriendDrops | Chime::FriendBack => {
                            settings.friend_connection = false
                        }
                        Chime::ShareStarts | Chime::ShareStops => settings.shares = false,
                        Chime::YouMute
                        | Chime::YouUnmute
                        | Chime::YouDeafen
                        | Chime::YouUndeafen => settings.own_mute = false,
                        Chime::YouDrop | Chime::YouBack => settings.own_connection = false,
                    }
                }
                let (old, next, _) = event(chime);
                let mut cues = decider(old, settings, now);
                assert_eq!(cues.step(next.clone(), 0, now), (None, None), "{chime:?}");
                cues.set(SoundCues {
                    volume: 10,
                    ..SoundCues::default()
                });
                assert_eq!(cues.step(next, 0, now), (None, None), "{chime:?}");
            }
        }
    }

    #[test]
    fn disabled_kinds_leave_the_next_one() {
        let now = Instant::now();
        let mut next = seen();
        next.friends.push(([2; 32], true));
        next.friends[0].1 = true;
        next.other_share = Some(([1; 32], 1));
        next.link = LinkState::Reconnecting;
        next.voice = OwnVoice::Deafened;
        let settings = SoundCues {
            joins: false,
            own_connection: false,
            ..SoundCues::default()
        };
        let mut cues = decider(seen(), settings, now);
        assert_eq!(
            cues.step(next.clone(), 0, now),
            (Some(Chime::FriendDrops), Some(Chime::YouDeafen))
        );
        let mut cues = decider(
            seen(),
            SoundCues {
                friend_connection: false,
                ..settings
            },
            now,
        );
        assert_eq!(
            cues.step(next, 0, now),
            (Some(Chime::ShareStarts), Some(Chime::YouDeafen))
        );
        for i in 0..3 {
            let mut next = seen();
            next.friends.push(([i + 2; 32], false));
            cues.step(next, 0, now);
        }
        cues.set(SoundCues::default());
        let mut next = seen();
        next.friends.push(([9; 32], false));
        assert_eq!(cues.step(next, 0, now).0, Some(Chime::FriendLeaves));
    }

    #[test]
    fn fresh_roster_plays_nothing() {
        let now = Instant::now();
        let mut old = seen();
        old.link = LinkState::Reconnecting;
        let mut cues = decider(old, SoundCues::default(), now);
        let mut next = seen();
        next.friends = (1..=7).map(|key| ([key; 32], key == 7)).collect();
        next.other_share = Some(([3; 32], 4));
        assert_eq!(
            cues.step(next.clone(), 1, now),
            (None, Some(Chime::YouBack))
        );
        assert_eq!(cues.step(next.clone(), 1, now), (None, None));
        next.friends[6].1 = false;
        assert_eq!(cues.step(next.clone(), 1, now).0, Some(Chime::FriendBack));
        next.friends.clear();
        next.other_share = None;
        assert_eq!(cues.step(next, 2, now), (None, None));
    }

    #[test]
    fn disabled_events_spend_nothing() {
        let now = Instant::now();
        let settings = SoundCues {
            joins: false,
            shares: false,
            friend_connection: false,
            ..SoundCues::default()
        };
        let mut cues = decider(seen(), settings, now);
        let mut next = seen();
        for key in 2..=7 {
            next.friends.push(([key; 32], false));
            next.other_share = Some(([key; 32], u32::from(key)));
            assert_eq!(cues.step(next.clone(), 0, now), (None, None));
        }
        cues.set(SoundCues::default());
        next.friends.push(([8; 32], false));
        assert_eq!(cues.step(next.clone(), 0, now).0, Some(Chime::FriendJoins));
        for key in [6, 7] {
            next.friends.retain(|(friend, _)| *friend != [key; 32]);
            assert_eq!(cues.step(next.clone(), 0, now).0, Some(Chime::FriendLeaves));
        }
        cues.set(SoundCues {
            joins: false,
            ..SoundCues::default()
        });
        next.link = LinkState::Closed;
        next.closed = true;
        next.other_share = None;
        assert_eq!(cues.step(next, 0, now), (None, None));
    }

    #[test]
    fn one_room_cue_a_step() {
        let now = Instant::now();
        let mut old = seen();
        old.other_share = Some(([1; 32], 1));
        let mut next = seen();
        next.friends.clear();
        next.friends.push(([2; 32], false));
        let mut cues = decider(old, SoundCues::default(), now);
        assert_eq!(cues.step(next, 0, now).0, Some(Chime::FriendLeaves));
        let mut next = seen();
        next.friends.push(([2; 32], false));
        next.other_share = Some(([2; 32], 1));
        let mut cues = decider(seen(), SoundCues::default(), now);
        assert_eq!(cues.step(next, 0, now).0, Some(Chime::FriendJoins));
        let mut old = seen();
        old.friends = (1..=7).map(|key| ([key; 32], false)).collect();
        let mut next = old.clone();
        for (_, reconnecting) in &mut next.friends {
            *reconnecting = true;
        }
        let mut cues = decider(old, SoundCues::default(), now);
        assert_eq!(cues.step(next, 0, now), (Some(Chime::FriendDrops), None));
    }

    #[test]
    fn three_then_one_a_second() {
        let now = Instant::now();
        let mut cues = decider(seen(), SoundCues::default(), now);
        let mut next = seen();
        for key in 2..=11 {
            next.friends.push(([key; 32], false));
            assert_eq!(
                cues.step(
                    next.clone(),
                    0,
                    now + Duration::from_millis(u64::from(key) * 10)
                )
                .0,
                (key <= 4).then_some(Chime::FriendJoins)
            );
        }
        next.voice = OwnVoice::Muted;
        assert_eq!(
            cues.step(next.clone(), 0, now + Duration::from_millis(120)),
            (None, Some(Chime::YouMute))
        );
        next.friends.push(([12; 32], false));
        assert_eq!(
            cues.step(next.clone(), 0, now + Duration::from_millis(1120))
                .0,
            Some(Chime::FriendJoins)
        );
        next.link = LinkState::Closed;
        next.closed = true;
        assert_eq!(
            cues.step(next.clone(), 0, now + Duration::from_millis(1120))
                .0,
            Some(Chime::FriendLeaves)
        );
        assert_eq!(
            cues.step(next, 0, now + Duration::from_millis(1120)),
            (None, None)
        );
    }

    #[test]
    fn own_link() {
        let now = Instant::now();
        for role in [Role::Host, Role::Client] {
            let mut old = seen();
            old.role = role;
            old.link = LinkState::Connecting;
            let mut cues = decider(old.clone(), SoundCues::default(), now);
            for (link, want) in [
                (LinkState::Live, None),
                (LinkState::Reconnecting, Some(Chime::YouDrop)),
                (LinkState::Lost, None),
                (LinkState::Live, Some(Chime::YouBack)),
                (LinkState::Closed, None),
                (LinkState::Live, None),
                (LinkState::Lost, Some(Chime::YouDrop)),
                (LinkState::Lost, None),
                (LinkState::Live, Some(Chime::YouBack)),
            ] {
                old.link = link;
                assert_eq!(
                    cues.step(old.clone(), 0, now).1,
                    if role == Role::Client { want } else { None }
                );
            }
            old.failed = true;
            assert_eq!(cues.step(old.clone(), 0, now).1, Some(Chime::YouDrop));
            assert_eq!(cues.step(old, 0, now).1, None);
        }
    }

    #[test]
    fn a_failed_socket_is_not_friends_leaving() {
        let now = Instant::now();
        let mut old = seen();
        old.role = Role::Host;
        let mut next = old.clone();
        next.friends.clear();
        next.failed = true;
        let mut cues = decider(old, SoundCues::default(), now);
        assert_eq!(cues.step(next, 0, now), (None, Some(Chime::YouDrop)));
    }

    #[test]
    fn own_voice() {
        let now = Instant::now();
        for (from, to, want) in [
            (OwnVoice::Open, OwnVoice::Muted, Chime::YouMute),
            (OwnVoice::Muted, OwnVoice::Open, Chime::YouUnmute),
            (OwnVoice::Open, OwnVoice::Deafened, Chime::YouDeafen),
            (OwnVoice::Muted, OwnVoice::Deafened, Chime::YouDeafen),
            (OwnVoice::Deafened, OwnVoice::Muted, Chime::YouUndeafen),
            (OwnVoice::Deafened, OwnVoice::Open, Chime::YouUndeafen),
        ] {
            let mut old = seen();
            old.voice = from;
            let mut next = seen();
            next.voice = to;
            let mut cues = decider(old, SoundCues::default(), now);
            assert_eq!(cues.step(next, 0, now).1, Some(want));
        }
    }

    #[test]
    fn a_new_share_number_is_a_start() {
        let now = Instant::now();
        let mut old = seen();
        old.other_share = Some(([1; 32], 1));
        let mut next = seen();
        next.other_share = Some(([1; 32], 2));
        let mut cues = decider(old, SoundCues::default(), now);
        assert_eq!(cues.step(next, 0, now).0, Some(Chime::ShareStarts));
    }
}
