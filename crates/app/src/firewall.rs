// The firewall step as the panel runs it: the check at every start, Allow
// through the administrator prompt, and a log line for each step.

use std::io;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use eframe::egui::Context;
use net::firewall::{self, FirewallState};

use crate::backlog::Backlog;
use crate::elevated;
use crate::messages;
use crate::screens::start::Note;
use crate::win::{self, Elevated};

// How long the panel shows nothing but its title while the check runs. A
// firewall service that hangs is no reason to keep Booth from starting, and
// a firewall that cannot be read counts as nothing to do.
const CHECK_LIMIT: Duration = Duration::from_secs(2);

pub struct Firewall {
    port: u16,
    check: Option<Check>,
    prompt: Option<Receiver<Prompted>>,
    // The latest answer: from the start, or from after the prompt.
    state: Option<FirewallState>,
}

struct Check {
    thread: JoinHandle<(FirewallState, Duration)>,
    // From the first frame that waited for it, which is what the user sees.
    waited_since: Option<Instant>,
}

struct Prompted {
    ran: Elevated,
    // The rules read again, whenever the helper ran at all.
    after: Option<FirewallState>,
}

impl Firewall {
    pub fn check_now(port: u16, backlog: &mut Backlog) -> Firewall {
        let mut firewall = Firewall {
            port,
            check: None,
            prompt: None,
            state: None,
        };
        let started = thread::Builder::new()
            .name("firewall check".into())
            .spawn(move || {
                let started = Instant::now();
                (look(port), started.elapsed())
            });
        match started {
            Ok(thread) => {
                firewall.check = Some(Check {
                    thread,
                    waited_since: None,
                });
            }
            Err(err) => firewall.settle(
                "at start",
                FirewallState::Unknown(format!("could not start the check: {err}")),
                backlog,
            ),
        }
        firewall
    }

    // The answer from the start, once the check has given one or taken too
    // long. A check that ran out of time is left to finish on its own.
    pub fn checked(&mut self, backlog: &mut Backlog) -> Option<&FirewallState> {
        let waited = self.check.as_mut().map(|check| {
            let since = check.waited_since.get_or_insert_with(Instant::now);
            since.elapsed()
        });
        if let Some(check) = self.check.take_if(|check| check.thread.is_finished()) {
            let (state, took) = check.thread.join().unwrap_or_else(|_| {
                let panicked = String::from("the check stopped with a panic");
                (FirewallState::Unknown(panicked), Duration::ZERO)
            });
            self.settle(
                &format!("at start, {} ms", took.as_millis()),
                state,
                backlog,
            );
        } else if waited.is_some_and(|waited| waited >= CHECK_LIMIT) {
            self.check = None;
            let late = format!(
                "windows firewall did not answer within {} s",
                CHECK_LIMIT.as_secs()
            );
            self.settle("at start", FirewallState::Unknown(late), backlog);
        }
        if self.check.is_some() {
            None
        } else {
            self.state.as_ref()
        }
    }

    // Starts the helper through the administrator prompt, on a thread that
    // waits for it and then reads the rules again. Err is what to tell the
    // user when it could not even start.
    pub fn allow(
        &mut self,
        ctx: &Context,
        owner: Option<isize>,
        backlog: &mut Backlog,
    ) -> Result<(), Note> {
        let exe = std::env::current_exe().map_err(|err| could_not_prompt(err, backlog))?;
        let ctx = ctx.clone();
        let port = self.port;
        let (answer, prompt) = mpsc::sync_channel(1);
        thread::Builder::new()
            .name("firewall prompt".into())
            .spawn(move || {
                let ran = win::run_elevated(&exe, elevated::FLAG, owner);
                let after = matches!(ran, Elevated::Exited(_)).then(|| firewall::check(&exe, port));
                // Sent before the repaint is asked for, so the frame it
                // brings finds the answer waiting.
                let _ = answer.send(Prompted { ran, after });
                ctx.request_repaint();
            })
            .map_err(|err| could_not_prompt(err, backlog))?;
        backlog.add(String::from(
            "firewall: allow pressed, administrator prompt opened",
        ));
        self.prompt = Some(prompt);
        Ok(())
    }

    pub fn waiting(&self) -> bool {
        self.prompt.is_some()
    }

    // None while the prompt or the helper is still open. Then what the start
    // screen should say, which is nothing once Windows lets Booth in.
    pub fn prompt_ended(&mut self, backlog: &mut Backlog) -> Option<Option<Note>> {
        let prompted = match self.prompt.as_ref()?.try_recv() {
            Ok(prompted) => prompted,
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => Prompted {
                ran: Elevated::Failed(io::Error::other("the prompt thread stopped with a panic")),
                after: None,
            },
        };
        self.prompt = None;
        let Prompted { ran, after } = prompted;
        let note = match &ran {
            Elevated::Cancelled => {
                backlog.add(String::from("firewall: administrator prompt closed"));
                Some(Note::said(messages::FIREWALL_CANCELLED))
            }
            Elevated::Failed(err) => {
                backlog.add(format!("firewall: administrator step failed: {err}"));
                Some(Note::failed(messages::firewall_prompt_error(err)))
            }
            Elevated::Exited(code) => {
                backlog.add(format!("firewall: helper ended with code {code}"));
                (*code != 0).then(|| Note::failed(messages::firewall_exit(*code)))
            }
        };
        if let Some(state) = after {
            self.settle("after the prompt", state, backlog);
        }
        let kept_out = matches!(
            self.state,
            Some(
                FirewallState::Blocked(_)
                    | FirewallState::Missing(_)
                    | FirewallState::BlockingAll(_)
            )
        );
        if matches!(ran, Elevated::Exited(0)) && kept_out {
            let text = String::from(messages::FIREWALL_STILL_BLOCKED);
            return Some(Some(Note::failed(text)));
        }
        Some(note)
    }

