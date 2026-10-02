// Which process holds a UDP port on this PC, asked when a bind finds the
// port in use. "Address in use" alone sends people to look for another
// program, when the holder is as often Booth itself: a second copy on
// another profile, or this copy before its last room has let go.

use std::ffi::OsString;
use std::mem::{offset_of, size_of};
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;
use std::ptr;

use windows_sys::Win32::Foundation::{CloseHandle, ERROR_INSUFFICIENT_BUFFER, NO_ERROR};
use windows_sys::Win32::NetworkManagement::IpHelper::{
    GetExtendedUdpTable, MIB_UDP6ROW_OWNER_PID, MIB_UDP6TABLE_OWNER_PID, MIB_UDPROW_OWNER_PID,
    MIB_UDPTABLE_OWNER_PID, UDP_TABLE_OWNER_PID,
};
use windows_sys::Win32::Networking::WinSock::{AF_INET, AF_INET6};
use windows_sys::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Holder {
    pub pid: u32,
    // None when Windows will not name the program to this one: the System
    // process, or one running as another account.
    pub exe: Option<PathBuf>,
}

impl Holder {
    pub fn is_this_process(&self) -> bool {
        self.pid == std::process::id()
    }
}

// The table can grow between the size query and the read when a program
// opens a socket in between.
const TRIES: usize = 4;
// Windows' longest path, in UTF-16 units.
const LONGEST_PATH: usize = 32_768;

// The IPv6 table first: a dual-stack socket like Booth's own shows there.
// None when no socket holds the port any more, or Windows would not say.
pub fn udp_port(port: u16) -> Option<Holder> {
    let pid = [AF_INET6, AF_INET]
        .into_iter()
        .find_map(|family| table(family).and_then(|rows| owner(&rows, family, port)))?;
    Some(Holder {
        pid,
        exe: program(pid),
    })
}

// The rows as bytes, read field by field below, so nothing is cast.
#[allow(unsafe_code)]
fn table(family: u16) -> Option<Vec<u8>> {
    let mut size = 0u32;
    for _ in 0..TRIES {
        // u32 storage keeps the rows aligned the way Windows writes them.
        let mut buf = vec![0u32; (size as usize).div_ceil(4)];
        let out = if buf.is_empty() {
            ptr::null_mut()
        } else {
            buf.as_mut_ptr().cast()
        };
        let mut len = u32::try_from(buf.len() * 4).unwrap_or(u32::MAX);
        // SAFETY: `out` is null with `len` 0, or `buf`, writable for `len`
        // bytes; `len` is a live u32 the call overwrites with the size it
        // needs. No reserved value is passed.
        let rc = unsafe {
            GetExtendedUdpTable(out, &mut len, 0, u32::from(family), UDP_TABLE_OWNER_PID, 0)
        };
        match rc {
            NO_ERROR => return Some(buf.iter().flat_map(|word| word.to_ne_bytes()).collect()),
            ERROR_INSUFFICIENT_BUFFER => size = len,
            _ => return None,
        }
    }
    None
}

// The first row on `port`. Ports are in network order in the low 16 bits.
fn owner(rows: &[u8], family: u16, port: u16) -> Option<u32> {
    let (first, row_size, port_at, pid_at) = if family == AF_INET6 {
        (
            offset_of!(MIB_UDP6TABLE_OWNER_PID, table),
            size_of::<MIB_UDP6ROW_OWNER_PID>(),
            offset_of!(MIB_UDP6ROW_OWNER_PID, dwLocalPort),
            offset_of!(MIB_UDP6ROW_OWNER_PID, dwOwningPid),
        )
    } else {
        (
            offset_of!(MIB_UDPTABLE_OWNER_PID, table),
            size_of::<MIB_UDPROW_OWNER_PID>(),
            offset_of!(MIB_UDPROW_OWNER_PID, dwLocalPort),
            offset_of!(MIB_UDPROW_OWNER_PID, dwOwningPid),
        )
    };
    let count = word(rows, 0)? as usize;
    (0..count).find_map(|n| {
        let row = first + n * row_size;
        let local = word(rows, row + port_at)?;
        (u16::from_be(local as u16) == port).then(|| word(rows, row + pid_at))?
    })
}

