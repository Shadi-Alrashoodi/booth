// Every call into Windows the hotkeys make. The hotkey thread owns a hidden
// window that Raw Input delivers the keyboard to, and the mouse while remote
// control needs it, a foreground hook, and while the viewer controls
// fullscreen a low-level keyboard hook; its message loop turns what arrives
// into calls on hotkeys::Loop.
//
// A message-only window (HWND_MESSAGE) would be the obvious owner, but
// Microsoft documents only that one sends and receives messages, and says
// nothing about it getting background Raw Input with RIDEV_INPUTSINK. So this
// is a plain top-level window that is never shown or activated: a tool
// window, so it never gets a taskbar button either.

use std::cell::RefCell;
use std::io;
use std::ptr;
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_CLASS_ALREADY_EXISTS, HANDLE, HWND, LPARAM, LRESULT,
    WPARAM,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::RemoteDesktop::{
    ProcessIdToSessionId, WTS_CURRENT_SERVER_HANDLE, WTS_PROCESS_INFOW, WTSEnumerateProcessesW,
    WTSFreeMemory,
};
use windows_sys::Win32::System::StationsAndDesktops::{
    CloseDesktop, DESKTOP_READOBJECTS, OpenInputDesktop,
};
use windows_sys::Win32::System::SystemInformation::GetTickCount;
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessId, GetCurrentThread, GetCurrentThreadId, OpenProcess,
    OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION, SetThreadPriority,
    THREAD_PRIORITY_HIGHEST,
};
use windows_sys::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use windows_sys::Win32::UI::Input::{
    GetRawInputData, GetRegisteredRawInputDevices, HRAWINPUT, RAWINPUT, RAWINPUTDEVICE,
    RAWINPUTHEADER, RID_INPUT, RIDEV_DEVNOTIFY, RIDEV_INPUTSINK, RIDEV_REMOVE, RIM_TYPEKEYBOARD,
    RIM_TYPEMOUSE, RegisterRawInputDevices,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    EVENT_SYSTEM_FOREGROUND, GIDC_REMOVAL, GetForegroundWindow, GetMessageW,
    GetWindowThreadProcessId, HC_ACTION, HHOOK, IsWindow, KBDLLHOOKSTRUCT, KillTimer,
    LLKHF_EXTENDED, LLKHF_INJECTED, LLKHF_UP, MSG, PostMessageW, PostThreadMessageW,
    RegisterClassExW, SetTimer, SetWindowsHookExW, UnhookWindowsHookEx, WH_KEYBOARD_LL,
    WINEVENT_OUTOFCONTEXT, WM_APP, WM_INPUT, WM_INPUT_DEVICE_CHANGE, WM_QUIT, WM_TIMER,
    WNDCLASSEXW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP,
};

use crate::feed::{Captured, RawMouse, Sink};
use crate::grab::{Grab, from_hook};
use crate::hotkeys::Loop;
use crate::remote::Shared;
use crate::tracker::{Elevation, RawKey};

// HID generic desktop page, keyboard and mouse.
const USAGE_PAGE_GENERIC: u16 = 0x01;
const USAGE_KEYBOARD: u16 = 0x06;
const USAGE_MOUSE: u16 = 0x02;
// Posted to the window when commands wait in the channel.
const WM_COMMANDS: u32 = WM_APP;
// Posted to the thread by the foreground hook, with the time of the change.
const WM_FOREGROUND: u32 = WM_APP + 1;
// Posted to the window when a keyboard went away, with its device handle.
const WM_DEVICE_GONE: u32 = WM_APP + 2;
// Posted to the window when a remote control switch changed.
const WM_SWITCHES: u32 = WM_APP + 3;
// Input that arrives even while another window has focus. Never in this
// crate's own tests: the hidden window is never in front, so without the
// flag it gets no key or mouse event at all, and a test never reads what
// someone types while it runs.
const BACKGROUND: u32 = if cfg!(test) { 0 } else { RIDEV_INPUTSINK };
// Background keys, and a word when a keyboard goes, so what it held can be
// let go.
const LISTEN: u32 = BACKGROUND | RIDEV_DEVNOTIFY;
const WATCH_TIMER: usize = 1;
// Half the one second a stuck push to talk may last, so a late timer still
// makes it.
const WATCH_EVERY_MS: u32 = 500;
// Far more than a process registers; the panel has the mouse and this.
const MOST_REGISTRATIONS: usize = 16;

