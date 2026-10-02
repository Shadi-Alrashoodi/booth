// Tells the room when Windows adds, removes or changes one of this PC's own
// addresses: another Wi-Fi, a cable plugged back in, a VPN coming up. The room
// then asks STUN at once instead of at its next round. A router that only gets
// a new outside IPv4 address changes nothing on the PC (a new IPv6 prefix
// does), so this is one sign among several. What changed is not passed on:
// the room checks for itself.

use std::cell::Cell;
use std::ffi::c_void;
use std::panic::{self, AssertUnwindSafe};
use std::{fmt, io, ptr, thread};

use windows_sys::Win32::Foundation::{HANDLE, NO_ERROR};
use windows_sys::Win32::NetworkManagement::IpHelper::{
    CancelMibChangeNotify2, MIB_NOTIFICATION_TYPE, MIB_UNICASTIPADDRESS_ROW,
    NotifyUnicastIpAddressChange,
};
use windows_sys::Win32::Networking::WinSock::AF_UNSPEC;

// Windows only carries a thin pointer, and a closure behind dyn is a fat one,
// hence the second box.
type Callback = Box<dyn Fn() + Send + Sync>;

pub struct AddressWatch {
    // None only while drop runs.
    registration: Option<Registration>,
}

// No Drop of its own: whoever holds it must call cancel, and a Registration
// that is dropped instead is leaked, which is safe.
struct Registration {
    handle: HANDLE,
    // From Box::into_raw in register. Freed only in cancel.
    context: *mut Callback,
}

thread_local! {
    // CancelMibChangeNotify2 waits for running callbacks to return, so a
    // callback thread that called it for its own registration would wait for
    // itself forever.
    static IN_CALLBACK: Cell<bool> = const { Cell::new(false) };
}

// The callback runs on a Windows worker thread, one call at a time, for every
// address added, removed or changed, IPv4 and IPv6, and not once at the start.
// Dropping the watch waits for a running call to return, so the callback must
// never wait on anything the dropping thread could hold, a full bounded
// channel included: post on an unbounded channel and return.
pub fn start(callback: impl Fn() + Send + Sync + 'static) -> io::Result<AddressWatch> {
    register(Box::new(callback), false)
}

// `call_now` asks Windows for one call with no change behind it. Windows makes
// it on the calling thread before NotifyUnicastIpAddressChange returns. Only
// the tests use it, to have a callback running when they want one.
#[allow(unsafe_code)]
fn register(callback: Callback, call_now: bool) -> io::Result<AddressWatch> {
    let context = Box::into_raw(Box::new(callback));
    let mut handle: HANDLE = ptr::null_mut();
    // SAFETY: on_change has the PUNICAST_IPADDRESS_CHANGE_CALLBACK signature,
    // `context` stays valid until CancelMibChangeNotify2 for this handle has
    // returned (Registration::cancel), and `handle` is a live HANDLE for the
    // call to fill.
    let rc = unsafe {
        NotifyUnicastIpAddressChange(
            AF_UNSPEC,
            Some(on_change),
            context.cast_const().cast::<c_void>(),
            call_now,
            &mut handle,
        )
    };
    if rc != NO_ERROR {
        // SAFETY: nothing was registered, so no callback can hold `context`,
        // and it came from Box::into_raw above.
        drop(unsafe { Box::from_raw(context) });
        let err = io::Error::from_raw_os_error(rc as i32);
        return Err(io::Error::new(
            err.kind(),
            format!("could not watch for address changes: {err}"),
        ));
    }
    Ok(AddressWatch {
        registration: Some(Registration { handle, context }),
    })
}

#[allow(unsafe_code)]
unsafe extern "system" fn on_change(
    context: *const c_void,
    _row: *const MIB_UNICASTIPADDRESS_ROW,
    _kind: MIB_NOTIFICATION_TYPE,
) {
    // SAFETY: `context` is the pointer register handed to Windows. Windows
    // calls this only while the registration is live, and the box behind it
    // is freed only after CancelMibChangeNotify2 has returned, which it does
    // only once no call like this one is still running.
    let callback = unsafe { &*context.cast::<Callback>() };
    let outer = IN_CALLBACK.replace(true);
    // A panic must not unwind into Windows' thread: Rust would abort the whole
    // app. A missed address notice is not worth a dropped call.
    let _ = panic::catch_unwind(AssertUnwindSafe(callback));
    IN_CALLBACK.set(outer);
}

