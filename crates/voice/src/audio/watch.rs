// The device lists, kept current while something shows them. One thread owns
// the enumerator and Windows' change notifications; the owner reads the last
// lists whenever it draws.

use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossbeam_channel::{RecvTimeoutError, Sender};

use super::devices::Lists;
use super::error::AudioError;
use super::wasapi::Session;

// Plugging in a headset brings a burst of notifications (added, state, one
// default per role); they are read as one change.
const SETTLE: Duration = Duration::from_millis(50);

enum Message {
    Changed,
    Stop,
}

type Latest = Arc<Mutex<Option<Result<Lists, AudioError>>>>;

pub struct Watch {
    control: Sender<Message>,
    thread: Option<JoinHandle<()>>,
    latest: Latest,
}

impl Watch {
    // `changed` runs on the watch thread after each new reading, the first
    // one included.
    pub fn start(changed: impl Fn() + Send + 'static) -> Result<Watch, AudioError> {
        let (control, rx) = crossbeam_channel::unbounded();
        let notices = control.clone();
        let latest: Latest = Arc::default();
        let thread_latest = Arc::clone(&latest);
        let thread = thread::Builder::new()
            .name(String::from("audio devices"))
            .spawn(move || {
                let publish = |lists| {
                    *thread_latest.lock().unwrap_or_else(PoisonError::into_inner) = Some(lists);
                    changed();
                };
                let session = match Session::new() {
                    Ok(session) => session,
                    Err(err) => return publish(Err(err)),
                };
                // Without notifications the lists are still right when the
                // screen opens; they only miss a device plugged in while it
                // is open.
                let registration = session
                    .watch(move |_| {
                        let _ = notices.send(Message::Changed);
                    })
                    .ok();
                publish(session.lists());
                'watch: while let Ok(Message::Changed) = rx.recv() {
                    loop {
                        match rx.recv_timeout(SETTLE) {
                            Ok(Message::Changed) => {}
                            Ok(Message::Stop) | Err(RecvTimeoutError::Disconnected) => {
                                break 'watch;
                            }
                            Err(RecvTimeoutError::Timeout) => break,
                        }
                    }
                    publish(session.lists());
                }
                drop(registration);
                drop(session);
            })
            .map_err(|err| AudioError::Thread(err.to_string()))?;
        Ok(Watch {
            control,
            thread: Some(thread),
            latest,
        })
    }

    // None until the first reading is in.
    pub fn lists(&self) -> Option<Result<Lists, AudioError>> {
        self.latest
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        let _ = self.control.send(Message::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