pub(crate) struct Thread {
    // An HWND is a pointer, which is not Send; it is only handed back to
    // Windows.
    window: isize,
    id: u32,
    join: Option<JoinHandle<()>>,
}

impl Thread {
    pub(crate) fn spawn(mut run: Loop) -> io::Result<Thread> {
        let (ready, answer) = mpsc::sync_channel(1);
        let join = thread::Builder::new()
            .name(String::from("hotkeys"))
            .spawn(move || {
                // This thread answers push to talk and the panic key, and
                // while the Windows keys hook is in, every key on this PC
                // waits for it. A game can keep every core busy, and a
                // thread at normal priority then waits for a time slice
                // after its message arrives: the room's timer thread woke 6
                // to 24 ms late at the 99th percentile that way, which alone
                // is past the panic key's 10 ms, and late enough answers
                // make Windows take the hook away. The thread sleeps in
                // GetMessageW between events and works for microseconds on
                // each, so going ahead of the game takes almost nothing from
                // it. Failing to raise it costs only that margin.
                let _ = raise_priority();
                let window = match Window::open(Arc::clone(run.shared())) {
                    Ok(window) => window,
                    Err(err) => {
                        let _ = ready.send(Err(err));
                        return;
                    }
                };
                // SAFETY: takes no arguments and cannot fail.
                let id = unsafe { GetCurrentThreadId() };
                let _ = ready.send(Ok((window.hwnd as isize, id)));
                let window = window.run(&mut run);
                // The command channel closes before the window goes, so a
                // command that went through always finds the window there.
                drop(run);
                drop(window);
            })
            .map_err(|err| {
                io::Error::new(
                    err.kind(),
                    format!("could not start the hotkey thread: {err}"),
                )
            })?;
        match answer.recv() {
            Ok(Ok((window, id))) => Ok(Thread {
                window,
                id,
                join: Some(join),
            }),
            Ok(Err(err)) => {
                let _ = join.join();
                Err(err)
            }
            Err(_) => {
                let _ = join.join();
                Err(io::Error::other(
                    "the hotkey thread ended before its window was made",
                ))
            }
        }
    }

    pub(crate) fn wake(&self) {
        // SAFETY: posting copies three integers into the window's queue. The
        // caller only posts after a command went into the channel, and the
        // thread closes the channel before it destroys the window.
        unsafe { PostMessageW(self.window as HWND, WM_COMMANDS, 0, 0) };
    }

    // Windows reports RIDEV_DEVNOTIFY back as unset whatever was registered,
    // so only the flag that brings keys from behind other windows is looked
    // at.
    pub(crate) fn listening(&self) -> bool {
        registered_to(self.window as HWND)
            .is_some_and(|flags| flags & RIDEV_INPUTSINK == BACKGROUND)
    }
}

impl Drop for Thread {
    fn drop(&mut self) {
        let Some(join) = self.join.take() else {
            return;
        };
        // A thread that already ended (it panicked) no longer owns its id,
        // which Windows may have given to another thread since.
        if !join.is_finished() {
            // SAFETY: posts WM_QUIT with no data to the hotkey thread, which
            // is still running and has a message queue since its window was
            // made before `spawn` returned.
            unsafe { PostThreadMessageW(self.id, WM_QUIT, 0, 0) };
        }
        let _ = join.join();
    }
}

// Lives on the hotkey thread only.
struct Window {
    hwnd: HWND,
    hook: HWINEVENTHOOK,
    timer: bool,
    shared: Arc<Shared>,
    // Some while this window holds the process's mouse registration, with
    // what was registered before, to put back.
    mouse: Option<Option<RAWINPUTDEVICE>>,
    // Windows said no; asked again once the switch has been off.
    mouse_refused: bool,
    keys_hook: HHOOK,
    keys_refused: bool,
}

