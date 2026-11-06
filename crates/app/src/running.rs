// One Booth per data folder, and who else is Booth. Two copies on one
// profile would each write the known lists back from their own memory, so
// a device removed in one could come back with the other's next save. A
// second copy started on the same profile brings the first one's panel
// forward instead of opening one of its own.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::path::Path;
use std::ptr;
use std::thread;
use std::time::{Duration, Instant};

use net::Holder;
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::System::DataExchange::COPYDATASTRUCT;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, FindWindowExW, GetWindowThreadProcessId, PostMessageW,
    SMTO_ABORTIFHUNG, SMTO_ERRORONEXIT, SendMessageTimeoutW, WM_COPYDATA,
};

use crate::{links, tray};

// Held open as long as the panel is, and let go by Windows when the
// process ends, a crash included. Others may read it, not open it to write,
// which is what a second copy asks for. It holds the number of the process
// that has it, for that second copy to find.
pub const LOCK_FILE: &str = "booth.lock";
const FILE_SHARE_READ: u32 = 0x1;
const ERROR_SHARING_VIOLATION: i32 = 32;

// How long a second copy looks for the first one's panel. A first copy
// started a moment before, as when Booth is opened twice in a row, has its
// tray a second or two after it took the folder.
pub const WAIT: Duration = Duration::from_secs(3);
const POLL: Duration = Duration::from_millis(50);

pub enum Lock {
    Held(File),
    // Another copy has this folder, and its panel was asked to come forward.
    BroughtForward,
    // Another copy has this folder, and its panel was not asked for, or did
    // not answer within the wait.
    Open,
    // Not a sign of a second copy, and no reason not to start.
    Unheld(io::Error),
}

// Never BroughtForward: the other copy is left as it is.
pub fn hold(dir: &Path) -> Lock {
    match open(dir) {
        Ok(file) => Lock::Held(file),
        Err(err) if err.raw_os_error() == Some(ERROR_SHARING_VIOLATION) => Lock::Open,
        Err(err) => Lock::Unheld(err),
    }
}

pub fn take(dir: &Path, wait: Duration) -> Lock {
    let until = Instant::now() + wait;
    loop {
        match hold(dir) {
            Lock::Open => {}
            done => return done,
        }
        // This process's own number is no other copy: it is left from an
        // earlier process Windows gave the same number, or this process has
        // the folder already, which only tests do.
        let other = holder(dir).filter(|pid| *pid != std::process::id());
        if other.is_some_and(bring_forward) {
            return Lock::BroughtForward;
        }
        if Instant::now() >= until {
            return Lock::Open;
        }
        thread::sleep(POLL);
    }
}

pub enum HandOver {
    Held(File),
    Unheld(io::Error),
    Handed,
    InRoom,
    Older(u32),
    NoAnswer,
}

pub fn hand_over(dir: &Path, wait: Duration, link: &str) -> HandOver {
    let until = Instant::now() + wait;
    let mut sent = false;
    loop {
        match hold(dir) {
            Lock::Held(file) => return HandOver::Held(file),
            Lock::Unheld(err) => return HandOver::Unheld(err),
            Lock::Open => {}
            Lock::BroughtForward => unreachable!(),
        }
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return HandOver::NoAnswer;
        }
        if !sent
            && let Some(pid) = holder(dir).filter(|pid| *pid != std::process::id())
            && let Some(window) = tray_window(pid)
        {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return HandOver::NoAnswer;
            }
            sent = true;
            let data = COPYDATASTRUCT {
                dwData: links::MESSAGE_TAG,
                cbData: link.len() as u32,
                lpData: link.as_ptr().cast_mut().cast(),
            };
            let mut answer = 0;
            let timeout = left.as_millis().clamp(1, u128::from(u32::MAX)) as u32;
            // A timeout does not cancel delivery, so this message is never
            // sent again, even if the tray has not answered by the deadline.
            // SAFETY: data and its bytes outlive the synchronous send; Windows
            // copies them to the other process for the duration of its call.
            let ok = unsafe {
                AllowSetForegroundWindow(pid);
                SendMessageTimeoutW(
                    window,
                    WM_COPYDATA,
                    0,
                    &raw const data as isize,
                    SMTO_ABORTIFHUNG | SMTO_ERRORONEXIT,
                    timeout,
                    &mut answer,
                )
            };
            if ok != 0 {
                return match answer {
                    1 => HandOver::Handed,
                    2 => HandOver::InRoom,
                    _ => HandOver::Older(pid),
                };
            }
        }
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return HandOver::NoAnswer;
        }
        thread::sleep(POLL.min(left));
    }
}

