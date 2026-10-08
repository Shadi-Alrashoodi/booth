// The host's invite table and the address list that goes into each invite.

use std::net::{IpAddr, Ipv6Addr, SocketAddr, SocketAddrV4};
use std::time::{Duration, Instant};

use invite::{BuildError, Candidate, CandidateKind, Invite, Mapping};
use net::addrs::{AddrKind, LocalAddr};
use zeroize::Zeroizing;

use crate::view::{InviteView, RouterState};

const MAX_IN_INVITE: usize = 8;
// A room holds seven clients, eight people with the host, so this is room for
// every friend's PC and then some. Without a cap, someone holding a leaked 24 h code could mint keys all
// day and push real friends out of the host's key table.
const KEYS_PER_MULTI_USE: usize = 32;

pub(crate) enum Local {
    Discover(Vec<LocalAddr>),
    Fixed(Vec<Candidate>),
}

impl Local {
    // Only the pinned stable address goes in the invite, because every reply
    // to a global IPv6 address leaves from it.
    pub(crate) fn ipv6_source(&self, has_ipv6: bool) -> Option<Ipv6Addr> {
        let Local::Discover(addrs) = self else {
            return None;
        };
        addrs
            .iter()
            .filter(|_| has_ipv6)
            .find_map(|addr| match (addr.kind, addr.ip) {
                (AddrKind::Ipv6Global, IpAddr::V6(v6)) => Some(v6),
                _ => None,
            })
    }

    // A fixed list stands in for the PC's own addresses only; what STUN saw
    // from outside still goes last.
    pub(crate) fn candidates(
        &self,
        port: u16,
        has_ipv6: bool,
        public: &[SocketAddrV4],
    ) -> Vec<Candidate> {
        let mut out = match self {
            Local::Fixed(fixed) => fixed.clone(),
            Local::Discover(addrs) => discovered(addrs, port, has_ipv6, self.ipv6_source(has_ipv6)),
        };
        let mut outside: Vec<Candidate> = Vec::with_capacity(public.len());
        for &addr in public {
            let candidate = Candidate {
                kind: CandidateKind::Public,
                addr: SocketAddr::V4(addr),
            };
            if !out.contains(&candidate) && !outside.contains(&candidate) {
                outside.push(candidate);
            }
        }
        // Still last, but a PC with many adapters and tunnels must not push
        // out the addresses that work from the internet.
        outside.truncate(MAX_IN_INVITE);
        out.truncate(MAX_IN_INVITE - outside.len());
        out.extend(outside);
        out
    }
}

// LAN addresses first, then Tailscale and WireGuard, then the stable IPv6
// address. A LAN address on a virtual adapter without a gateway is left out:
// on a PC with Hyper-V or WSL those are their switches, which no friend can
// reach, and each one makes the code longer. A network card is kept with or
// without a gateway, because on a plain switch or a cable with fixed
// addresses it has none and a friend reaches it all the same. Tailscale and
// WireGuard usually have no gateway either, and are kept whatever they have.
fn discovered(
    addrs: &[LocalAddr],
    port: u16,
    has_ipv6: bool,
    pinned: Option<Ipv6Addr>,
) -> Vec<Candidate> {
    let usable = |addr: &&LocalAddr| has_ipv6 || addr.ip.is_ipv4();
    let mut out = Vec::new();
    let mut add = |kind, addr| {
        let candidate = Candidate { kind, addr };
        if !out.contains(&candidate) {
            out.push(candidate);
        }
    };
    for addr in addrs.iter().filter(usable) {
        if addr.kind == AddrKind::Lan && (addr.has_gateway || addr.hardware_adapter) {
            add(CandidateKind::Lan, SocketAddr::new(addr.ip, port));
        }
    }
    for addr in addrs.iter().filter(usable) {
        if addr.kind == AddrKind::Vpn {
            add(CandidateKind::Vpn, SocketAddr::new(addr.ip, port));
        }
    }
    if let Some(v6) = pinned {
        add(CandidateKind::Ipv6, SocketAddr::new(IpAddr::V6(v6), port));
    }
    out
}