thread_local! {
    // The Windows keys hook's state. Windows calls a low-level hook on the
    // thread that set it, inside GetMessageW, so only the hotkey thread ever
    // touches this.
    static KEYS: RefCell<Option<Hooked>> = const { RefCell::new(None) };
}

struct Hooked {
    grab: Grab,
    sink: Sink,
}

impl Hooked {
    // A key taken while the viewer is not in front, a release or repeat of
    // one taken before, stays away from Windows and goes nowhere.
    fn event(&mut self, event: &KBDLLHOOKSTRUCT) -> bool {
        let key = from_hook(
            event.vkCode,
            event.scanCode,
            event.flags & LLKHF_EXTENDED != 0,
        );
        let down = event.flags & LLKHF_UP == 0;
        let injected = event.flags & LLKHF_INJECTED != 0;
        let open = self.sink.windows_keys();
        let take = self.grab.hook(key, down, injected, open.is_some());
        if take
            && let Some(key) = key
            && let Some(period) = open
        {
            let at = Instant::now();
            self.sink.send(Captured::Key { key, down, at }, period);
        }
        take
    }
}

impl Window {
    fn open(shared: Arc<Shared>) -> io::Result<Window> {
        let class: Vec<u16> = "BoothHotkeys\0".encode_utf16().collect();
        // SAFETY: a null name asks for this exe's own module handle, which
        // needs no closing.
        let instance = unsafe { GetModuleHandleW(ptr::null()) };
        let info = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            lpszClassName: class.as_ptr(),
            ..WNDCLASSEXW::default()
        };
        // SAFETY: `info` carries its own size, and the class name is zero
        // terminated and outlives the call, which copies it.
        if unsafe { RegisterClassExW(&info) } == 0 {
            let err = io::Error::last_os_error();
            // The class stays for the life of the process once made, so a
            // second start finds it there.
            if err.raw_os_error() != Some(ERROR_CLASS_ALREADY_EXISTS as i32) {
                return Err(context("could not register the hotkey window class", err));
            }
        }
        // SAFETY: the class was registered above with this module, the name
        // is zero terminated, and every other pointer is null, which
        // CreateWindowExW allows for a top-level window without a title.
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                class.as_ptr(),
                ptr::null(),
                WS_POPUP,
                0,
                0,
                0,
                0,
                ptr::null_mut(),
                ptr::null_mut(),
                instance,
                ptr::null(),
            )
        };
        if hwnd.is_null() {
            return Err(context(
                "could not make the hotkey window",
                io::Error::last_os_error(),
            ));
        }
        let mut window = Window {
            hwnd,
            hook: ptr::null_mut(),
            timer: false,
            shared,
            mouse: None,
            mouse_refused: false,
            keys_hook: ptr::null_mut(),
            keys_refused: false,
        };
        // The window it names was made on this thread and lives until Drop.
        let device = RAWINPUTDEVICE {
            usUsagePage: USAGE_PAGE_GENERIC,
            usUsage: USAGE_KEYBOARD,
            dwFlags: LISTEN,
            hwndTarget: hwnd,
        };
        if !register(&device) {
            return Err(context(
                "could not register for keyboard raw input",
                io::Error::last_os_error(),
            ));
        }
        // SAFETY: an out-of-context hook needs no module; the callback has
        // the WINEVENTPROC signature and runs on this thread, inside
        // GetMessageW, until UnhookWinEvent in Drop.
        window.hook = unsafe {
            SetWinEventHook(
                EVENT_SYSTEM_FOREGROUND,
                EVENT_SYSTEM_FOREGROUND,
                ptr::null_mut(),
                Some(foreground_changed),
                0,
                0,
                WINEVENT_OUTOFCONTEXT,
            )
        };
        if window.hook.is_null() {
            return Err(context(
                "could not watch which window is in front",
                io::Error::last_os_error(),
            ));
        }
        window.shared.set_window(Some(hwnd as isize));
        Ok(window)
    }

    fn run(mut self, run: &mut Loop) -> Window {
        // SAFETY: takes no arguments; the same clock Windows stamps messages
        // with.
        let now = unsafe { GetTickCount() };
        run.front(foreground_elevation(), now);
        self.follow_watch(run);
        self.follow_switches(run);
        let mut msg = MSG::default();
        loop {
            // SAFETY: `msg` is a live MSG for the call to fill; a null window
            // takes this thread's messages and its thread messages both.
            let got = unsafe { GetMessageW(&mut msg, ptr::null_mut(), 0, 0) };
            // 0 is WM_QUIT. -1 is an error, which with these arguments cannot
            // happen and would repeat forever if it did.
            if got == 0 || got == -1 {
                break;
            }
            // WM_INPUT still goes through the default handling after the
            // read: it frees what Windows kept for the event.
            let mut dispatch = true;
            match msg.message {
                WM_INPUT => match read_raw(msg.lParam as HRAWINPUT) {
                    Some(Raw::Key(raw)) => {
                        let hook_took = self.hook_took(raw);
                        run.raw(raw, msg.time, hook_took);
                        self.check_hook();
                    }
                    Some(Raw::Mouse(raw)) => run.mouse(raw),
                    None => {}
                },
                WM_SWITCHES if msg.hwnd == self.hwnd => dispatch = false,
                WM_FOREGROUND => {
                    run.front(foreground_elevation(), msg.wParam as u32);
                    dispatch = false;
                }
                WM_TIMER if msg.hwnd == self.hwnd && msg.wParam == WATCH_TIMER => {
                    let front = input_desktop_open().then(foreground_elevation);
                    run.watch(front, msg.time);
                    dispatch = false;
                }
                WM_DEVICE_GONE if msg.hwnd == self.hwnd => {
                    run.device_gone(msg.lParam as usize);
                    dispatch = false;
                }
                WM_COMMANDS => {
                    run.commands();
                    dispatch = false;
                }
                _ => {}
            }
            if dispatch {
                // SAFETY: `msg` came from GetMessageW on this thread.
                unsafe { DispatchMessageW(&msg) };
            }
            self.follow_watch(run);
            self.follow_switches(run);
        }
        self
    }

    // A few atomic reads after every message; Windows is asked for
    // something only when a switch changed.
    fn follow_switches(&mut self, run: &mut Loop) {
        let (mouse, keys) = run.wants();
        if !mouse {
            self.mouse_refused = false;
            self.give_mouse_back();
        } else if self.mouse.is_none() && !self.mouse_refused {
            self.take_mouse();
        }
        if !keys {
            self.keys_refused = false;
            self.unhook_keys();
        } else if self.keys_hook.is_null() && !self.keys_refused {
            self.hook_keys(run);
        }
    }

    // The panel's toolkit holds the mouse registration otherwise, for
    // events the panel does not use.
    fn take_mouse(&mut self) {
        let before = registration(USAGE_MOUSE);
        let device = RAWINPUTDEVICE {
            usUsagePage: USAGE_PAGE_GENERIC,
            usUsage: USAGE_MOUSE,
            dwFlags: BACKGROUND,
            hwndTarget: self.hwnd,
        };
        if register(&device) {
            self.mouse = Some(before);
            self.shared.set_mouse(true);
        } else {
            self.mouse_refused = true;
        }
    }

    // Only while the registration is still this window's: one made later
    // elsewhere in the process stays. What was there before goes back if
    // its window still exists. Windows never reports RIDEV_DEVNOTIFY back,
    // so a registration that had it comes back without.
    fn give_mouse_back(&mut self) {
        let Some(before) = self.mouse.take() else {
            return;
        };
        self.shared.set_mouse(false);
        if registration(USAGE_MOUSE).is_none_or(|now| now.hwndTarget != self.hwnd) {
            return;
        }
        let back = before.filter(|device| {
            // SAFETY: IsWindow takes any value and only answers.
            device.hwndTarget.is_null() || unsafe { IsWindow(device.hwndTarget) } != 0
        });
        if back.is_some_and(|device| register(&device)) {
            return;
        }
        register(&RAWINPUTDEVICE {
            usUsagePage: USAGE_PAGE_GENERIC,
            usUsage: USAGE_MOUSE,
            dwFlags: RIDEV_REMOVE,
            hwndTarget: ptr::null_mut(),
        });
    }

    fn hook_keys(&mut self, run: &Loop) {
        let (grab, sink) = run.grab();
        KEYS.set(Some(Hooked { grab, sink }));
        // SAFETY: a null name asks for this exe's own module, which holds
        // the hook procedure and needs no closing.
        let instance = unsafe { GetModuleHandleW(ptr::null()) };
        // SAFETY: the procedure has HOOKPROC's signature. A low-level hook is
        // called on this thread, inside GetMessageW, until unhook_keys or
        // Drop removes it.
        let hook = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keys_hook), instance, 0) };
        if hook.is_null() {
            KEYS.set(None);
            self.keys_refused = true;
            return;
        }
        self.keys_hook = hook;
        self.shared.set_hooked(true);
    }

    // What it held goes with it; those keys went to the sharer.
    fn unhook_keys(&mut self) {
        if self.keys_hook.is_null() {
            return;
        }
        // SAFETY: set on this thread and removed once. One that Windows took
        // away already only fails.
        unsafe { UnhookWindowsHookEx(self.keys_hook) };
        self.keys_hook = ptr::null_mut();
        KEYS.set(None);
        self.shared.set_hooked(false);
    }

    // Decided with the same switch the hook reads, the viewer in front
    // included.
    fn hook_took(&self, raw: RawKey) -> bool {
        if self.keys_hook.is_null() {
            return false;
        }
        KEYS.with(|hooked| {
            hooked.try_borrow_mut().is_ok_and(|mut hooked| {
                hooked.as_mut().is_some_and(|hooked| {
                    let on = hooked.sink.windows_keys().is_some();
                    hooked.grab.raw(raw, on)
                })
            })
        })
    }

    // Windows removes a low-level hook without a word when the thread that
    // set it answers too late, and there is no call that says whether it is
    // still in. Raw Input still reports every key, so keys it sees and the
    // hook does not tell. The keys the hook would have taken stay on this PC
    // meanwhile; the hook goes back in with the next message.
    fn check_hook(&mut self) {
        if self.keys_hook.is_null() {
            return;
        }
        let lost = KEYS.with(|hooked| {
            hooked
                .try_borrow()
                .is_ok_and(|hooked| hooked.as_ref().is_some_and(|hooked| hooked.grab.lost()))
        });
        if lost {
            self.unhook_keys();
            self.shared.hook_lost();
        }
    }

    // The watch runs only while a key is down, so an idle PC never wakes
    // this thread.
    fn follow_watch(&mut self, run: &Loop) {
        let want = run.watching();
        if want == self.timer {
            return;
        }
        // SAFETY: the window is this thread's own and alive; the timer has
        // no callback, so it arrives as WM_TIMER in the loop above.
        let ok = unsafe {
            if want {
                SetTimer(self.hwnd, WATCH_TIMER, WATCH_EVERY_MS, None) != 0
            } else {
                KillTimer(self.hwnd, WATCH_TIMER) != 0
            }
        };
        if ok {
            self.timer = want;
        }
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        // No switch posts to it from here on.
        self.shared.set_window(None);
        self.unhook_keys();
        self.give_mouse_back();
        // SAFETY: each handle was made in `open` on this thread and is let go
        // once, here. Killing a timer that is not set only fails.
        unsafe {
            if !self.hook.is_null() {
                UnhookWinEvent(self.hook);
            }
            KillTimer(self.hwnd, WATCH_TIMER);
        }
        // Only when the registration is still this window's: another one in
        // this process, made later, would be removed with it.
        if registered_to(self.hwnd).is_some() {
            register(&RAWINPUTDEVICE {
                usUsagePage: USAGE_PAGE_GENERIC,
                usUsage: USAGE_KEYBOARD,
                dwFlags: RIDEV_REMOVE,
                hwndTarget: ptr::null_mut(),
            });
        }
        // SAFETY: the window was made on this thread, which is the one that
        // may destroy it, and nothing uses it after this.
        unsafe { DestroyWindow(self.hwnd) };
    }
}