impl Registration {
    #[allow(unsafe_code)]
    fn cancel(self) {
        // SAFETY: `handle` came from a successful NotifyUnicastIpAddressChange
        // and a Registration is cancelled at most once, since this takes self.
        let rc = unsafe { CancelMibChangeNotify2(self.handle) };
        if rc != NO_ERROR {
            // The registration may still be live, so Windows may still hand
            // `context` to a callback. Leaking the box is the only safe thing.
            return;
        }
        // SAFETY: CancelMibChangeNotify2 has returned: callbacks that were
        // running have returned and no new one starts, so nothing else can
        // reach `context`. It came from Box::into_raw in register.
        drop(unsafe { Box::from_raw(self.context) });
    }
}

// SAFETY: any thread may pass the handle to CancelMibChangeNotify2 (Windows
// only asks that it is not a callback thread of the same handle, which Drop
// sees to), and the box it owns holds a Send + Sync callback.
#[allow(unsafe_code)]
unsafe impl Send for Registration {}

// SAFETY: nothing at all can be done through a shared reference to it.
#[allow(unsafe_code)]
unsafe impl Sync for Registration {}

impl Drop for AddressWatch {
    fn drop(&mut self) {
        let Some(registration) = self.registration.take() else {
            return;
        };
        if !IN_CALLBACK.get() {
            registration.cancel();
            return;
        }
        // Dropped from inside a callback, this watch's own or another one's
        // (nothing says Windows does not run every registration's calls on
        // one thread in turn). A new thread can wait for this call to return
        // where this one cannot. If no thread can be made, the closure and
        // the registration in it are dropped without a cancel: leaked, safe.
        let _ = thread::Builder::new()
            .name("stop address watch".into())
            .spawn(move || registration.cancel());
    }
}

impl fmt::Debug for AddressWatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AddressWatch")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::mpsc::{self, Sender};
    use std::time::Duration;

    const WAIT: Duration = Duration::from_secs(5);

    // Reports the name of the thread that freed the callback it was moved
    // into, which tells which way Drop went.
    struct FreedOn(Sender<Option<String>>);

    impl Drop for FreedOn {
        fn drop(&mut self) {
            let _ = self.0.send(thread::current().name().map(str::to_owned));
        }
    }

    fn freed_on() -> (FreedOn, mpsc::Receiver<Option<String>>) {
        let (sender, receiver) = mpsc::channel();
        (FreedOn(sender), receiver)
    }

    fn this_thread() -> Option<String> {
        thread::current().name().map(str::to_owned)
    }

    // Windows makes the first call on the calling thread, before register
    // returns. The flag has to be down again afterwards, or every later drop
    // on this thread would go the slow way.
    #[test]
    fn first_call_leaves_the_thread_as_it_was() {
        let (freed, receiver) = freed_on();
        let (sender, calls) = mpsc::channel();
        let watch = register(
            Box::new(move || {
                let _ = &freed;
                let _ = sender.send(());
            }),
            true,
        )
        .unwrap();
        calls
            .recv_timeout(WAIT)
            .expect("windows did not make the first call");
        drop(watch);
        assert_eq!(receiver.recv_timeout(WAIT).unwrap(), this_thread());
    }

    #[test]
    fn panic_in_the_callback() {
        let (freed, receiver) = freed_on();
        let (sender, calls) = mpsc::channel();
        let watch = register(
            Box::new(move || {
                let _ = &freed;
                let _ = sender.send(());
                panic!("callback panics on purpose");
            }),
            true,
        )
        .unwrap();
        calls
            .recv_timeout(WAIT)
            .expect("windows did not make the first call");
        drop(watch);
        assert_eq!(receiver.recv_timeout(WAIT).unwrap(), this_thread());
    }

    #[test]
    fn drop_inside_a_callback() {
        let (freed, receiver) = freed_on();
        let other = start(move || {
            let _ = &freed;
        })
        .unwrap();
        let slot = Mutex::new(Some(other));
        let (sender, dropped) = mpsc::channel();
        let watch = register(
            Box::new(move || {
                let other = slot.lock().unwrap().take();
                if other.is_some() {
                    drop(other);
                    let _ = sender.send(());
                }
            }),
            true,
        )
        .unwrap();
        dropped
            .recv_timeout(WAIT)
            .expect("the callback did not get past the drop");
        drop(watch);
        assert_eq!(
            receiver.recv_timeout(WAIT).unwrap().as_deref(),
            Some("stop address watch")
        );
    }
}
