// The raw adapter list from GetAdaptersAddresses and the route lookup, copied
// into plain Rust values so that every decision about addresses and paths is
// made in safe code.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::{io, mem, ptr, slice};

use socket2::SockAddr;
use windows_sys::Win32::Foundation::{
    ERROR_BUFFER_OVERFLOW, ERROR_NO_DATA, ERROR_SUCCESS, NO_ERROR,
};
use windows_sys::Win32::NetworkManagement::IpHelper::{
    GAA_FLAG_INCLUDE_GATEWAYS, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER,
    GAA_FLAG_SKIP_MULTICAST, GetAdaptersAddresses, GetBestInterfaceEx, GetIfEntry2,
    IP_ADAPTER_ADDRESSES_LH, IP_ADAPTER_GATEWAY_ADDRESS_LH, MIB_IF_ROW2,
};
use windows_sys::Win32::NetworkManagement::Ndis::{IfOperStatusUp, NET_LUID_LH};
use windows_sys::Win32::Networking::WinSock::{
    AF_INET, AF_INET6, AF_UNSPEC, IpDadStatePreferred, IpSuffixOriginRandom, SOCKADDR,
    SOCKET_ADDRESS,
};

#[derive(Clone, Debug, Default)]
pub(crate) struct Adapter {
    pub name: String,
    pub description: String,
    pub up: bool,
    pub has_gateway: bool,
    // Windows' own word on whether this is a real network card, as opposed to
    // a virtual switch, a tunnel or the loopback. None when it would not say.
    pub hardware: Option<bool>,
    // Both families mixed, in Windows' order.
    pub gateways: Vec<IpAddr>,
    pub addrs: Vec<AdapterAddr>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct AdapterAddr {
    pub ip: IpAddr,
    // Windows marks its daily temporary IPv6 addresses with a random suffix.
    pub random_suffix: bool,
    // Tentative, duplicate and deprecated addresses cannot take new traffic.
    pub preferred: bool,
}

// Microsoft suggests starting at 15 KB. The list can grow between the size
// query and the real call when an adapter appears, hence a few tries.
const FIRST_GUESS: u32 = 15 * 1024;
const TRIES: usize = 4;
// Friendly names and descriptions are far shorter; this only bounds the scan
// if a terminator were ever missing.
const MAX_NAME: usize = 512;

pub(crate) fn list() -> io::Result<Vec<Adapter>> {
    let mut size = FIRST_GUESS;
    for _ in 0..TRIES {
        match fetch(size)? {
            Fetch::Done(raw) => return Ok(walk(&raw)),
            Fetch::Empty => return Ok(Vec::new()),
            Fetch::Grow(needed) => size = needed,
        }
    }
    Err(io::Error::other(
        "could not list network adapters: the list kept growing while it was read",
    ))
}

// Only ever holds what GetAdaptersAddresses wrote when it succeeded, which is
// what makes walking it sound: every pointer inside points back into it.
struct RawList(Vec<u64>);

enum Fetch {
    Done(RawList),
    Empty,
    Grow(u32),
}

#[allow(unsafe_code)]
fn fetch(size: u32) -> io::Result<Fetch> {
    let flags = GAA_FLAG_SKIP_ANYCAST
        | GAA_FLAG_SKIP_MULTICAST
        | GAA_FLAG_SKIP_DNS_SERVER
        | GAA_FLAG_INCLUDE_GATEWAYS;
    // u64 storage keeps the records 8-byte aligned, which they need.
    let mut buf = vec![0u64; (size as usize).div_ceil(8)];
    let mut len = u32::try_from(buf.len() * 8).unwrap_or(u32::MAX);
    // SAFETY: `buf` is writable, 8-byte aligned and at least `len` bytes long,
    // and `len` is a live u32 the call overwrites with the size it needs.
    let rc = unsafe {
        GetAdaptersAddresses(
            u32::from(AF_UNSPEC),
            flags,
            ptr::null(),
            buf.as_mut_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>(),
            &mut len,
        )
    };
    match rc {
        ERROR_SUCCESS => Ok(Fetch::Done(RawList(buf))),
        ERROR_NO_DATA => Ok(Fetch::Empty),
        ERROR_BUFFER_OVERFLOW => Ok(Fetch::Grow(len)),
        code => {
            let err = io::Error::from_raw_os_error(code as i32);
            Err(io::Error::new(
                err.kind(),
                format!("could not list network adapters: {err}"),
            ))
        }
    }
}

#[allow(unsafe_code)]
fn walk(raw: &RawList) -> Vec<Adapter> {
    let mut adapters = Vec::new();
    let mut cur = raw.0.as_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
    while !cur.is_null() {
        // SAFETY: `cur` is the first record or a Next pointer written by
        // GetAdaptersAddresses, so it points at a record inside `raw`, which
        // is borrowed for the whole loop.
        let rec = unsafe { &*cur };
        let mut addrs = Vec::new();
        let mut unicast = rec.FirstUnicastAddress;
        while !unicast.is_null() {
            // SAFETY: as above, a unicast record inside `raw`.
            let u = unsafe { &*unicast };
            // SAFETY: the SOCKET_ADDRESS comes from a record inside `raw`.
            if let Some(ip) = unsafe { read_ip(&u.Address) } {
                addrs.push(AdapterAddr {
                    ip,
                    random_suffix: u.SuffixOrigin == IpSuffixOriginRandom,
                    preferred: u.DadState == IpDadStatePreferred,
                });
            }
            unicast = u.Next;
        }
        let mut gateways = Vec::new();
        let mut gateway = rec.FirstGatewayAddress;
        while !gateway.is_null() {
            // SAFETY: as above, a gateway record inside `raw`.
            let g: &IP_ADAPTER_GATEWAY_ADDRESS_LH = unsafe { &*gateway };
            // SAFETY: the SOCKET_ADDRESS comes from a record inside `raw`.
            if let Some(ip) = unsafe { read_ip(&g.Address) } {
                gateways.push(ip);
            }
            gateway = g.Next;
        }
        adapters.push(Adapter {
            // SAFETY: both strings belong to a record inside `raw`.
            name: unsafe { read_wide(rec.FriendlyName) },
            description: unsafe { read_wide(rec.Description) },
            up: rec.OperStatus == IfOperStatusUp,
            has_gateway: !rec.FirstGatewayAddress.is_null(),
            hardware: is_hardware(rec.Luid).ok(),
            gateways,
            addrs,
        });
        cur = rec.Next;
    }
    adapters
}

// SAFETY (caller): `addr` must come from a live adapter list, so that a
// non-null lpSockaddr points at iSockaddrLength readable bytes.
#[allow(unsafe_code)]
unsafe fn read_ip(addr: &SOCKET_ADDRESS) -> Option<IpAddr> {
    let len = usize::try_from(addr.iSockaddrLength).ok()?;
    if addr.lpSockaddr.is_null() {
        return None;
    }
    // Large enough for SOCKADDR_IN6, the biggest kind we read.
    let mut raw = [0u8; 28];
    let n = len.min(raw.len());
    // SAFETY: the caller guarantees `len` readable bytes; at most that many are
    // copied, into a local array that cannot overlap the adapter list.
    unsafe { ptr::copy_nonoverlapping(addr.lpSockaddr.cast::<u8>(), raw.as_mut_ptr(), n) };
    sockaddr_ip(raw.get(..n)?)
}

fn sockaddr_ip(raw: &[u8]) -> Option<IpAddr> {
    let family = u16::from_ne_bytes(raw.get(0..2)?.try_into().ok()?);
    match family {
        AF_INET => {
            let octets: [u8; 4] = raw.get(4..8)?.try_into().ok()?;
            Some(IpAddr::V4(Ipv4Addr::from(octets)))
        }
        AF_INET6 => {
            let octets: [u8; 16] = raw.get(8..24)?.try_into().ok()?;
            Some(IpAddr::V6(Ipv6Addr::from(octets)))
        }
        _ => None,
    }
}

// SAFETY (caller): `p` must be null or a NUL-terminated UTF-16 string from a
// live adapter list.
#[allow(unsafe_code)]
unsafe fn read_wide(p: *const u16) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut len = 0;
    // SAFETY: every unit up to and including the terminator is readable, and
    // the scan stops at the terminator or at MAX_NAME, whichever comes first.
    while len < MAX_NAME && unsafe { *p.add(len) } != 0 {
        len += 1;
    }
    // SAFETY: the `len` units just scanned are initialised and readable.
    String::from_utf16_lossy(unsafe { slice::from_raw_parts(p, len) })
}