// Out of context: runs on the hotkey thread while it waits in GetMessageW.
// The check itself happens in the loop, with the rest of the thread's work.
unsafe extern "system" fn foreground_changed(
    _hook: HWINEVENTHOOK,
    _event: u32,
    _hwnd: HWND,
    _object: i32,
    _child: i32,
    _thread: u32,
    time: u32,
) {
    // SAFETY: posts a message carrying one integer to this same thread's
    // queue.
    unsafe { PostThreadMessageW(GetCurrentThreadId(), WM_FOREGROUND, time as WPARAM, 0) };
}

// Microsoft documents WM_INPUT_DEVICE_CHANGE as sent to the window, where
// it would never come out of GetMessageW; Windows 11 posts it. Sent or
// posted, it ends up here, and goes back to the loop as a message of its
// own. Everything else, the cleanup after WM_INPUT included, is Windows'
// default handling.
unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_INPUT_DEVICE_CHANGE && wparam == GIDC_REMOVAL as WPARAM {
        // SAFETY: posts the device handle, which is only compared as a
        // number, to this same window, which is alive while it gets
        // messages.
        unsafe { PostMessageW(hwnd, WM_DEVICE_GONE, 0, lparam) };
        return 0;
    }
    // SAFETY: the arguments are the ones Windows called with.
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

