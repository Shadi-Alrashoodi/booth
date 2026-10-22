// PC sound in a room: what a sharer's PC plays, sent with the share on a
// stream of its own (Channel::Sound) to the share's watchers whose copies
// read it (peer::PC_SOUND_SINCE), through the host as video is.
//
// A watcher hears it through one ear (hear.rs), the share watched's: made by
// the first frame of it that comes, and let go with the watch, the share, or
// the sharer saying its sound is off. The receive thread pushes into it under
// the state lock and the render thread pulls from it through the mixer, each
// holding the ear's own lock for one frame, as voice's ears are (talk.rs).

mod hear;
mod wire;

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use crossbeam_channel::Sender;

use crate::limit::Bucket;
use crate::log::{Log, log};
use crate::peer::Clock;
use crate::talk::{RETIRED_FOR, ToSpeaker};
use crate::view::{Latency, Numbers, SoundNumbers};

pub(crate) use hear::{Ear, Stream};
pub(crate) use wire::{MAX_SOUND, SoundFrame, read_relayed, read_sent, relay_in_place};

// A sharer sends 100 frames a second, 10 ms each. Past this rate the host
// drops what comes, since each frame goes on to every watcher; the burst
// covers a sharer's capture thread catching up after a stall.
pub(crate) const SOUND_PER_SECOND: f64 = 150.0;
pub(crate) const SOUND_BURST: f64 = 50.0;

// Half the level captured: music stays under the limiter and voices on top.
pub(crate) const SOUND_LEVEL: f32 = 0.5;

// New ears a second: each is a stereo decoder and its buffer, kept
// RETIRED_FOR after it goes. One plays at a time, and a watch, or the sharer
// turning sound off and on, makes one; a sharer that says off before every
// frame, which only a hostile one does, would otherwise make one per frame.
const NEW_EARS_PER_SECOND: f64 = 2.0;
const NEW_EAR_BURST: f64 = 4.0;

// The ear of the share watched.
struct Listening {
    share: u32,
    // Its sharer's, which the mixer knows the stream by.
    sharer: [u8; 32],
    ear: Arc<Mutex<Ear>>,
}

// The room's side of PC sound on a watcher, under the state lock: the ear of
// the share it watches, those lately let go, and what its sharer last said.
pub(crate) struct Sound {
    speaker: Sender<ToSpeaker>,
    clock: Clock,
    log: Log,
    listening: Option<Listening>,
    retired: Vec<(Arc<Mutex<Ear>>, Instant)>,
    new_ears: Bucket,
    // The share whose sharer said its sound is off. Frames captured before
    // the press can come after the word, and make no ear: only a frame that
    // starts a spell from a reset encoder does, or the sharer saying on.
    off: Option<u32>,
    // What the sharer of the share watched last said: on or off.
    told: Option<(u32, bool)>,
    // Frames of the share watched that no ear took: past the new-ear limit,
    // or after off.
    dropped: u64,
}

impl Sound {
    pub(crate) fn new(speaker: Sender<ToSpeaker>, clock: Clock, log: Log, now: Instant) -> Sound {
        Sound {
            speaker,
            clock,
            log,
            listening: None,
            retired: Vec::new(),
            new_ears: Bucket::full(now, NEW_EAR_BURST),
            off: None,
            told: None,
            dropped: 0,
        }
    }

    // A frame of the share this PC watches, from its sharer, checked by
    // read_relayed. `captured` is its capture time on this PC's ping clock,
    // and whether that is only about right, once the offsets are known. True
    // when it made the ear, which the numbers show.
    pub(crate) fn hear(
        &mut self,
        sharer: [u8; 32],
        frame: &SoundFrame,
        captured: Option<(u64, bool)>,
        now: Instant,
    ) -> bool {
        let fits = self
            .listening
            .as_ref()
            .is_some_and(|listening| listening.share == frame.share && listening.sharer == sharer);
        if !fits && !self.listen(sharer, frame, now) {
            return false;
        }
        let took = self
            .listening
            .as_ref()
            .is_some_and(|listening| lock(&listening.ear).push(frame, captured));
        if !took {
            self.dropped += 1;
        }
        !fits
    }