fn open(dir: &Path) -> io::Result<File> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(FILE_SHARE_READ)
        .open(dir.join(LOCK_FILE))?;
    // Without the number a second copy only says this one is open.
    let _ = file
        .set_len(0)
        .and_then(|()| write!(file, "{}\r\n", std::process::id()));
    Ok(file)
}

// None while the copy that has it is still writing it, or for a copy from
// before the number was written.
fn holder(dir: &Path) -> Option<u32> {
    fs::read_to_string(dir.join(LOCK_FILE))
        .ok()?
        .trim()
        .parse()
        .ok()
}

// Through the tray window of the copy with that number, which shows its
// panel the way a click on the icon does. That copy may take the focus:
// this one may, since it was just started, and hands that on.
pub fn bring_forward(pid: u32) -> bool {
    let show = tray::show_message();
    if show == 0 {
        return false;
    }
    let Some(window) = tray_window(pid) else {
        return false;
    };
    // SAFETY: plain values; both fail harmlessly for a window or a
    // process that has just gone.
    unsafe {
        AllowSetForegroundWindow(pid);
        PostMessageW(window, show, 0, 0) != 0
    }
}

fn tray_window(pid: u32) -> Option<HWND> {
    let class: Vec<u16> = tray::CLASS.encode_utf16().chain([0]).collect();
    let mut after = ptr::null_mut();
    loop {
        // SAFETY: the class name is zero terminated and outlives the call;
        // `after` is null or a window this loop was just given.
        let window = unsafe { FindWindowExW(ptr::null_mut(), after, class.as_ptr(), ptr::null()) };
        if window.is_null() {
            return None;
        }
        let mut owner = 0u32;
        // SAFETY: `owner` is a live u32; a window gone since it was found
        // leaves it 0.
        unsafe { GetWindowThreadProcessId(window, &mut owner) };
        if owner == pid {
            return Some(window);
        }
        after = window;
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PortHolder {
    // This copy: something its last room left running still has the port.
    ThisCopy,
    // On another profile, since a copy on this one is stopped by the lock
    // before it binds anything.
    AnotherCopy,
    // By its file name.
    Program(String),
    // The port is free again, or Windows would not name who had it.
    Unknown,
}

pub fn port_holder(holder: Option<&Holder>, this_pid: u32, this_exe: Option<&Path>) -> PortHolder {
    let Some(holder) = holder else {
        return PortHolder::Unknown;
    };
    if holder.pid == this_pid {
        return PortHolder::ThisCopy;
    }
    let Some(name) = holder.exe.as_deref().and_then(Path::file_name) else {
        return PortHolder::Unknown;
    };
    let name = name.to_string_lossy();
    // Another copy of Booth may be another version in another folder, but
    // it is still called booth.exe, or whatever this one is called.
    let booth = name.eq_ignore_ascii_case("booth.exe")
        || this_exe
            .and_then(Path::file_name)
            .is_some_and(|this| this.to_string_lossy().eq_ignore_ascii_case(&name));
    if booth {
        PortHolder::AnotherCopy
    } else {
        PortHolder::Program(name.into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read};
    use std::net::{Ipv4Addr, UdpSocket};
    use std::path::PathBuf;
    use std::process::{Command, Stdio};

    use room::RoomError;

    use crate::messages;

    fn folder(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("booth-app-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn second_copy_turned_away() {
        let dir = folder("held");
        let other = dir.join("other profile");
        fs::create_dir_all(&other).unwrap();
        let Lock::Held(first) = take(&dir, Duration::ZERO) else {
            panic!("the first copy holds it");
        };
        // No tray of this test process answers, so it is open, and said so.
        let started = Instant::now();
        assert!(matches!(take(&dir, Duration::ZERO), Lock::Open));
        assert!(started.elapsed() < Duration::from_secs(1));
        // A backup tool or a virus scanner reading it is no second copy.
        let number = fs::read_to_string(dir.join(LOCK_FILE)).expect("others may read it");
        assert_eq!(number, format!("{}\r\n", std::process::id()));
        assert_eq!(holder(&dir), Some(std::process::id()));
        // Another profile is another folder.
        let Lock::Held(beside) = take(&other, Duration::ZERO) else {
            panic!("another profile starts");
        };
        drop(first);
        let Lock::Held(again) = take(&dir, Duration::ZERO) else {
            panic!("free once the first copy is gone");
        };
        drop((again, beside));
        let _ = fs::remove_dir_all(&dir);
    }

    // The first copy closes while the second waits for it, and the second
    // starts as if it had been alone.
    #[test]
    fn folder_handed_over_on_close() {
        let dir = folder("closing");
        let Lock::Held(first) = take(&dir, Duration::ZERO) else {
            panic!("the first copy holds it");
        };
        let closing = thread::spawn(move || {
            thread::sleep(Duration::from_millis(300));
            drop(first);
        });
        let started = Instant::now();
        let second = take(&dir, Duration::from_secs(5));
        let took = started.elapsed();
        closing.join().unwrap();
        assert!(matches!(second, Lock::Held(_)), "the second copy holds it");
        assert!(took >= Duration::from_millis(250), "{took:?}");
        assert!(took < Duration::from_secs(2), "{took:?}");
        drop(second);
        let _ = fs::remove_dir_all(&dir);
    }

    const FOLDER_VAR: &str = "BOOTH_TEST_OPEN_ON";
    const OPEN: &str = "open on the folder";
    const ASKED: &str = "the panel was asked for";
    const LINK_MODE: &str = "BOOTH_TEST_LINK_MODE";
    const LINK_TEXT: &str = "booth://join/booth1-test";
    const LINK_SEEN: &str = "the invite link arrived";

    fn with_link_holder(mode: &str, check: impl FnOnce(&Path, u32)) {
        let dir = folder(&format!("link-{mode}"));
        let mut first = Command::new(std::env::current_exe().unwrap())
            .args([
                "running::tests::hold_for_an_invite_link",
                "--exact",
                "--ignored",
                "--nocapture",
            ])
            .env(FOLDER_VAR, &dir)
            .env(LINK_MODE, mode)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut lines = BufReader::new(first.stdout.take().unwrap()).lines();
        assert!(
            lines
                .by_ref()
                .map_while(Result::ok)
                .any(|line| line.contains(OPEN))
        );
        let checked =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| check(&dir, first.id())));
        drop(first.stdin.take());
        let rest: Vec<_> = lines.map_while(Result::ok).collect();
        let ended = first.wait().unwrap();
        let _ = fs::remove_dir_all(&dir);
        if let Err(payload) = checked {
            std::panic::resume_unwind(payload);
        }
        assert!(ended.success());
        if matches!(mode, "taken" | "room" | "older" | "hung") {
            assert_eq!(
                rest.iter().filter(|line| line.contains(LINK_SEEN)).count(),
                1,
                "{rest:?}"
            );
        }
        if mode == "gone" {
            assert!(
                rest.iter().all(|line| !line.contains(LINK_SEEN)),
                "{rest:?}"
            );
        }
    }

    #[test]
    fn link_hand_over_sends_the_bytes_once() {
        with_link_holder("taken", |dir, _| {
            let at = Instant::now();
            assert!(matches!(
                hand_over(dir, Duration::from_secs(2), LINK_TEXT),
                HandOver::Handed
            ));
            let took = at.elapsed();
            println!("invite link handed over in {took:?}");
            assert!(took < Duration::from_secs(1), "{took:?}");
        });
    }

    #[test]
    fn link_hand_over_in_a_room() {
        with_link_holder("room", |dir, _| {
            assert!(matches!(
                hand_over(dir, Duration::from_secs(1), LINK_TEXT),
                HandOver::InRoom
            ));
        });
    }

    #[test]
    fn link_hand_over_to_an_older_copy() {
        with_link_holder("older", |dir, pid| {
            assert!(
                matches!(hand_over(dir, Duration::from_secs(1), LINK_TEXT), HandOver::Older(other) if other == pid)
            );
        });
    }

    #[test]
    fn link_hand_over_without_a_tray_waits() {
        with_link_holder("none", |dir, _| {
            let at = Instant::now();
            let wait = Duration::from_millis(200);
            assert!(matches!(
                hand_over(dir, wait, LINK_TEXT),
                HandOver::NoAnswer
            ));
            assert!(at.elapsed() >= wait);
            assert!(at.elapsed() < Duration::from_secs(1));
        });
    }

    #[test]
    fn link_hand_over_gives_up_on_a_hung_tray() {
        with_link_holder("hung", |dir, _| {
            let at = Instant::now();
            let wait = Duration::from_millis(100);
            assert!(matches!(
                hand_over(dir, wait, LINK_TEXT),
                HandOver::NoAnswer
            ));
            assert!(at.elapsed() >= wait);
            assert!(at.elapsed() < Duration::from_secs(1));
        });
    }

    // The first tray window goes while the message waits on it, and another
    // comes well before the deadline: the failed send is not made again.
    #[test]
    fn link_hand_over_failed_send_is_not_sent_again() {
        with_link_holder("gone", |dir, _| {
            let at = Instant::now();
            let wait = Duration::from_secs(1);
            assert!(matches!(
                hand_over(dir, wait, LINK_TEXT),
                HandOver::NoAnswer
            ));
            assert!(at.elapsed() >= wait);
        });
    }

    #[test]
    fn link_hand_over_takes_a_closing_folder() {
        with_link_holder("closing", |dir, _| {
            let at = Instant::now();
            let taken = hand_over(dir, Duration::from_secs(2), LINK_TEXT);
            assert!(matches!(taken, HandOver::Held(_)));
            assert!(at.elapsed() >= Duration::from_millis(250));
            assert!(at.elapsed() < Duration::from_secs(1));
        });
    }

    #[test]
    #[ignore = "the holder in the invite link tests"]
    fn hold_for_an_invite_link() {
        use windows_sys::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
        use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows_sys::Win32::System::Threading::GetCurrentThreadId;
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, GetQueueStatus, MSG,
            PostThreadMessageW, QS_SENDMESSAGE, RegisterClassW, WM_QUIT, WNDCLASSW,
        };

        unsafe extern "system" fn older(
            hwnd: HWND,
            message: u32,
            wparam: WPARAM,
            lparam: LPARAM,
        ) -> LRESULT {
            if message == WM_COPYDATA {
                println!("{LINK_SEEN}");
            }
            // SAFETY: these are the arguments Windows gave this window.
            unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
        }

        let Some(dir) = std::env::var_os(FOLDER_VAR) else {
            return;
        };
        let mode = std::env::var(LINK_MODE).unwrap();
        let Lock::Held(held) = hold(Path::new(&dir)) else {
            panic!("the holder takes the folder");
        };
        if mode == "closing" {
            let _tray = tray::Tray::start(
                || {},
                |bytes| {
                    assert_eq!(bytes, LINK_TEXT.as_bytes());
                    thread::sleep(Duration::from_millis(300));
                    std::process::exit(0);
                },
            )
            .unwrap();
            println!("{OPEN}");
            let _ = std::io::stdin().read(&mut [0; 1]);
            return;
        }
        if mode == "older" || mode == "gone" {
            let class: Vec<u16> = tray::CLASS.encode_utf16().chain([0]).collect();
            // SAFETY: the class and its procedure outlive this thread's message loop.
            unsafe {
                let wc = WNDCLASSW {
                    lpfnWndProc: Some(older),
                    hInstance: GetModuleHandleW(ptr::null()),
                    lpszClassName: class.as_ptr(),
                    ..WNDCLASSW::default()
                };
                assert_ne!(RegisterClassW(&wc), 0);
                let window = |class: *const u16| {
                    CreateWindowExW(
                        0,
                        class,
                        ptr::null(),
                        0,
                        0,
                        0,
                        0,
                        0,
                        ptr::null_mut(),
                        ptr::null_mut(),
                        GetModuleHandleW(ptr::null()),
                        ptr::null(),
                    )
                };
                if mode == "gone" {
                    // The first window is on a thread that never takes its
                    // messages and goes with that thread once the link run's
                    // message waits there, so that send fails at once.
                    let (made, ready) = std::sync::mpsc::channel();
                    let first_class = class.clone();
                    let first = thread::spawn(move || {
                        assert!(!window(first_class.as_ptr()).is_null());
                        made.send(()).unwrap();
                        let until = Instant::now() + Duration::from_secs(5);
                        while (GetQueueStatus(QS_SENDMESSAGE) >> 16) & QS_SENDMESSAGE == 0
                            && Instant::now() < until
                        {
                            thread::sleep(Duration::from_millis(1));
                        }
                    });
                    ready.recv().unwrap();
                    println!("{OPEN}");
                    first.join().unwrap();
                }
                assert!(!window(class.as_ptr()).is_null());
                let tid = GetCurrentThreadId();
                thread::spawn(move || {
                    let _ = std::io::stdin().read(&mut [0; 1]);
                    PostThreadMessageW(tid, WM_QUIT, 0, 0);
                });
                if mode == "older" {
                    println!("{OPEN}");
                }
                let mut msg = MSG::default();
                while GetMessageW(&mut msg, ptr::null_mut(), 0, 0) > 0 {
                    DispatchMessageW(&msg);
                }
            }
            drop(held);
            return;
        }
        let tray = if mode == "none" {
            None
        } else {
            Some(
                tray::Tray::start(
                    || {},
                    move |bytes| {
                        assert_eq!(bytes, LINK_TEXT.as_bytes());
                        if mode == "hung" {
                            thread::sleep(Duration::from_millis(300));
                        }
                        println!("{LINK_SEEN}");
                        if mode == "room" { 2 } else { 1 }
                    },
                )
                .unwrap(),
            )
        };
        println!("{OPEN}");
        let _ = std::io::stdin().read(&mut [0; 1]);
        drop(tray);
        drop(held);
    }

    // The first copy is a second process of this test program, with the
    // folder and a tray of its own, so the number in the lock file, the
    // search for its window and the ask all cross processes as they do
    // between two copies of Booth.
    #[test]
    fn second_copy_brings_first_forward() {
        let dir = folder("forward");
        let mut first = Command::new(std::env::current_exe().unwrap())
            .args([
                "running::tests::stay_open_until_stdin_closes",
                "--exact",
                "--ignored",
                "--nocapture",
            ])
            .env(FOLDER_VAR, &dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("start a first copy of the test program");
        let mut lines = BufReader::new(first.stdout.take().unwrap()).lines();
        let open = lines
            .by_ref()
            .map_while(Result::ok)
            .any(|line| line.contains(OPEN));
        assert!(open, "the first copy did not take the folder");
        assert_eq!(holder(&dir), Some(first.id()));

        // Started with options for its own run, the second copy is only
        // told, and the first one's panel stays where it is.
        assert!(matches!(hold(&dir), Lock::Open));

        let asked = Instant::now();
        let second = take(&dir, Duration::from_secs(5));
        let forward = matches!(second, Lock::BroughtForward);
        let shown = forward
            && lines
                .by_ref()
                .map_while(Result::ok)
                .any(|line| line.contains(ASKED));
        let took = asked.elapsed();
        drop(first.stdin.take());
        let rest: Vec<String> = lines.map_while(Result::ok).collect();
        let _ = first.wait();
        let _ = fs::remove_dir_all(&dir);
        assert!(forward, "the second copy was not handed to the first");
        assert!(shown, "the first copy's panel was not asked for");
        println!("the first copy's panel was asked for, across processes, in {took:?}");
        assert!(took < Duration::from_secs(1), "{took:?}");
        // Once: the hold above asked for nothing.
        assert!(!rest.iter().any(|line| line.contains(ASKED)), "{rest:?}");
    }

    #[test]
    #[ignore = "the first copy of second_copy_brings_first_forward"]
    fn stay_open_until_stdin_closes() {
        let Some(dir) = std::env::var_os(FOLDER_VAR) else {
            return;
        };
        let Lock::Held(held) = hold(Path::new(&dir)) else {
            panic!("the first copy could not take the folder");
        };
        let tray = tray::Tray::start(|| println!("{ASKED}"), |_| 0).expect("the tray starts");
        println!("{OPEN}");
        let _ = std::io::stdin().read(&mut [0u8; 1]);
        drop(tray);
        drop(held);
    }

    #[test]
    fn a_lock_file_from_an_older_copy_names_nobody() {
        let dir = folder("older");
        fs::write(dir.join(LOCK_FILE), b"").unwrap();
        assert_eq!(holder(&dir), None);
        fs::write(dir.join(LOCK_FILE), b"12\r").unwrap();
        assert_eq!(holder(&dir), Some(12));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_panel_answers_for_a_process_without_one() {
        // Process 0 is the idle process, which has no windows.
        assert!(!bring_forward(0));
    }

    fn held_by(pid: u32, exe: &str) -> Holder {
        Holder {
            pid,
            exe: Some(PathBuf::from(exe)),
        }
    }

    #[test]
    fn who_holds_the_port_decides_what_the_panel_says() {
        let this_exe = Path::new(r"C:\Users\a\Desktop\booth-0.1.0-windows-x64\booth.exe");
        let who = |holder: Option<&Holder>| port_holder(holder, 1000, Some(this_exe));
        assert_eq!(who(None), PortHolder::Unknown);
        assert_eq!(
            who(Some(&held_by(1000, this_exe.to_str().unwrap()))),
            PortHolder::ThisCopy
        );
        assert_eq!(
            who(Some(&held_by(2000, this_exe.to_str().unwrap()))),
            PortHolder::AnotherCopy
        );
        // Another version, from another folder.
        assert_eq!(
            who(Some(&held_by(2000, r"D:\Games\Booth\BOOTH.EXE"))),
            PortHolder::AnotherCopy
        );
        assert_eq!(
            who(Some(&held_by(2000, r"C:\Program Files\Steam\steam.exe"))),
            PortHolder::Program(String::from("steam.exe"))
        );
        assert_eq!(
            who(Some(&Holder { pid: 4, exe: None })),
            PortHolder::Unknown
        );
        // A renamed copy is known by this one's own name.
        let renamed = Path::new(r"C:\tools\booth-test.exe");
        assert_eq!(
            port_holder(
                Some(&held_by(2000, r"C:\other\booth-test.exe")),
                1000,
                Some(renamed)
            ),
            PortHolder::AnotherCopy
        );
    }

    const PORT_VAR: &str = "BOOTH_TEST_HOLD_UDP_PORT";
    const HOLDING: &str = "holding the udp port";

    // A free port, held by this process while `f` runs, or by a second
    // process of this same test program, as a second copy of Booth would.
    fn with_port_held(by_another: bool, f: impl FnOnce(u16)) {
        let free = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = free.local_addr().unwrap().port();
        if !by_another {
            f(port);
            return;
        }
        drop(free);
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "running::tests::hold_a_port_until_stdin_closes",
                "--exact",
                "--ignored",
                "--nocapture",
            ])
            .env(PORT_VAR, port.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("start a second copy of the test program");
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let holding = lines
            .by_ref()
            .map_while(Result::ok)
            .any(|line| line.contains(HOLDING));
        assert!(holding, "the second process did not take port {port}");
        f(port);
        drop(child.stdin.take());
        lines.for_each(drop);
        let _ = child.wait();
    }

    #[test]
    #[ignore = "the second process of the tests that bind a port held elsewhere"]
    fn hold_a_port_until_stdin_closes() {
        let Ok(port) = std::env::var(PORT_VAR) else {
            return;
        };
        let held = UdpSocket::bind((Ipv4Addr::LOCALHOST, port.parse().unwrap())).unwrap();
        println!("{HOLDING} {port}");
        let _ = std::io::stdin().read(&mut [0u8; 1]);
        drop(held);
    }

    // What a room's bind gives the panel, from the real failure.
    fn host_error(port: u16) -> String {
        let err = net::Socket::bind(port).expect_err("the port is held");
        messages::room_error(&RoomError::Bind(err))
    }

    #[test]
    fn port_held_by_this_copy() {
        with_port_held(false, |port| {
            assert_eq!(
                host_error(port),
                messages::in_use(port, &PortHolder::ThisCopy)
            );
        });
    }

    #[test]
    fn port_held_by_another_copy() {
        with_port_held(true, |port| {
            assert_eq!(
                host_error(port),
                messages::in_use(port, &PortHolder::AnotherCopy)
            );
        });
    }
}
