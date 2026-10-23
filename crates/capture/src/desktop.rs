// Whether Windows shows this session's own desktop. Desktop Duplication
// refuses the secure desktop and a disconnected session, which a monitor's
// capture reports as a pause; Windows.Graphics.Capture goes on without a
// word, so a window share asks here.

use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::RemoteDesktop::{
    ProcessIdToSessionId, WTS_CONNECTSTATE_CLASS, WTS_CURRENT_SERVER_HANDLE, WTS_CURRENT_SESSION,
    WTSActive, WTSConnectState, WTSFreeMemory, WTSGetActiveConsoleSessionId,
    WTSQuerySessionInformationW,
};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, DESKTOP_CONTROL_FLAGS, DESKTOP_READOBJECTS, GetThreadDesktop,
    GetUserObjectInformationW, HDESK, OpenInputDesktop, UOI_NAME,
};
use windows::Win32::System::Threading::{GetCurrentProcessId, GetCurrentThreadId};
use windows::core::PWSTR;

use crate::PauseReason;

// Longer than any desktop name Windows gives; a longer one reads as another
// desktop.
const NAME: usize = 256;

// Outside the console session the session manager is asked at most this
// often: each question is a call into another process, and a window share
// looks with every picture and every move of the pointer.
const ASK_EVERY: Duration = Duration::from_millis(100);

// None while the desktop that has the screen and the keyboard is the calling
// thread's own and the session is connected; otherwise the pause a monitor's
// capture has for it, or Changing when Windows could not be asked whether
// the session is connected. Microseconds on this PC's own screen, with no
// call to another process.
pub fn desktop_away() -> Option<PauseReason> {
    #[cfg(feature = "fault")]
    if let Some((away, takes)) = crate::fault::desktop_said() {
        std::thread::sleep(takes);
        return away;
    }
    // The lock screen, a UAC prompt and Ctrl+Alt+Del are a desktop that
    // only the system may open.
    // SAFETY: asks for the least access there is; the handle is closed
    // below.
    let Ok(input) =
        (unsafe { OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_READOBJECTS) })
    else {
        return Some(PauseReason::SecureDesktop);
    };
    // A secure screen saver's desktop, or another one a program switched
    // to, can open but is not the one the shared window is on.
    // SAFETY: a plain query; a thread's own desktop handle is never closed.
    let ours =
        unsafe { GetThreadDesktop(GetCurrentThreadId()) }.is_ok_and(|own| same_name(input, own));
    // SAFETY: opened above, closed once, not used after.
    let _ = unsafe { CloseDesktop(input) };
    if !ours {
        return Some(PauseReason::SecureDesktop);
    }
    // Last, since outside the console session it is a call into another
    // process, which a lock should never wait behind.
    session_away()
}

// By name, since two handles to one desktop are two different numbers. Both
// are in this process's window station, where a name is one desktop.
fn same_name(a: HDESK, b: HDESK) -> bool {
    let (mut first, mut second) = ([0; NAME], [0; NAME]);
    match (name(a, &mut first), name(b, &mut second)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

fn name(desktop: HDESK, into: &mut [u16; NAME]) -> Option<&[u16]> {
    // SAFETY: `into` is writable for the bytes passed, and a longer name
    // fails the call instead of writing past them.
    unsafe {
        GetUserObjectInformationW(
            HANDLE(desktop.0),
            UOI_NAME,
            Some(into.as_mut_ptr().cast()),
            size_of_val(into) as u32,
            None,
        )
    }
    .ok()?;
    let len = into.iter().position(|&unit| unit == 0)?;
    Some(&into[..len])
}

// A Remote Desktop session left open, or a user switched away from, is
// disconnected, and OpenInputDesktop still opens the desktop it will show
// when it is back. The session on this PC's own screen is connected, which
// Windows keeps where any process reads it; any other session is asked
// about, a call into another process that takes about 35 us here, and the
// answer kept for ASK_EVERY. Only an answer that it is active counts: a
// question that fails keeps the PC sound off rather than sending what may
// be a PC left alone, and says why.
fn session_away() -> Option<PauseReason> {
    static SESSION: OnceLock<Option<u32>> = OnceLock::new();
    static ASKED: Mutex<Option<(Instant, Option<PauseReason>)>> = Mutex::new(None);
    // A process never changes session.
    let session = *SESSION.get_or_init(|| {
        let mut session = 0;
        // SAFETY: fills one live u32; a process may always ask about itself.
        unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) }
            .is_ok()
            .then_some(session)
    });
    // SAFETY: takes no arguments.
    if session.is_some_and(|session| session == unsafe { WTSGetActiveConsoleSessionId() }) {
        return None;
    }
    let mut asked = ASKED.lock().unwrap_or_else(PoisonError::into_inner);
    let now = Instant::now();
    if let Some((at, away)) = &*asked
        && now.saturating_duration_since(*at) < ASK_EVERY
    {
        return away.clone();
    }
    let away = match connect_state() {
        Ok(state) if state == WTSActive => None,
        Ok(_) => Some(PauseReason::Disconnected),
        Err(why) => Some(PauseReason::Changing(format!(
            "could not find out whether this session is connected: {why}"
        ))),
    };
    *asked = Some((now, away.clone()));
    away
}