// While it is in, every key on this PC waits for this, so it only decides
// and hands on.
unsafe extern "system" fn keys_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        // SAFETY: with HC_ACTION, a low-level keyboard hook's lparam points
        // at a KBDLLHOOKSTRUCT that lives for the call.
        let event = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
        let take = KEYS.with(|hooked| {
            hooked
                .try_borrow_mut()
                .is_ok_and(|mut hooked| hooked.as_mut().is_some_and(|hooked| hooked.event(event)))
        });
        if take {
            return 1;
        }
    }
    // SAFETY: Windows' own arguments, passed on to the next hook; the first
    // argument is ignored.
    unsafe { CallNextHookEx(ptr::null_mut(), code, wparam, lparam) }
}

// Asked at every event while the viewer sends, so it is the one call.
pub(crate) fn foreground_window() -> isize {
    // SAFETY: takes no arguments; a null result means no window in front.
    unsafe { GetForegroundWindow() as isize }
}

// Whether this process's mouse registration still names `window`, with the
// flag that brings the mouse from behind other windows.
pub(crate) fn mouse_registered_to(window: isize) -> bool {
    registration(USAGE_MOUSE).is_some_and(|device| {
        device.hwndTarget as isize == window && device.dwFlags & RIDEV_INPUTSINK == BACKGROUND
    })
}