struct Entry {
    id: [u8; 8],
    secret: Zeroizing<[u8; 16]>,
    multi_use: bool,
    expires: Instant,
    expires_at_unix: u64,
    // Single use admits one client key, multi use up to KEYS_PER_MULTI_USE.
    // A key it admitted may handshake again with it until it expires.
    admitted: Vec<[u8; 32]>,
    code: String,
    expired_shown: bool,
    // The address the router mapped, when the invite carries it.
    mapped: Option<SocketAddr>,
    // This PC's public address changed after it was made.
    before_change: bool,
}

impl Entry {
    fn admits(&self, key: &[u8; 32], now: Instant) -> bool {
        now < self.expires && (self.admitted.contains(key) || self.admitted.len() < self.keys())
    }

    fn keys(&self) -> usize {
        if self.multi_use {
            KEYS_PER_MULTI_USE
        } else {
            1
        }
    }
}

pub(crate) struct Invites {
    // Oldest first. The last one is the one the panel shows.
    entries: Vec<Entry>,
    // new_invite before the router test finished: made once it has.
    wanted: Option<bool>,
}

// What went into a new invite, for the log. Nothing in it lets anyone join.
pub(crate) struct Made {
    pub candidates: Vec<Candidate>,
    pub expires_at_unix: u64,
    pub mapped: bool,
    pub mapped_verified: bool,
    pub second_router: bool,
}

pub(crate) struct Recipe {
    pub host_key: [u8; 32],
    pub candidates: Vec<Candidate>,
    pub mapping: Mapping,
    // The address the router mapped, which is also among the candidates.
    pub mapped: Option<SocketAddr>,
    pub mapped_verified: bool,
    pub second_router: bool,
    // The host's own dynamic DNS name, already checked.
    pub address_name: Option<String>,
    pub single_use_lifetime: Duration,
    pub multi_use_lifetime: Duration,
    pub now: Instant,
    pub now_unix: u64,
}

impl Invites {
    pub(crate) fn new() -> Invites {
        Invites {
            entries: Vec::new(),
            wanted: Some(false),
        }
    }

    pub(crate) fn want(&mut self, multi_use: bool) {
        self.wanted = Some(multi_use);
    }

    pub(crate) fn take_wanted(&mut self) -> Option<bool> {
        self.wanted.take()
    }

    pub(crate) fn is_wanted(&self) -> bool {
        self.wanted.is_some()
    }

    // A candidate Invite::new refuses is left out rather than failing the
    // whole invite: the others may still reach this PC.
    pub(crate) fn make(&mut self, recipe: Recipe, multi_use: bool) -> Result<Made, BuildError> {
        let lifetime = if multi_use {
            recipe.multi_use_lifetime
        } else {
            recipe.single_use_lifetime
        };
        let mut candidates = recipe.candidates;
        let mut invite = loop {
            // The flag says a friend can use the address, so it goes only
            // with the address itself.
            let mapped = recipe.mapped.is_some_and(|addr| {
                candidates
                    .iter()
                    .any(|c| c.kind == CandidateKind::Public && c.addr == addr)
            });
            match Invite::new(
                recipe.host_key,
                candidates.clone(),
                recipe.mapping,
                mapped,
                mapped && recipe.mapped_verified,
                recipe.second_router,
                recipe.address_name.clone(),
                multi_use,
                recipe.now_unix,
            ) {
                Ok(invite) => break invite,
                Err(BuildError::BadCandidate { candidate, reason }) => {
                    let before = candidates.len();
                    candidates
                        .retain(|c| c.kind != candidate.kind || c.addr.ip() != candidate.addr.ip());
                    if candidates.len() == before {
                        return Err(BuildError::BadCandidate { candidate, reason });
                    }
                }
                Err(err) => return Err(err),
            }
        };
        invite.expires_at = recipe
            .now_unix
            .saturating_add(lifetime.as_millis().div_ceil(1000) as u64);
        invite.check()?;
        self.entries.push(Entry {
            id: invite.invite_id,
            secret: Zeroizing::new(invite.secret),
            multi_use,
            expires: recipe.now + lifetime,
            expires_at_unix: invite.expires_at,
            admitted: Vec::new(),
            code: invite.encode(),
            expired_shown: false,
            mapped: recipe.mapped.filter(|_| invite.mapped),
            before_change: false,
        });
        Ok(Made {
            candidates: std::mem::take(&mut invite.candidates),
            expires_at_unix: invite.expires_at,
            mapped: invite.mapped,
            mapped_verified: invite.mapped_verified,
            second_router: invite.second_router,
        })
    }