fn connect_state() -> Result<WTS_CONNECTSTATE_CLASS, String> {
    let mut state = PWSTR::null();
    let mut bytes = 0;
    // SAFETY: asks about this process's own session on this PC; on success
    // Windows hands back a buffer of its own, freed below.
    let asked = unsafe {
        WTSQuerySessionInformationW(
            Some(WTS_CURRENT_SERVER_HANDLE),
            WTS_CURRENT_SESSION,
            WTSConnectState,
            &mut state,
            &mut bytes,
        )
    };
    // Not meaning(), whose words are for the capture's own calls: access
    // denied there means the secure desktop.
    if let Err(err) = asked {
        return Err(crate::error::windows_words(&err));
    }
    if state.is_null() {
        return Err(String::from("Windows gave no answer"));
    }
    let read = (bytes as usize >= size_of::<WTS_CONNECTSTATE_CLASS>())
        // SAFETY: the buffer holds the state, an INT, in the bytes it says.
        .then(|| unsafe { state.0.cast::<WTS_CONNECTSTATE_CLASS>().read_unaligned() });
    // SAFETY: freed once, with the call that goes with the query, after the
    // last read.
    unsafe { WTSFreeMemory(state.0.cast()) };
    read.ok_or_else(|| {
        format!(
            "Windows answered in {bytes} bytes, not {}",
            size_of::<WTS_CONNECTSTATE_CLASS>()
        )
    })
}

#[cfg(test)]
mod tests {
    use std::thread;

    use windows::Win32::System::Shutdown::LockWorkStation;

    use super::*;

    // As every test that captures this PC's screen, this one runs on its
    // desktop, unlocked. The runner, which has no monitor, skips it. The
    // session manager is asked as well, which a look skips for the session
    // on this PC's own screen. A window share looks with every picture and
    // every move of the pointer, some hundreds of times a second at most.
    #[test]
    fn this_desktop_is_the_users() {
        if crate::monitors().unwrap().is_empty() {
            println!("skipped: no monitor is attached to the desktop");
            return;
        }
        assert_eq!(connect_state(), Ok(WTSActive));
        assert_eq!(session_away(), None);
        let mut took: Vec<Duration> = (0..1000)
            .map(|_| {
                let start = Instant::now();
                assert_eq!(desktop_away(), None);
                start.elapsed()
            })
            .collect();
        took.sort();
        let (median, p99) = (took[500], took[990]);
        println!("a look at the desktop: median {median:?}, 99th {p99:?}");
        assert!(median < Duration::from_millis(1), "{median:?}");
    }

    // The lock screen is the secure desktop, which this PC can only be shown
    // for real: run by hand, by someone who will unlock it, with
    // cargo test -p capture --lib -- --ignored the_lock_screen_is_away
    #[test]
    #[ignore = "locks this PC"]
    fn the_lock_screen_is_away() {
        assert_eq!(
            desktop_away(),
            None,
            "the desktop is the user's to begin with"
        );
        // SAFETY: takes no arguments; the lock comes after it returns.
        unsafe { LockWorkStation() }.expect("lock this PC");
        let locked = Instant::now();
        while locked.elapsed() < Duration::from_secs(5) {
            if let Some(away) = desktop_away() {
                println!("away {:?} after the lock: {away}", locked.elapsed());
                assert_eq!(away, PauseReason::SecureDesktop);
                return;
            }
            thread::sleep(Duration::from_millis(1));
        }
        panic!("the desktop was still the user's 5 s after the lock");
    }
}
