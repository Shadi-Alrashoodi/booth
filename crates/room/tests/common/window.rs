// A plain window a test shares: on its own thread, which pumps its messages
// until it is closed, and placed where the test says.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use room::WindowId;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DispatchMessageW, IsWindow, MSG, PM_REMOVE, PeekMessageW, PostMessageW,
    SHOW_WINDOW_CMD, SetWindowTextW, ShowWindowAsync, TranslateMessage, WINDOW_STYLE, WM_CLOSE,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP, WS_VISIBLE,
};
use windows::core::{PCWSTR, w};

pub struct Window(isize);

impl Window {
    // Borderless, so the pictures are exactly this size.
    pub fn open(width: i32, height: i32) -> Window {
        Window::with_style(WS_POPUP, width, height)
    }

    pub fn with_style(style: WINDOW_STYLE, width: i32, height: i32) -> Window {
        let (sent, got) = mpsc::channel();
        thread::spawn(move || {
            // SAFETY: a system class, constant strings, no parent.
            let hwnd = unsafe {
                CreateWindowExW(
                    WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                    w!("STATIC"),
                    w!("Booth window share test"),
                    style | WS_VISIBLE,
                    120,
                    120,
                    width,
                    height,
                    None,
                    None,
                    None,
                    None,
                )
            }
            .unwrap();
            sent.send(hwnd.0 as isize).unwrap();
            let mut msg = MSG::default();
            // SAFETY: a window of this thread; a gone one answers false.
            while unsafe { IsWindow(Some(hwnd)) }.as_bool() {
                // SAFETY: a live MSG for each call.
                while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
                    unsafe {
                        let _ = TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                }
                thread::sleep(Duration::from_millis(5));
            }
        });
        Window(got.recv().unwrap())
    }

    pub fn id(&self) -> WindowId {
        WindowId::of(self.0).unwrap()
    }

    pub fn show(&self, how: SHOW_WINDOW_CMD) {
        // SAFETY: a window of this process; posted to its thread.
        let _ = unsafe { ShowWindowAsync(HWND(self.0 as *mut _), how) };
    }

    pub fn close(&self) {
        // SAFETY: posted; the window's own thread destroys it.
        let _ =
            unsafe { PostMessageW(Some(HWND(self.0 as *mut _)), WM_CLOSE, WPARAM(0), LPARAM(0)) };
    }

    // Its text changes every `every` until what this returns is dropped, so
    // a share of it always has a new picture to send.
    pub fn keep_changing(&self, every: Duration) -> Changing {
        let stop = Arc::new(AtomicBool::new(false));
        let hwnd = self.0;
        let thread = thread::spawn({
            let stop = Arc::clone(&stop);
            move || {
                let mut count = 0u64;
                while !stop.load(Ordering::Acquire) {
                    count += 1;
                    let text: Vec<u16> = format!("Booth window share test {count}")
                        .encode_utf16()
                        .chain([0])
                        .collect();
                    // SAFETY: a window of this process, whose own thread
                    // takes the text, or one gone, which fails the call; the
                    // text ends in a nul and outlives the call.
                    let _ = unsafe { SetWindowTextW(HWND(hwnd as *mut _), PCWSTR(text.as_ptr())) };
                    thread::sleep(every);
                }
            }
        });
        Changing {
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        self.close();
    }
}

pub struct Changing {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Changing {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