// Only asks and posts: safe from any thread.
pub(crate) fn wake_for_switches(window: isize) {
    // SAFETY: posting copies three integers into the window's queue. The
    // caller holds the lock under which the window's own thread forgets it
    // before destroying it.
    unsafe { PostMessageW(window as HWND, WM_SWITCHES, 0, 0) };
}

pub(crate) fn raise_priority() -> io::Result<()> {
    // SAFETY: GetCurrentThread returns a pseudo handle for the calling
    // thread, which needs no closing.
    if unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST) } == 0 {
        return Err(context(
            "could not raise the hotkey thread's priority",
            io::Error::last_os_error(),
        ));
    }
    Ok(())
}

fn register(device: &RAWINPUTDEVICE) -> bool {
    // SAFETY: one RAWINPUTDEVICE with its size. A removal names no window;
    // any other names one made on this thread, or one the process kept.
    unsafe { RegisterRawInputDevices(device, 1, size_of::<RAWINPUTDEVICE>() as u32) != 0 }
}

// This process's registration for a generic desktop usage, if it has one.
fn registration(usage: u16) -> Option<RAWINPUTDEVICE> {
    let mut devices = [RAWINPUTDEVICE::default(); MOST_REGISTRATIONS];
    let mut count = MOST_REGISTRATIONS as u32;
    // SAFETY: `devices` has room for `count` entries of the size passed.
    let got = unsafe {
        GetRegisteredRawInputDevices(
            devices.as_mut_ptr(),
            &mut count,
            size_of::<RAWINPUTDEVICE>() as u32,
        )
    };
    if got == u32::MAX {
        return None;
    }
    devices[..(got as usize).min(MOST_REGISTRATIONS)]
        .iter()
        .find(|device| device.usUsagePage == USAGE_PAGE_GENERIC && device.usUsage == usage)
        .copied()
}

