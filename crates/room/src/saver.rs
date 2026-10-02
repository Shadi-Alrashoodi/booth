// Writes a room's known list off the threads that answer packets. A save is
// DPAPI and a flushed file: a few milliseconds, far more on a busy disk, and
// a friend's ping must never wait behind one. The room hands a list over at
// most once a second (known::SAVE_GAP); one slot holds it, so while a write is
// slow each newer list replaces the one waiting and only the newest is
// written.

use std::io;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError};

use crate::known::{Save, Saved, Turn};
use crate::log::{Log, log};

// leave() runs on the panel's thread. A write takes milliseconds; a disk
// that has stalled gets this long, and then the write finishes on its own.
// A write cut off by the process ending leaves the old file whole, and one
// that lands after the panel changed the list is left out (known::Turn).
const FINISH_WAIT: Duration = Duration::from_millis(500);

#[derive(Default)]
struct Waiting {
    save: Option<Save>,
    // Write what is waiting now, then end.
    finish: bool,
}

#[derive(Default)]
struct Inbox {
    waiting: Mutex<Waiting>,
    wake: Condvar,
}

impl Inbox {
    fn waiting(&self) -> MutexGuard<'_, Waiting> {
        self.waiting.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

pub(crate) struct Saver {
    inbox: Arc<Inbox>,
    thread: Option<JoinHandle<()>>,
    // Nothing is sent on it. It disconnects when the thread ends, which a
    // join cannot wait for with a time limit.
    done: Receiver<()>,
}

impl Saver {
    pub(crate) fn start(turn: Turn, log: Log) -> io::Result<Saver> {
        let inbox = Arc::new(Inbox::default());
        let (finished, done) = crossbeam_channel::bounded::<()>(0);
        let thread = thread::Builder::new().name("room saver".into()).spawn({
            let inbox = Arc::clone(&inbox);
            move || {
                run(&inbox, &turn, &log);
                drop(finished);
            }
        })?;
        Ok(Saver {
            inbox,
            thread: Some(thread),
            done,
        })
    }

    pub(crate) fn handle(&self) -> SaveHandle {
        SaveHandle(Arc::clone(&self.inbox))
    }

    // Writes what is still waiting and ends the thread. A room that closes
    // writes its last list before leave returns, so the panel reads it back
    // as it was.
    pub(crate) fn finish(&mut self) {
        let Some(thread) = self.thread.take() else {
            return;
        };
        self.inbox.waiting().finish = true;
        self.inbox.wake.notify_one();
        if let Err(RecvTimeoutError::Disconnected) =
            self.done.recv_deadline(Instant::now() + FINISH_WAIT)
        {
            let _ = thread.join();
        }
    }
}

impl Drop for Saver {
    fn drop(&mut self) {
        self.finish();
    }
}

// What the room's own threads hold to hand a list over.
#[derive(Clone)]
pub(crate) struct SaveHandle(Arc<Inbox>);

impl SaveHandle {
    // The list it replaces, if one was still waiting, is wiped as it drops.
    pub(crate) fn save(&self, save: Save) {
        self.0.waiting().save = Some(save);
        self.0.wake.notify_one();
    }
}

fn run(inbox: &Inbox, turn: &Turn, log: &Log) {
    loop {
        let (save, finish) = {
            let mut waiting = inbox.waiting();
            while waiting.save.is_none() && !waiting.finish {
                waiting = inbox
                    .wake
                    .wait(waiting)
                    .unwrap_or_else(PoisonError::into_inner);
            }
            (waiting.save.take(), waiting.finish)
        };
        if let Some(save) = save {
            write(turn, &save, log);
        }
        if finish {
            return;
        }
    }
}

// A list that cannot be written stays in memory for the rest of the room
// and is tried again with the next change.
fn write(turn: &Turn, save: &Save, log: &Log) {
    let name = turn.list().file_name();
    match turn.save(&save.bytes) {
        Ok(Saved::Written) => log!(log, "saved {name}"),
        Ok(Saved::Passed) => log!(
            log,
            "{name} not saved: it was changed in settings, or read by a newer room, since this room read it"
        ),
        Err(err) => log!(log, "{err}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::known::{self, KnownDevice, KnownDevices, List, encode_devices};
    use std::fs;
    use std::path::PathBuf;
    use zeroize::Zeroizing;

    fn devices(count: u8) -> Save {
        let list = KnownDevices {
            devices: (1..=count)
                .map(|n| KnownDevice {
                    key: [n; 32],
                    name: format!("Friend {n}"),
                    first_seen: 1,
                    last_seen: 2,
                    secret: Zeroizing::new([n; 32]),
                })
                .collect(),
            blocked: Vec::new(),
        };
        Save {
            bytes: encode_devices(&list),
        }
    }

    fn folder(test: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("booth-saver-{test}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("make a folder");
        dir
    }

    // A burst of changes costs at most two writes, the one under way and
    // the newest, and the newest is the one on disk once the room closes.
    #[test]
    fn burst_writes_the_newest() {
        let dir = folder("burst");
        let (log, captured) = Log::capture(64);
        let (_, turn) = known::room_devices(&dir);
        let mut saver = Saver::start(turn, log).expect("the saver starts");
        let handle = saver.handle();
        for count in 1..=20 {
            handle.save(devices(count));
        }
        saver.finish();
        let lines = captured.lines();
        assert!(!lines.is_empty() && lines.len() <= 2, "{lines:?}");
        assert!(
            lines.iter().all(|line| line == "saved devices.bin"),
            "{lines:?}"
        );
        let kept = known::devices(&dir).expect("the list reads");
        assert_eq!(kept.devices.len(), 20, "the newest list is the one on disk");
        let _ = fs::remove_dir_all(&dir);
    }

    // A device removed in settings after the room closed stays removed,
    // however late the room's last save comes.
    #[test]
    fn a_late_save_never_undoes_a_remove() {
        let dir = folder("late");
        let (log, captured) = Log::capture(64);
        let (_, turn) = known::room_devices(&dir);
        known::save(&dir, List::Devices, &devices(2).bytes).expect("an earlier save");
        known::remove_device(&dir, &[2; 32]).expect("removed in settings");
        let mut saver = Saver::start(turn, log).expect("the saver starts");
        saver.handle().save(devices(2));
        saver.finish();
        let kept = known::devices(&dir).expect("the list reads");
        assert_eq!(kept.devices.len(), 1);
        assert_eq!(kept.devices[0].key, [1; 32]);
        let lines = captured.lines();
        assert!(
            lines
                .iter()
                .any(|line| line.starts_with("devices.bin not saved: it was changed in settings")),
            "{lines:?}"
        );

        // A room opened after that has the list again, and saves it.
        let (log, captured) = Log::capture(64);
        let (_, turn) = known::room_devices(&dir);
        let mut saver = Saver::start(turn, log).expect("the saver starts");
        saver.handle().save(devices(3));
        saver.finish();
        assert_eq!(captured.lines(), ["saved devices.bin"]);
        assert_eq!(known::devices(&dir).unwrap().devices.len(), 3);
        let _ = fs::remove_dir_all(&dir);
    }
}