// The adapter Windows' routing table sends a packet for `peer` through, as
// (friendly name, description).
pub(crate) fn route_to(peer: SocketAddr) -> io::Result<(String, String)> {
    let index = best_interface(peer).map_err(|err| {
        io::Error::new(
            err.kind(),
            format!("could not find the route to {peer}: {err}"),
        )
    })?;
    interface_names(index).map_err(|err| {
        io::Error::new(
            err.kind(),
            format!("could not read network adapter {index}: {err}"),
        )
    })
}

#[allow(unsafe_code)]
fn best_interface(peer: SocketAddr) -> io::Result<u32> {
    let addr = SockAddr::from(peer);
    let mut index = 0u32;
    // SAFETY: `addr` holds a complete SOCKADDR_IN or SOCKADDR_IN6 and outlives
    // the call, which only reads it; `index` is a live u32 the call writes.
    let rc = unsafe { GetBestInterfaceEx(addr.as_ptr().cast::<SOCKADDR>(), &mut index) };
    if rc != NO_ERROR {
        return Err(io::Error::from_raw_os_error(rc as i32));
    }
    Ok(index)
}

fn interface_names(index: u32) -> io::Result<(String, String)> {
    let row = interface_row(Interface::Index(index))?;
    Ok((wide_array(&row.Alias), wide_array(&row.Description)))
}