    // Why secret_for found nothing, in words for the log.
    pub(crate) fn refusal(&self, id: &[u8; 8], key: &[u8; 32], now: Instant) -> &'static str {
        match self.entries.iter().find(|entry| entry.id == *id) {
            None => "an invite this host does not have, or one long expired",
            Some(entry) if now >= entry.expires => "an expired invite",
            Some(entry) if entry.multi_use && !entry.admitted.contains(key) => {
                "a multi-use invite that has let in all the keys it may"
            }
            Some(_) => "an invite already used by another key",
        }
    }

    pub(crate) fn secret_for(
        &self,
        id: &[u8; 8],
        key: &[u8; 32],
        now: Instant,
    ) -> Option<Zeroizing<[u8; 16]>> {
        self.entries
            .iter()
            .find(|entry| entry.id == *id && entry.admits(key, now))
            .map(|entry| entry.secret.clone())
    }

    // True when the invite has let `key` in, now or before.
    pub(crate) fn admit(&mut self, id: &[u8; 8], key: [u8; 32]) -> bool {
        let Some(entry) = self.entries.iter_mut().find(|entry| entry.id == *id) else {
            return false;
        };
        if entry.admitted.contains(&key) {
            return true;
        }
        if entry.admitted.len() >= entry.keys() {
            return false;
        }
        entry.admitted.push(key);
        true
    }

    // Returns true when the invite on show just expired.
    pub(crate) fn tick(&mut self, now: Instant) -> bool {
        let last = self.entries.len().saturating_sub(1);
        let mut index = 0;
        self.entries.retain(|entry| {
            let keep = index == last || now < entry.expires;
            index += 1;
            keep
        });
        match self.entries.last_mut() {
            Some(shown) if !shown.expired_shown && now >= shown.expires => {
                shown.expired_shown = true;
                true
            }
            _ => false,
        }
    }

    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        self.entries
            .last()
            .filter(|shown| !shown.expired_shown)
            .map(|shown| shown.expires)
    }

    pub(crate) fn view(&self, now: Instant, router: RouterState) -> Option<InviteView> {
        match self.entries.last() {
            Some(shown) => Some(InviteView {
                code: shown.code.clone(),
                multi_use: shown.multi_use,
                expires_at_unix: shown.expires_at_unix,
                used: !shown.admitted.is_empty(),
                expired: now >= shown.expires,
                router,
                mapped_since: false,
                address_changed_since: shown.before_change,
            }),
            None if router == RouterState::Testing => Some(InviteView {
                code: String::new(),
                multi_use: self.wanted.unwrap_or(false),
                expires_at_unix: 0,
                used: false,
                expired: false,
                router,
                mapped_since: false,
                address_changed_since: false,
            }),
            None => None,
        }
    }

    // This PC's public address changed: the outside addresses in the invite
    // on show lead nowhere now. Returns true when there was one.
    pub(crate) fn shown_before_change(&mut self) -> bool {
        match self.entries.last_mut() {
            Some(shown) => {
                shown.before_change = true;
                true
            }
            None => false,
        }
    }

    // The router now offers `mapped`, and the invite on show was made
    // without it.
    pub(crate) fn shown_lacks(&self, mapped: Option<SocketAddr>) -> bool {
        match (self.entries.last(), mapped) {
            (Some(shown), Some(addr)) => shown.mapped != Some(addr),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn local(ip: &str, kind: AddrKind, has_gateway: bool) -> LocalAddr {
        LocalAddr {
            ip: ip.parse().unwrap(),
            kind,
            adapter: String::from("test"),
            has_gateway,
            gateway: None,
            vpn_adapter: kind == AddrKind::Vpn,
            hardware_adapter: false,
        }
    }

    fn card(ip: &str, has_gateway: bool) -> LocalAddr {
        LocalAddr {
            hardware_adapter: true,
            ..local(ip, AddrKind::Lan, has_gateway)
        }
    }

    #[test]
    fn candidate_order() {
        let addrs = Local::Discover(vec![
            local("172.20.0.1", AddrKind::Lan, false),
            local("2a02:8108::10", AddrKind::Ipv6Global, true),
            local("2a02:8108::11", AddrKind::Ipv6Global, true),
            local("100.101.102.103", AddrKind::Vpn, false),
            card("192.168.1.20", true),
            local("fd7a:115c:a1e0::5", AddrKind::Vpn, false),
            card("10.0.0.2", false),
        ]);
        let got: Vec<(CandidateKind, String)> = addrs
            .candidates(41000, true, &[MAPPED, FORWARDED])
            .into_iter()
            .map(|c| (c.kind, c.addr.to_string()))
            .collect();
        let want = [
            (CandidateKind::Lan, "192.168.1.20:41000"),
            (CandidateKind::Lan, "10.0.0.2:41000"),
            (CandidateKind::Vpn, "100.101.102.103:41000"),
            (CandidateKind::Vpn, "[fd7a:115c:a1e0::5]:41000"),
            (CandidateKind::Ipv6, "[2a02:8108::10]:41000"),
            (CandidateKind::Public, "203.0.113.9:52000"),
            (CandidateKind::Public, "203.0.113.9:41000"),
        ];
        let want: Vec<(CandidateKind, String)> =
            want.iter().map(|(k, a)| (*k, a.to_string())).collect();
        assert_eq!(got, want);

        let v4_only = addrs.candidates(41000, false, &[]);
        assert!(v4_only.iter().all(|c| c.addr.is_ipv4()), "{v4_only:?}");
        assert_eq!(addrs.ipv6_source(false), None);
        assert_eq!(
            addrs.ipv6_source(true),
            Some("2a02:8108::10".parse().unwrap())
        );
    }

    // An invite from my PC on 2026-09-27 carried 172.17.80.1 and
    // 172.23.144.1, the Hyper-V and WSL switches, which have no gateway. A
    // card on a plain switch or a cable with fixed addresses has none
    // either, and stays.
    #[test]
    fn gatewayless_virtual_adapters_left_out() {
        let addrs = Local::Discover(vec![
            local("172.17.80.1", AddrKind::Lan, false),
            card("192.168.1.20", true),
            local("172.23.144.1", AddrKind::Lan, false),
            card("10.0.0.2", false),
            local("100.101.102.103", AddrKind::Vpn, false),
            local("10.8.0.2", AddrKind::Vpn, false),
        ]);
        let got: Vec<(CandidateKind, String)> = addrs
            .candidates(41000, true, &[FORWARDED])
            .into_iter()
            .map(|c| (c.kind, c.addr.to_string()))
            .collect();
        let want = [
            (CandidateKind::Lan, "192.168.1.20:41000"),
            (CandidateKind::Lan, "10.0.0.2:41000"),
            (CandidateKind::Vpn, "100.101.102.103:41000"),
            (CandidateKind::Vpn, "10.8.0.2:41000"),
            (CandidateKind::Public, "203.0.113.9:41000"),
        ];
        let want: Vec<(CandidateKind, String)> =
            want.iter().map(|(k, a)| (*k, a.to_string())).collect();
        assert_eq!(got, want);
    }

    #[test]
    fn public_survives_long_list() {
        let mut addrs = vec![
            local("192.168.1.20", AddrKind::Lan, true),
            local("192.168.0.31", AddrKind::Lan, true),
            local("10.20.0.5", AddrKind::Lan, true),
            local("100.101.102.103", AddrKind::Vpn, false),
            local("fd7a:115c:a1e0::5", AddrKind::Vpn, false),
            local("10.8.0.2", AddrKind::Vpn, false),
            local("2a02:8108::10", AddrKind::Ipv6Global, true),
        ];
        for i in 1..=6 {
            addrs.push(local(&format!("172.{}.0.1", 16 + i), AddrKind::Lan, false));
        }
        let addrs = Local::Discover(addrs);
        let publics = |candidates: &[Candidate]| -> Vec<SocketAddr> {
            candidates
                .iter()
                .filter(|c| c.kind == CandidateKind::Public)
                .map(|c| c.addr)
                .collect()
        };

        let without = addrs.candidates(41000, true, &[]);
        assert_eq!(without.len(), 7);
        assert!(without.iter().all(|c| c.kind != CandidateKind::Public));
        assert!(
            without
                .iter()
                .all(|c| !c.addr.ip().to_string().starts_with("172."))
        );

        let got = addrs.candidates(41000, true, &[MAPPED]);
        assert_eq!(got.len(), MAX_IN_INVITE);
        assert_eq!(got[0].addr.to_string(), "192.168.1.20:41000");
        let last = got.last().unwrap();
        assert_eq!(last.kind, CandidateKind::Public);
        assert_eq!(last.addr, SocketAddr::V4(MAPPED));

        // Both outside addresses keep their seats; the stable IPv6 address,
        // last of this PC's own, made room.
        let both = addrs.candidates(41000, true, &[MAPPED, FORWARDED]);
        assert_eq!(both.len(), MAX_IN_INVITE);
        assert_eq!(both[0].addr.to_string(), "192.168.1.20:41000");
        assert_eq!(
            publics(&both),
            [SocketAddr::V4(MAPPED), SocketAddr::V4(FORWARDED)]
        );
        assert!(both.iter().all(|c| c.kind != CandidateKind::Ipv6));

        // A port the router mapped comes first of the three, and the last
        // tunnel made room as well.
        let by_router = SocketAddrV4::new(Ipv4Addr::new(203, 0, 113, 9), 60123);
        let three = addrs.candidates(41000, true, &[by_router, MAPPED, FORWARDED]);
        assert_eq!(three.len(), MAX_IN_INVITE);
        assert_eq!(three[0].addr.to_string(), "192.168.1.20:41000");
        let want = [by_router, MAPPED, FORWARDED].map(SocketAddr::V4);
        assert_eq!(publics(&three), want);
        assert!(three.iter().all(|c| c.addr.to_string() != "10.8.0.2:41000"));
    }

    #[test]
    fn an_outside_address_goes_in_once() {
        let addrs = Local::Discover(vec![local("192.168.1.20", AddrKind::Lan, true)]);
        let got = addrs.candidates(41000, true, &[FORWARDED, FORWARDED]);
        assert_eq!(got.len(), 2);
        assert_eq!(got[1].addr, SocketAddr::V4(FORWARDED));
    }

    // The invite crate takes two public candidates on one address, told
    // apart by port, and gives both back.
    #[test]
    fn two_public_ports_survive() {
        let mut invites = Invites::new();
        let now = Instant::now();
        let publics = Local::Fixed(Vec::new()).candidates(41000, true, &[MAPPED, FORWARDED]);
        invites
            .make(
                recipe(now, Duration::from_secs(600), publics.clone()),
                false,
            )
            .unwrap();
        let view = invites.view(now, RouterState::Easy).unwrap();
        assert_eq!(Invite::decode(&view.code).unwrap().candidates, publics);
    }

    const MAPPED: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(203, 0, 113, 9), 52000);
    const FORWARDED: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(203, 0, 113, 9), 41000);

    #[test]
    fn multi_use_key_cap() {
        let mut invites = Invites::new();
        let now = Instant::now();
        invites
            .make(recipe(now, Duration::from_secs(600), Vec::new()), true)
            .unwrap();
        let id = Invite::decode(&invites.view(now, RouterState::Unknown).unwrap().code)
            .unwrap()
            .invite_id;
        for n in 0..KEYS_PER_MULTI_USE as u8 {
            assert!(invites.secret_for(&id, &[n; 32], now).is_some(), "key {n}");
            invites.admit(&id, [n; 32]);
        }
        let one_more = [200; 32];
        assert!(invites.secret_for(&id, &one_more, now).is_none());
        invites.admit(&id, one_more);
        assert!(invites.secret_for(&id, &one_more, now).is_none());
        assert!(invites.secret_for(&id, &[0; 32], now).is_some());
    }

    #[test]
    fn debug_output_hides_the_code() {
        let mut invites = Invites::new();
        let now = Instant::now();
        invites
            .make(recipe(now, Duration::from_secs(600), Vec::new()), false)
            .unwrap();
        let view = invites.view(now, RouterState::Unknown).unwrap();
        assert!(!view.code.is_empty());
        for printed in [format!("{view:?}"), format!("{view:#?}")] {
            assert!(!printed.contains(&view.code), "{printed}");
            assert!(printed.contains("hidden"), "{printed}");
        }
    }

    fn recipe(now: Instant, lifetime: Duration, candidates: Vec<Candidate>) -> Recipe {
        Recipe {
            host_key: [9; 32],
            candidates,
            mapping: Mapping::Unknown,
            mapped: None,
            mapped_verified: false,
            second_router: false,
            address_name: None,
            single_use_lifetime: lifetime,
            multi_use_lifetime: lifetime,
            now,
            now_unix: 1_800_000_000,
        }
    }

    #[test]
    fn mapped_flag_needs_mapped_address() {
        let now = Instant::now();
        let public = |addr: &str| Candidate {
            kind: CandidateKind::Public,
            addr: addr.parse().unwrap(),
        };
        let flags = |candidates: Vec<Candidate>, mapped: &str| {
            let mut invites = Invites::new();
            let made = invites
                .make(
                    Recipe {
                        mapped: Some(mapped.parse().unwrap()),
                        mapped_verified: true,
                        second_router: false,
                        ..recipe(now, Duration::from_secs(600), candidates)
                    },
                    false,
                )
                .unwrap();
            let code = invites.view(now, RouterState::Mapped).unwrap().code;
            let decoded = Invite::decode(&code).unwrap();
            assert_eq!(decoded.mapped, made.mapped);
            assert_eq!(decoded.mapped_verified, made.mapped_verified);
            (made.mapped, made.mapped_verified, decoded.candidates.len())
        };
        let kept = vec![public("203.0.113.9:41000"), public("203.0.113.9:52000")];
        assert_eq!(flags(kept.clone(), "203.0.113.9:41000"), (true, true, 2));
        assert_eq!(flags(kept, "203.0.113.9:41001"), (false, false, 2));
        // A router that names a reserved address: the candidate is refused,
        // and the flag with it.
        let refused = vec![public("240.0.0.1:41000"), public("203.0.113.9:52000")];
        assert_eq!(flags(refused, "240.0.0.1:41000"), (false, false, 1));
    }

    #[test]
    fn refused_candidates_are_left_out() {
        let mut invites = Invites::new();
        let now = Instant::now();
        let good = Candidate {
            kind: CandidateKind::Lan,
            addr: "192.168.1.20:41000".parse().unwrap(),
        };
        let loopback = Candidate {
            kind: CandidateKind::Lan,
            addr: "127.0.0.1:41000".parse().unwrap(),
        };
        invites
            .make(
                recipe(now, Duration::from_secs(600), vec![loopback, good]),
                false,
            )
            .unwrap();
        let view = invites.view(now, RouterState::Unknown).unwrap();
        let decoded = Invite::decode(&view.code).unwrap();
        assert_eq!(decoded.candidates, vec![good]);
        assert_eq!(decoded.expires_at, 1_800_000_600);
    }

    #[test]
    fn single_use_admits_one_key_until_expiry() {
        let mut invites = Invites::new();
        let now = Instant::now();
        invites
            .make(recipe(now, Duration::from_millis(300), Vec::new()), false)
            .unwrap();
        let id = Invite::decode(&invites.view(now, RouterState::Unknown).unwrap().code)
            .unwrap()
            .invite_id;
        let (a, b) = ([1; 32], [2; 32]);
        assert!(invites.secret_for(&id, &a, now).is_some());
        invites.admit(&id, a);
        assert!(invites.secret_for(&id, &a, now).is_some());
        assert!(invites.secret_for(&id, &b, now).is_none());
        assert!(invites.view(now, RouterState::Unknown).unwrap().used);

        let later = now + Duration::from_millis(300);
        assert!(invites.secret_for(&id, &a, later).is_none());
        assert!(invites.tick(later));
        assert!(!invites.tick(later));
        assert!(invites.view(later, RouterState::Unknown).unwrap().expired);
    }
}