// The flags of this process's keyboard registration, when it names `hwnd`.
fn registered_to(hwnd: HWND) -> Option<u32> {
    registration(USAGE_KEYBOARD)
        .filter(|device| device.hwndTarget == hwnd)
        .map(|device| device.dwFlags)
}

enum Raw {
    Key(RawKey),
    Mouse(RawMouse),
}

fn read_raw(handle: HRAWINPUT) -> Option<Raw> {
    let mut input = RAWINPUT::default();
    let mut size = size_of::<RAWINPUT>() as u32;
    // SAFETY: `input` is writable for `size` bytes, which holds a keyboard
    // or a mouse event, the only kinds registered; anything larger fails
    // the call instead of writing past it.
    let read = unsafe {
        GetRawInputData(
            handle,
            RID_INPUT,
            (&raw mut input).cast(),
            &mut size,
            size_of::<RAWINPUTHEADER>() as u32,
        )
    };
    if read == u32::MAX || read == 0 {
        return None;
    }
    let device = input.header.hDevice as usize;
    match input.header.dwType {
        RIM_TYPEKEYBOARD => {
            // SAFETY: the header says the union holds a keyboard event.
            let keyboard = unsafe { input.data.keyboard };
            Some(Raw::Key(RawKey {
                make_code: keyboard.MakeCode,
                flags: keyboard.Flags,
                vkey: keyboard.VKey,
                device,
            }))
        }
        RIM_TYPEMOUSE => {
            // SAFETY: the header says the union holds a mouse event, and
            // its button fields are two plain u16 over one u32.
            let (mouse, buttons) =
                unsafe { (input.data.mouse, input.data.mouse.Anonymous.Anonymous) };
            Some(Raw::Mouse(RawMouse {
                flags: mouse.usFlags,
                buttons: buttons.usButtonFlags,
                data: buttons.usButtonData,
                x: mouse.lLastX,
                y: mouse.lLastY,
                device,
            }))
        }
        _ => None,
    }
}

// A locked screen and the administrator prompt are desktops of their own,
// which a normal process cannot open, and no key reaches it while one of them
// takes the keyboard.
fn input_desktop_open() -> bool {
    // SAFETY: asks for the least access there is; the handle is closed at
    // once.
    unsafe {
        let desktop = OpenInputDesktop(0, 0, DESKTOP_READOBJECTS);
        if desktop.is_null() {
            return false;
        }
        CloseDesktop(desktop);
    }
    true
}

// Asked when another window comes to the front, and by the watch. It takes
// several system calls, so the injector reads the hotkey thread's verdict
// from Remote::paused instead of asking before every event.
pub(crate) fn foreground_elevation() -> io::Result<Elevation> {
    // SAFETY: takes no arguments.
    let hwnd = unsafe { GetForegroundWindow() };
    // No window in front happens between two of them. A desktop this process
    // cannot see is what the watch looks for.
    if hwnd.is_null() {
        return Ok(Elevation::Normal);
    }
    let mut pid = 0;
    // SAFETY: `pid` is a live u32 for the call to fill; a window that has
    // gone leaves it 0.
    unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    if pid == 0 {
        return Ok(Elevation::Normal);
    }
    // A window on this desktop belongs to a process in this session.
    elevation(pid, true)
}