// HardwareInterface, the lowest bit of InterfaceAndOperStatusFlags in
// netioapi.h. It comes from how the adapter's driver registered, so renaming
// an adapter does not change it.
const HARDWARE_INTERFACE: u8 = 1;

fn is_hardware(luid: NET_LUID_LH) -> io::Result<bool> {
    let row = interface_row(Interface::Luid(luid))?;
    Ok(row.InterfaceAndOperStatusFlags._bitfield & HARDWARE_INTERFACE != 0)
}

enum Interface {
    Index(u32),
    Luid(NET_LUID_LH),
}

#[allow(unsafe_code)]
fn interface_row(which: Interface) -> io::Result<MIB_IF_ROW2> {
    // SAFETY: MIB_IF_ROW2 is integers, fixed arrays, GUIDs and unions of
    // integers, for all of which all-zero bytes is a valid value.
    let mut row: MIB_IF_ROW2 = unsafe { mem::zeroed() };
    // Either one is enough for GetIfEntry2; the other stays zero.
    match which {
        Interface::Index(index) => row.InterfaceIndex = index,
        Interface::Luid(luid) => row.InterfaceLuid = luid,
    }
    // SAFETY: `row` is a live, writable MIB_IF_ROW2 with its LUID or index
    // set, which is all GetIfEntry2 reads before filling in the rest.
    let rc = unsafe { GetIfEntry2(&mut row) };
    if rc != NO_ERROR {
        return Err(io::Error::from_raw_os_error(rc as i32));
    }
    Ok(row)
}

fn wide_array(units: &[u16]) -> String {
    let len = units.iter().position(|&u| u == 0).unwrap_or(units.len());
    String::from_utf16_lossy(units.get(..len).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    // What Windows says of each adapter on this PC, printed for the record.
    // A lookup that never worked would read as "virtual" everywhere and leave
    // every gatewayless LAN address out of invites again.
    #[test]
    fn hardware_flag_on_every_adapter() {
        let started = Instant::now();
        let adapters = list().expect("could not list network adapters");
        println!(
            "{} adapters listed in {:.2} ms",
            adapters.len(),
            started.elapsed().as_secs_f64() * 1000.0
        );
        for adapter in &adapters {
            let hardware = match adapter.hardware {
                Some(true) => "hardware",
                Some(false) => "virtual",
                None => "unknown",
            };
            println!(
                "  {hardware:<8} up={:<5} gateway={:<5} {} ({})",
                adapter.up, adapter.has_gateway, adapter.name, adapter.description
            );
        }
        for adapter in &adapters {
            assert!(
                adapter.hardware.is_some(),
                "windows would not describe adapter {}",
                adapter.name
            );
        }
        // A wrong bit would read as virtual everywhere and still pass the
        // checks above and below.
        assert!(
            adapters.iter().any(|a| a.hardware == Some(true)),
            "no adapter reads as a network card"
        );
        let loopback = adapters
            .iter()
            .find(|adapter| adapter.addrs.iter().any(|addr| addr.ip.is_loopback()))
            .expect("no loopback adapter listed");
        assert_eq!(loopback.hardware, Some(false));
    }
}