fn word(bytes: &[u8], at: usize) -> Option<u32> {
    let four = bytes.get(at..at + 4)?;
    Some(u32::from_ne_bytes(four.try_into().ok()?))
}

// The full path of a running program, as Task Manager's details show it.
#[allow(unsafe_code)]
fn program(pid: u32) -> Option<PathBuf> {
    // SAFETY: plain values in; a null handle is checked before any use.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return None;
    }
    let mut buf = vec![0u16; LONGEST_PATH];
    let mut len = LONGEST_PATH as u32;
    // SAFETY: `buf` is writable for `len` units and `len` is a live u32;
    // `process` is open until the CloseHandle, and closed once.
    let ok = unsafe {
        let ok =
            QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len);
        CloseHandle(process);
        ok
    };
    if ok == 0 {
        return None;
    }
    let units = buf.get(..len as usize)?;
    Some(PathBuf::from(OsString::from_wide(units)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read};
    use std::net::{Ipv4Addr, UdpSocket};
    use std::process::{Command, Stdio};

    const PORT_VAR: &str = "BOOTH_TEST_HOLD_UDP_PORT";
    const HOLDING: &str = "holding the udp port";

    #[test]
    fn port_held_by_this_process() {
        let held = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = held.local_addr().unwrap().port();
        let holder = udp_port(port).expect("windows lists the socket");
        assert!(holder.is_this_process(), "{holder:?}");
        let exe = std::env::current_exe().unwrap();
        assert_eq!(holder.exe.as_deref(), Some(exe.as_path()));
    }

    // A second process of this same test program holds the port, as a
    // second copy of Booth would.
    #[test]
    fn port_held_by_another_process() {
        let port = {
            let free = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            free.local_addr().unwrap().port()
        };
        let exe = std::env::current_exe().unwrap();
        let mut child = Command::new(&exe)
            .args([
                "holder::tests::hold_a_port_until_stdin_closes",
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
        let asked = std::time::Instant::now();
        let holder = udp_port(port);
        println!("windows named the holder in {:?}", asked.elapsed());
        drop(child.stdin.take());
        // Read to the end, so the second process can finish its report
        // rather than write into a closed pipe.
        lines.for_each(drop);
        let _ = child.wait();
        let holder = holder.expect("windows lists the other process's socket");
        assert_eq!(holder.pid, child.id());
        assert!(!holder.is_this_process());
        assert_eq!(holder.exe.as_deref(), Some(exe.as_path()));
    }

    #[test]
    #[ignore = "the second process of port_held_by_another_process"]
    fn hold_a_port_until_stdin_closes() {
        let Ok(port) = std::env::var(PORT_VAR) else {
            return;
        };
        let held = UdpSocket::bind((Ipv4Addr::LOCALHOST, port.parse().unwrap())).unwrap();
        println!("{HOLDING} {port}");
        let _ = std::io::stdin().read(&mut [0u8; 1]);
        drop(held);
    }

    #[test]
    fn free_port_has_no_holder() {
        let port = {
            let gone = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            gone.local_addr().unwrap().port()
        };
        assert_eq!(udp_port(port), None);
    }

    #[test]
    fn no_program_for_the_idle_process() {
        // OpenProcess refuses process 0 by its documentation.
        assert_eq!(program(0), None);
        assert!(program(std::process::id()).is_some());
    }

    // Two rows laid out as the IPv4 table is, the second on port 41000.
    #[test]
    fn port_is_in_network_order() {
        let mut rows = Vec::new();
        rows.extend_from_slice(&2u32.to_ne_bytes());
        for (port, pid) in [(5353u16, 1200u32), (41000, 4242)] {
            rows.extend_from_slice(&0u32.to_ne_bytes());
            rows.extend_from_slice(&u32::from(port.to_be()).to_ne_bytes());
            rows.extend_from_slice(&pid.to_ne_bytes());
        }
        assert_eq!(owner(&rows, AF_INET, 41000), Some(4242));
        assert_eq!(owner(&rows, AF_INET, 5353), Some(1200));
        assert_eq!(owner(&rows, AF_INET, 41001), None);
        // A count past the end of what was written finds nothing past it.
        rows[0] = 9;
        assert_eq!(owner(&rows, AF_INET, 41001), None);
        assert_eq!(owner(&[], AF_INET, 41000), None);
    }
}