    // The prompt, if it is still open, stays up and is answered on its own;
    // the log gets what came of it.
    pub fn not_now(&self, backlog: &mut Backlog) {
        if self.waiting() {
            backlog.add(String::from(
                "firewall: not now, with the administrator prompt still open",
            ));
        } else {
            backlog.add(String::from("firewall: not now"));
        }
    }

    pub fn state(&self) -> Option<&FirewallState> {
        self.state.as_ref()
    }

    fn settle(&mut self, when: &str, state: FirewallState, backlog: &mut Backlog) {
        backlog.add(format!("firewall {when}: {state}"));
        self.state = Some(state);
    }
}

fn could_not_prompt(err: io::Error, backlog: &mut Backlog) -> Note {
    backlog.add(format!(
        "firewall: could not start the firewall step as administrator: {err}"
    ));
    Note::failed(messages::firewall_prompt_error(&err))
}

fn look(port: u16) -> FirewallState {
    match std::env::current_exe() {
        Ok(exe) => firewall::check(&exe, port),
        Err(err) => FirewallState::Unknown(format!("could not find this exe's own path: {err}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use net::firewall::Profiles;

    fn quiet() -> Firewall {
        Firewall {
            port: 41000,
            check: None,
            prompt: None,
            state: None,
        }
    }

    const PUBLIC: Profiles = Profiles::PUBLIC;

    fn checking(thread: JoinHandle<(FirewallState, Duration)>, waited: Duration) -> Firewall {
        Firewall {
            check: Some(Check {
                thread,
                waited_since: Instant::now().checked_sub(waited),
            }),
            ..quiet()
        }
    }

    #[test]
    fn a_check_that_hangs_lets_the_panel_start() {
        let (release, hang) = mpsc::channel::<()>();
        let thread = thread::spawn(move || {
            let _ = hang.recv();
            (FirewallState::Missing(PUBLIC), Duration::ZERO)
        });
        let mut firewall = checking(thread, CHECK_LIMIT);
        let mut backlog = Backlog::default();
        let state = firewall.checked(&mut backlog).cloned();
        assert!(
            matches!(state, Some(FirewallState::Unknown(_))),
            "{state:?}"
        );
        assert_eq!(
            backlog.texts(),
            ["firewall at start: not known; windows firewall did not answer within 2 s"]
        );
        drop(release);
    }

    #[test]
    fn a_check_that_answers_in_time_is_taken() {
        let (release, hang) = mpsc::channel::<()>();
        let thread = thread::spawn(move || {
            let _ = hang.recv();
            (FirewallState::Missing(PUBLIC), Duration::ZERO)
        });
        let mut firewall = checking(thread, Duration::ZERO);
        let mut backlog = Backlog::default();
        assert_eq!(firewall.checked(&mut backlog), None);
        drop(release);
        let deadline = Instant::now() + Duration::from_secs(1);
        while firewall.checked(&mut backlog).is_none() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(firewall.state(), Some(&FirewallState::Missing(PUBLIC)));
    }

    fn prompted(ran: Elevated, after: Option<FirewallState>) -> Option<Option<Note>> {
        let (answer, prompt) = mpsc::sync_channel(1);
        let mut firewall = Firewall {
            prompt: Some(prompt),
            ..quiet()
        };
        answer.send(Prompted { ran, after }).unwrap();
        let ended = firewall.prompt_ended(&mut Backlog::default());
        assert!(!firewall.waiting());
        ended
    }

    fn said(ended: Option<Option<Note>>) -> Option<String> {
        ended
            .expect("the prompt ended")
            .map(|note| note.text().to_owned())
    }

    #[test]
    fn the_prompt_ends_in_one_sentence_or_none() {
        let allowed = Some(FirewallState::Allowed(PUBLIC));
        assert_eq!(said(prompted(Elevated::Exited(0), allowed)), None);
        let cancelled = said(prompted(Elevated::Cancelled, None));
        assert_eq!(cancelled.as_deref(), Some(messages::FIREWALL_CANCELLED));
        let refused = said(prompted(Elevated::Exited(22), None)).unwrap();
        assert!(refused.contains("did not let Booth"), "{refused}");
        let shut = Some(FirewallState::BlockingAll(PUBLIC));
        let still = said(prompted(Elevated::Exited(0), shut));
        assert_eq!(still.as_deref(), Some(messages::FIREWALL_STILL_BLOCKED));
    }

    #[test]
    fn nothing_ends_while_the_prompt_is_open() {
        let (_answer, prompt) = mpsc::sync_channel::<Prompted>(1);
        let mut firewall = Firewall {
            prompt: Some(prompt),
            ..quiet()
        };
        let mut backlog = Backlog::default();
        assert!(firewall.prompt_ended(&mut backlog).is_none());
        assert!(firewall.waiting());
        firewall.not_now(&mut backlog);
        assert_eq!(
            backlog.texts(),
            ["firewall: not now, with the administrator prompt still open"]
        );
    }

    #[test]
    fn a_prompt_thread_that_died_is_a_failure() {
        let (answer, prompt) = mpsc::sync_channel::<Prompted>(1);
        drop(answer);
        let mut firewall = Firewall {
            prompt: Some(prompt),
            ..quiet()
        };
        let text = said(firewall.prompt_ended(&mut Backlog::default())).unwrap();
        assert!(
            text.starts_with("Could not start the firewall step"),
            "{text}"
        );
    }
}