    // A new ear for the share of `frame`, in place of any other: one plays
    // at a time. False when none was made.
    fn listen(&mut self, sharer: [u8; 32], frame: &SoundFrame, now: Instant) -> bool {
        self.retire(now);
        if self.off == Some(frame.share) && !frame.first {
            self.dropped += 1;
            return false;
        }
        if !self.new_ears.take(now, NEW_EARS_PER_SECOND, NEW_EAR_BURST) {
            self.dropped += 1;
            return false;
        }
        let ear = match Ear::new() {
            Ok(ear) => Arc::new(Mutex::new(ear)),
            Err(err) => {
                log!(
                    self.log,
                    "pc sound: could not make a decoder for {}: {err}",
                    keys::fingerprint(&sharer)
                );
                return false;
            }
        };
        let _ = self
            .speaker
            .send(ToSpeaker::AddStream(sharer, Arc::clone(&ear), 1.0));
        self.off = None;
        self.listening = Some(Listening {
            share: frame.share,
            sharer,
            ear,
        });
        true
    }

    // The sharer of `share`, which this PC watches, says whether its sound
    // goes out, for the panel's words. Its frames never wait for the word,
    // which can come after the first of them; off lets the ear go.
    pub(crate) fn said(&mut self, share: u32, on: bool, now: Instant) {
        self.told = Some((share, on));
        if on {
            if self.off == Some(share) {
                self.off = None;
            }
            return;
        }
        if self
            .listening
            .as_ref()
            .is_some_and(|listening| listening.share == share)
        {
            self.retire(now);
        }
        self.off = Some(share);
    }

    // The watch is over, or the share is.
    pub(crate) fn stop(&mut self, now: Instant) {
        self.retire(now);
        self.off = None;
        self.told = None;
    }

    // Its next 2.5 ms still play, fading out, and the last reference stays
    // here for a while, so the decoder goes on this thread and not the
    // render thread.
    fn retire(&mut self, now: Instant) {
        if let Some(listening) = self.listening.take() {
            let _ = self.speaker.send(ToSpeaker::RemoveStream(listening.sharer));
            self.retired.push((listening.ear, now + RETIRED_FOR));
        }
    }

    // The sharer of `share` said its sound goes out.
    pub(crate) fn says_on(&self, share: u32) -> bool {
        self.told == Some((share, true))
    }

    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        self.retired.iter().map(|(_, until)| *until).min()
    }

    pub(crate) fn tick(&mut self, now: Instant) {
        self.retired.retain(|(_, until)| now < *until);
        stats::gauge!("room.sound.retired", self.retired);
        if stats::mem::ON
            && let Some(listening) = &self.listening
        {
            lock(&listening.ear).gauge();
        }
    }

    // The numbers the stats panel shows about the PC sound this PC hears.
    pub(crate) fn fill(&self, numbers: &mut Numbers, now: Instant) {
        numbers.sound_dropped = self.dropped;
        let Some(listening) = &self.listening else {
            return;
        };
        // Made before the lock, so the render thread never waits on an
        // allocation, and sorted after it.
        let mut recent = Vec::with_capacity(hear::SAMPLES_KEPT);
        let (stats, about) = {
            let ear = lock(&listening.ear);
            (ear.stats(), ear.heard(self.clock.micros(now), &mut recent))
        };
        numbers.sound = Some(SoundNumbers {
            capture_to_ear: share::spread(&mut recent).map(|(median_ms, p95_ms)| Latency {
                median_ms,
                p95_ms,
                about,
            }),
            depth_ms: stats.depth_ms,
            loss_pct: stats.loss_percent,
            made_up: stats.concealed,
        });
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