// Whether a process runs as administrator, as Windows' input rules see it:
// an elevated window gets no keys from here and sends none back. Access
// denied while opening a process in this session counts as elevated, since
// only a process with more rights than this one refuses that.
pub fn process_elevation(pid: u32) -> io::Result<Elevation> {
    elevation(pid, false)
}

fn elevation(pid: u32, in_session: bool) -> io::Result<Elevation> {
    // SAFETY: plain values; a null result is checked.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        let err = io::Error::last_os_error();
        return refused(pid, in_session, "open process", err);
    }
    let mut token: HANDLE = ptr::null_mut();
    // SAFETY: `process` is open with the access OpenProcessToken needs, and
    // `token` is a live HANDLE for the call to fill.
    let opened = unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } != 0;
    let failed = (!opened).then(io::Error::last_os_error);
    // SAFETY: opened above, closed once, not used after.
    unsafe { CloseHandle(process) };
    if let Some(err) = failed {
        return refused(pid, in_session, "read the token of process", err);
    }
    let elevated = token_elevated(token);
    // SAFETY: opened above, closed once, not used after.
    unsafe { CloseHandle(token) };
    elevated
        .map(|yes| {
            if yes {
                Elevation::Elevated
            } else {
                Elevation::Normal
            }
        })
        .map_err(|err| {
            context(
                &format!("could not read the elevation of process {pid}"),
                err,
            )
        })
}

pub fn this_process_elevated() -> bool {
    let mut token: HANDLE = ptr::null_mut();
    // SAFETY: the pseudo handle for this process needs no closing, and
    // `token` is a live HANDLE for the call to fill.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return false;
    }
    let elevated = token_elevated(token);
    // SAFETY: opened above, closed once, not used after.
    unsafe { CloseHandle(token) };
    elevated.unwrap_or(false)
}

fn token_elevated(token: HANDLE) -> io::Result<bool> {
    let mut elevation = TOKEN_ELEVATION::default();
    let mut len = 0u32;
    // SAFETY: `token` is open for query, and `elevation` is writable for the
    // size passed.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            (&raw mut elevation).cast(),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(elevation.TokenIsElevated != 0)
}

fn refused(pid: u32, in_session: bool, what: &str, err: io::Error) -> io::Result<Elevation> {
    let denied = err.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32);
    if denied && (in_session || in_this_session(pid)) {
        return Ok(Elevation::Unreadable);
    }
    Err(context(&format!("could not {what} {pid}"), err))
}

// A service refusing access says nothing about a window. ProcessIdToSessionId
// cannot answer for the processes that matter here, since it needs more
// access to them than they give, so the session list is asked instead.
fn in_this_session(pid: u32) -> bool {
    let mut ours = 0u32;
    // SAFETY: fills one live u32; a process may always ask about itself.
    if unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut ours) } == 0 {
        return false;
    }
    let mut list: *mut WTS_PROCESS_INFOW = ptr::null_mut();
    let mut count = 0u32;
    // SAFETY: this PC's own list, version 1 as the call requires; both
    // pointers are live for the call to fill.
    let listed =
        unsafe { WTSEnumerateProcessesW(WTS_CURRENT_SERVER_HANDLE, 0, 1, &mut list, &mut count) };
    if listed == 0 || list.is_null() {
        return false;
    }
    // SAFETY: on success `list` holds `count` entries until WTSFreeMemory.
    let processes = unsafe { std::slice::from_raw_parts(list, count as usize) };
    let found = processes
        .iter()
        .any(|process| process.ProcessId == pid && process.SessionId == ours);
    // SAFETY: freed once, with the call that goes with the list, after the
    // last read of it.
    unsafe { WTSFreeMemory(list.cast()) };
    found
}

fn context(what: &str, err: io::Error) -> io::Error {
    io::Error::new(err.kind(), format!("{what}: {err}"))
}

#[cfg(test)]
mod tests;
