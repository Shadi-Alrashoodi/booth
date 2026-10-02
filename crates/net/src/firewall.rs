// Windows Firewall rules for Booth's own exe, through the firewall's COM
// interface. Reading works for any user. Changing needs an administrator,
// which is why add_rule_for runs only in the elevated --add-firewall-rule
// process and nowhere else.

use std::borrow::Cow;
use std::fmt;
use std::marker::PhantomData;
use std::ops::BitOr;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::ptr;

use windows::Win32::Foundation::{E_ACCESSDENIED, VARIANT_TRUE};
use windows::Win32::NetworkManagement::WindowsFirewall::{
    INetFwPolicy2, INetFwRule, INetFwRules, NET_FW_ACTION_ALLOW, NET_FW_IP_PROTOCOL_ANY,
    NET_FW_IP_PROTOCOL_UDP, NET_FW_MODIFY_STATE_GP_OVERRIDE, NET_FW_MODIFY_STATE_INBOUND_BLOCKED,
    NET_FW_PROFILE_TYPE2, NET_FW_PROFILE2_ALL, NET_FW_RULE_DIR_IN, NetFwPolicy2, NetFwRule,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
    CoUninitialize, IDispatch,
};
use windows::Win32::System::Ole::IEnumVARIANT;
use windows::Win32::System::Variant::{VARIANT, VT_DISPATCH, VariantClear};
use windows::core::{BSTR, Interface};
use windows_sys::Win32::Globalization::{CSTR_EQUAL, CompareStringOrdinal};
use windows_sys::Win32::System::Environment::ExpandEnvironmentStringsForUserW;

// Ours is the rule with this name in this group. Windows names the rules it
// makes from its own prompt after the exe's description, or its file name,
// and puts them in no group, so those are never taken for ours.
const RULE_NAME: &str = "Booth";
const RULE_GROUP: &str = "Booth";
const RULE_DESCRIPTION: &str =
    "Lets friends reach Booth over UDP. Added by Booth after you allowed it.";
// Rules come out of the enumerator this many at a time; the firewall on a
// normal PC has several hundred.
const BATCH: usize = 32;
// The longest path Windows allows, with its terminating zero.
const PATH_CHARS: usize = 32768;

// A set of firewall profiles, in the firewall's own bit mask.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Profiles(i32);

impl Profiles {
    pub const DOMAIN: Profiles = Profiles(1);
    pub const PRIVATE: Profiles = Profiles(2);
    pub const PUBLIC: Profiles = Profiles(4);
    const NAMED: [(Profiles, &'static str); 3] = [
        (Profiles::DOMAIN, "domain"),
        (Profiles::PRIVATE, "private"),
        (Profiles::PUBLIC, "public"),
    ];

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub fn contains(self, other: Profiles) -> bool {
        self.0 & other.0 == other.0
    }

    fn overlaps(self, other: Profiles) -> bool {
        self.0 & other.0 != 0
    }
}

impl BitOr for Profiles {
    type Output = Profiles;

    fn bitor(self, other: Profiles) -> Profiles {
        Profiles(self.0 | other.0)
    }
}

impl fmt::Display for Profiles {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<&str> = Profiles::NAMED
            .iter()
            .filter(|(profile, _)| self.contains(*profile))
            .map(|(_, name)| *name)
            .collect();
        if names.is_empty() {
            f.write_str("none")
        } else {
            f.write_str(&names.join(", "))
        }
    }
}

// The profiles in the first three are the ones the rule has to cover: active
// now, with the firewall on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FirewallState {
    Allowed(Profiles),
    Blocked(Profiles),
    Missing(Profiles),
    // Windows drops everything unsolicited on these profiles whatever the
    // rules say: the "block all incoming connections" switch. No rule, and
    // so no administrator prompt, can help.
    BlockingAll(Profiles),
    // The firewall could not be read, it is off, or group policy decides
    // which rules count, so there is nothing to ask for. The text says
    // which, for the log.
    Unknown(String),
}

impl fmt::Display for FirewallState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FirewallState::Allowed(needed) => {
                write!(f, "allowed; active profiles with the firewall on: {needed}")
            }
            FirewallState::Blocked(needed) => write!(
                f,
                "blocked by a rule for this exe; active profiles with the firewall on: {needed}"
            ),
            FirewallState::Missing(needed) => write!(
                f,
                "no rule for this exe; active profiles with the firewall on: {needed}"
            ),
            FirewallState::BlockingAll(shut) => write!(
                f,
                "windows blocks all incoming connections on {shut}, whatever the rules say"
            ),
            FirewallState::Unknown(why) => write!(f, "not known; {why}"),
        }
    }
}

// The text is Windows' own message and code. A windows::core::Error can hold
// a COM pointer, which must not outlive COM on the thread, so only its text
// leaves this module.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FirewallError {
    Reach(String),
    // The firewall refused the change: this process is not elevated.
    AccessDenied,
    RemoveBlock(String),
    AddRule(String),
}

impl fmt::Display for FirewallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FirewallError::Reach(said) => write!(f, "could not reach windows firewall: {said}"),
            FirewallError::AccessDenied => {
                f.write_str("windows firewall refused the change: access denied")
            }
            FirewallError::RemoveBlock(said) => {
                write!(f, "could not remove a rule that blocks this exe: {said}")
            }
            FirewallError::AddRule(said) => {
                write!(f, "could not add the rule for this exe: {said}")
            }
        }
    }
}

impl std::error::Error for FirewallError {}

fn reach(err: windows::core::Error) -> FirewallError {
    FirewallError::Reach(err.to_string())
}

// Any change the firewall turns down for want of rights is access denied,
// whichever step it was; the parent tells the user the same thing for all.
fn refused(err: windows::core::Error, step: fn(String) -> FirewallError) -> FirewallError {
    if err.code() == E_ACCESSDENIED {
        FirewallError::AccessDenied
    } else {
        step(err.to_string())
    }
}

// `port` is the UDP port Booth listens on; a rule for this exe that allows
// only other ports does not count.
pub fn check(exe: &Path, port: u16) -> FirewallState {
    read(exe, port).unwrap_or_else(|err| FirewallState::Unknown(err.to_string()))
}

fn read(exe: &Path, port: u16) -> Result<FirewallState, FirewallError> {
    let _com = Com::start()?;
    let policy = policy().map_err(reach)?;
    let settings = settings(&policy).map_err(reach)?;
    if let Some(state) = settings.decide() {
        return Ok(state);
    }
    let rules = rule_list(&policy).map_err(reach)?;
    let facts: Vec<RuleFacts> = rules_for(&rules, exe)
        .map_err(reach)?
        .into_iter()
        .map(|found| found.facts)
        .collect();
    Ok(judge(&facts, &settings, port))
}

// Removes every inbound Block rule for this exe, adds the one rule, and only
// then removes earlier rules of ours, so a failed add leaves a working rule
// in place. Rules Windows made that allow this exe stay.
pub fn add_rule_for(exe: &Path) -> Result<(), FirewallError> {
    let _com = Com::start()?;
    let policy = policy().map_err(reach)?;
    let rules = rule_list(&policy).map_err(reach)?;
    let found = rules_for(&rules, exe).map_err(reach)?;
    for (index, old) in found.iter().enumerate() {
        if old.facts.blocks() {
            remove(&rules, old, index).map_err(|err| refused(err, FirewallError::RemoveBlock))?;
        }
    }
    add(&rules, exe).map_err(|err| refused(err, FirewallError::AddRule))?;
    // An old one left behind allows exactly what the new one does, so
    // failing to remove it is not worth failing the step for.
    for (index, old) in found.iter().enumerate() {
        if old.ours && !old.facts.blocks() {
            let _ = remove(&rules, old, index);
        }
    }
    Ok(())
}

// COM on the calling thread, apartment threaded, for as long as this lives.
// Each caller makes it before any COM pointer, so it is dropped after all of
// them. It stays on the thread that made it.
struct Com(PhantomData<*const ()>);

impl Com {
    #[allow(unsafe_code)]
    fn start() -> Result<Com, FirewallError> {
        // SAFETY: no reserved pointer is passed. Any success, S_FALSE
        // included, is paired with the CoUninitialize in drop.
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }
            .ok()
            .map_err(reach)?;
        Ok(Com(PhantomData))
    }
}

impl Drop for Com {
    #[allow(unsafe_code)]
    fn drop(&mut self) {
        // SAFETY: Com::start initialised COM on this thread, and every COM
        // pointer made under it was dropped before this.
        unsafe { CoUninitialize() };
    }
}

#[allow(unsafe_code)]
fn policy() -> windows::core::Result<INetFwPolicy2> {
    // SAFETY: the caller holds a Com, so COM is running on this thread.
    unsafe { CoCreateInstance(&NetFwPolicy2, None, CLSCTX_INPROC_SERVER) }
}

#[allow(unsafe_code)]
fn rule_list(policy: &INetFwPolicy2) -> windows::core::Result<INetFwRules> {
    // SAFETY: a getter on a live interface; the list comes back with its own
    // reference.
    unsafe { policy.Rules() }
}

// The firewall's own switches, which decide before any rule does.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Settings {
    active: Profiles,
    // Active, with the firewall on: the profiles a rule has to cover.
    needed: Profiles,
    // Needed profiles that let in anything no rule blocks.
    open: Profiles,
    // Needed profiles that drop everything unsolicited, rules or not.
    shut: Profiles,
    // Group policy tells Windows to ignore the rules kept on this PC, and
    // its own rules cannot be read from here.
    managed: bool,
}

impl Settings {
    fn decide(&self) -> Option<FirewallState> {
        if self.active.is_empty() {
            return Some(FirewallState::Unknown(String::from(
                "windows reports no active firewall profile",
            )));
        }
        if self.needed.is_empty() {
            return Some(FirewallState::Unknown(format!(
                "windows firewall is off on every active profile ({})",
                self.active
            )));
        }
        if self.managed {
            return Some(FirewallState::Unknown(String::from(
                "group policy decides which firewall rules apply on this pc, and its rules cannot be read",
            )));
        }
        (!self.shut.is_empty()).then_some(FirewallState::BlockingAll(self.shut))
    }
}

#[allow(unsafe_code)]
fn settings(policy: &INetFwPolicy2) -> windows::core::Result<Settings> {
    // SAFETY: getters on a live interface that return plain values.
    unsafe {
        let active = Profiles(policy.CurrentProfileTypes()?);
        let modify = policy.LocalPolicyModifyState()?;
        let mut settings = Settings {
            active,
            managed: modify == NET_FW_MODIFY_STATE_GP_OVERRIDE,
            ..Settings::default()
        };
        for (profile, _) in Profiles::NAMED {
            if !active.contains(profile) {
                continue;
            }
            let kind = NET_FW_PROFILE_TYPE2(profile.0);
            if !policy.get_FirewallEnabled(kind)?.as_bool() {
                continue;
            }
            settings.needed = settings.needed | profile;
            if policy.get_BlockAllInboundTraffic(kind)?.as_bool() {
                settings.shut = settings.shut | profile;
            }
            if policy.get_DefaultInboundAction(kind)? == NET_FW_ACTION_ALLOW {
                settings.open = settings.open | profile;
            }
        }
        // Windows says a new rule would not take effect because inbound is
        // shut, without saying where. Better no prompt than a useless one.
        if modify == NET_FW_MODIFY_STATE_INBOUND_BLOCKED && settings.shut.is_empty() {
            settings.shut = settings.needed;
        }
        Ok(settings)
    }
}

struct Found {
    rule: INetFwRule,
    name: BSTR,
    ours: bool,
    facts: RuleFacts,
}

// Only the program is read from other rules, so a firewall with hundreds of
// them costs a few milliseconds.
fn rules_for(rules: &INetFwRules, exe: &Path) -> windows::core::Result<Vec<Found>> {
    let exe: Vec<u16> = exe.as_os_str().encode_wide().collect();
    let mut found = Vec::new();
    for rule in every_rule(rules)? {
        if let Some(program) = program(&rule)
            && same_exe(&program, &exe, expand_system)
        {
            let name = name(&rule)?;
            found.push(Found {
                ours: name == RULE_NAME && group(&rule)? == RULE_GROUP,
                facts: facts(&rule)?,
                name,
                rule,
            });
        }
    }
    Ok(found)
}

#[allow(unsafe_code)]
fn every_rule(rules: &INetFwRules) -> windows::core::Result<Vec<INetFwRule>> {
    // SAFETY: a getter on a live interface; the enumerator comes back with
    // its own reference.
    let list: IEnumVARIANT = unsafe { rules._NewEnum() }?.cast()?;
    let mut all = Vec::new();
    loop {
        let mut batch: [VARIANT; BATCH] = Default::default();
        let mut fetched = 0u32;
        // SAFETY: `batch` is BATCH empty VARIANTs for the call to fill, and
        // `fetched` is a live u32 it writes the count to.
        let status = unsafe { list.Next(&mut batch, &mut fetched) };
        let fetched = (fetched as usize).min(BATCH);
        // Every filled VARIANT is emptied before anything below can return
        // early, so none of them keeps a rule alive.
        let taken: Vec<IDispatch> = batch[..fetched]
            .iter_mut()
            // SAFETY: each of these was filled in by Next just now.
            .filter_map(|variant| unsafe { take_dispatch(variant) })
            .collect();
        status.ok()?;
        for dispatch in taken {
            all.push(dispatch.cast()?);
        }
        // S_FALSE with a short batch is the end of the list.
        if fetched < BATCH {
            return Ok(all);
        }
    }
}

// SAFETY (caller): `variant` must have been filled in by IEnumVARIANT::Next
// and not cleared since.
#[allow(unsafe_code)]
unsafe fn take_dispatch(variant: &mut VARIANT) -> Option<IDispatch> {
    // SAFETY: vt names the live member of the union. For VT_DISPATCH that is
    // pdispVal, and the clone takes a reference of its own.
    let dispatch = unsafe {
        let inner = &variant.Anonymous.Anonymous;
        if inner.vt == VT_DISPATCH {
            (*inner.Anonymous.pdispVal).clone()
        } else {
            None
        }
    };
    // SAFETY: releases what the VARIANT held, once, and leaves it empty. It
    // fails only for types a list of rules never holds.
    let _ = unsafe { VariantClear(variant) };
    dispatch
}

// A rule whose program cannot be read is not counted as one of ours.
#[allow(unsafe_code)]
fn program(rule: &INetFwRule) -> Option<BSTR> {
    // SAFETY: a getter on a live interface; the BSTR is ours and freed on
    // drop.
    let program = unsafe { rule.ApplicationName() }.ok()?;
    (!program.is_empty()).then_some(program)
}

#[allow(unsafe_code)]
fn name(rule: &INetFwRule) -> windows::core::Result<BSTR> {
    // SAFETY: as in program.
    unsafe { rule.Name() }
}

#[allow(unsafe_code)]
fn group(rule: &INetFwRule) -> windows::core::Result<BSTR> {
    // SAFETY: as in program.
    unsafe { rule.Grouping() }
}

#[allow(unsafe_code)]
fn facts(rule: &INetFwRule) -> windows::core::Result<RuleFacts> {
    // SAFETY: getters on a live interface; each BSTR is ours and freed on
    // drop.
    unsafe {
        let protocol = rule.Protocol()?;
        let any_scope = rule.RemoteAddresses()? == "*"
            && rule
                .InterfaceTypes()?
                .to_string()
                .eq_ignore_ascii_case("all")
            && rule.ServiceName()?.is_empty();
        Ok(RuleFacts {
            inbound: rule.Direction()? == NET_FW_RULE_DIR_IN,
            allow: rule.Action()? == NET_FW_ACTION_ALLOW,
            enabled: rule.Enabled()?.as_bool(),
            udp: protocol == NET_FW_IP_PROTOCOL_UDP.0 || protocol == NET_FW_IP_PROTOCOL_ANY.0,
            profiles: Profiles(rule.Profiles()?),
            local_ports: rule.LocalPorts()?.to_string(),
            any_scope,
        })
    }
}

// INetFwRules::Remove takes a name, and names are not unique: Windows gives
// the rules it makes for every copy of booth.exe the same one. The rule gets
// a name nothing else has first, so exactly this one goes.
#[allow(unsafe_code)]
fn remove(rules: &INetFwRules, old: &Found, index: usize) -> windows::core::Result<()> {
    let passing = BSTR::from(format!(
        "Booth, being removed, {} {index}",
        std::process::id()
    ));
    // SAFETY: a setter and Remove on live interfaces, with a BSTR that
    // outlives both calls.
    unsafe {
        old.rule.SetName(&passing)?;
        if let Err(err) = rules.Remove(&passing) {
            // Better its own name back than a strange one left behind.
            let _ = old.rule.SetName(&old.name);
            return Err(err);
        }
    }
    Ok(())
}

// No local port is set, which means any: the port can change in settings.
#[allow(unsafe_code)]
fn add(rules: &INetFwRules, exe: &Path) -> windows::core::Result<()> {
    let program: Vec<u16> = exe.as_os_str().encode_wide().collect();
    // SAFETY: the caller holds a Com, so the rule object can be made; the
    // setters take plain values and BSTRs that outlive each call.
    unsafe {
        let rule: INetFwRule = CoCreateInstance(&NetFwRule, None, CLSCTX_INPROC_SERVER)?;
        rule.SetName(&BSTR::from(RULE_NAME))?;
        rule.SetGrouping(&BSTR::from(RULE_GROUP))?;
        rule.SetDescription(&BSTR::from(RULE_DESCRIPTION))?;
        rule.SetApplicationName(&BSTR::from_wide(&program))?;
        rule.SetProtocol(NET_FW_IP_PROTOCOL_UDP.0)?;
        rule.SetDirection(NET_FW_RULE_DIR_IN)?;
        rule.SetAction(NET_FW_ACTION_ALLOW)?;
        rule.SetProfiles(NET_FW_PROFILE2_ALL.0)?;
        rule.SetEnabled(VARIANT_TRUE)?;
        rules.Add(&rule)
    }
}

// What decides whether one rule for this exe lets Booth's UDP in.
#[derive(Clone, Debug, PartialEq, Eq)]
struct RuleFacts {
    inbound: bool,
    allow: bool,
    enabled: bool,
    udp: bool,
    profiles: Profiles,
    local_ports: String,
    // From any remote address, on every kind of network, for the program
    // itself rather than one of the services it hosts.
    any_scope: bool,
}

impl RuleFacts {
    fn applies(&self, needed: Profiles) -> bool {
        self.inbound && self.enabled && self.udp && self.profiles.overlaps(needed)
    }

    fn blocks(&self) -> bool {
        self.inbound && !self.allow
    }

    // A rule that lets in only the LAN, or only other ports, leaves friends
    // on the internet outside.
    fn lets_in(&self, port: u16) -> bool {
        self.allow && self.any_scope && ports_cover(&self.local_ports, port)
    }
}

// Block wins over allow in Windows, whatever order the rules came in. A
// block of any scope counts, since Allow removes it anyway.
fn judge(rules: &[RuleFacts], settings: &Settings, port: u16) -> FirewallState {
    let needed = settings.needed;
    let applying = || rules.iter().filter(|rule| rule.applies(needed));
    if applying().any(|rule| !rule.allow) {
        return FirewallState::Blocked(needed);
    }
    let covered = applying()
        .filter(|rule| rule.lets_in(port))
        .fold(settings.open, |covered, rule| covered | rule.profiles);
    if covered.contains(needed) {
        FirewallState::Allowed(needed)
    } else {
        FirewallState::Missing(needed)
    }
}

// Empty or "*" is any port. Otherwise a comma list of ports, ranges such as
// "5000-5020", and keywords such as "RPC" that never mean Booth's port.
fn ports_cover(list: &str, port: u16) -> bool {
    let list = list.trim();
    if list.is_empty() {
        return true;
    }
    list.split(',').map(str::trim).any(|item| {
        if item == "*" {
            return true;
        }
        let number = |text: &str| text.trim().parse::<u16>().ok();
        match item.split_once('-') {
            Some((low, high)) => match (number(low), number(high)) {
                (Some(low), Some(high)) => (low..=high).contains(&port),
                _ => false,
            },
            None => number(item) == Some(port),
        }
    })
}

const QUOTE: u16 = b'"' as u16;
const PERCENT: u16 = b'%' as u16;
const LONG_PREFIX: [u16; 4] = [b'\\' as u16, b'\\' as u16, b'?' as u16, b'\\' as u16];

// Compared as UTF-16 without case, the way Windows compares file names, so
// two different names never turn into the same text on the way. A rule's
// path can be in quotes and carry %NAME% variables.
fn same_exe(
    rule_program: &[u16],
    exe: &[u16],
    expand: impl Fn(&[u16]) -> Option<Vec<u16>>,
) -> bool {
    let program = unquote(rule_program);
    let program = if program.contains(&PERCENT) {
        match expand(program) {
            Some(expanded) => Cow::Owned(expanded),
            None => return false,
        }
    } else {
        Cow::Borrowed(program)
    };
    same_name(without_prefix(&program), without_prefix(exe))
}

fn unquote(text: &[u16]) -> &[u16] {
    let text = trim(text);
    match text {
        [QUOTE, inner @ .., QUOTE] => trim(inner),
        _ => text,
    }
}

fn trim(text: &[u16]) -> &[u16] {
    let space = |unit: &u16| char::from_u32(u32::from(*unit)).is_some_and(char::is_whitespace);
    let start = text
        .iter()
        .position(|unit| !space(unit))
        .unwrap_or(text.len());
    let end = text
        .iter()
        .rposition(|unit| !space(unit))
        .map_or(start, |last| last + 1);
    &text[start..end]
}

// The same path can come back from Windows as \\?\C:\...
fn without_prefix(path: &[u16]) -> &[u16] {
    match path.strip_prefix(&LONG_PREFIX[..]) {
        Some(rest) if rest.get(1) == Some(&u16::from(b':')) => rest,
        _ => path,
    }
}

#[allow(unsafe_code)]
fn same_name(a: &[u16], b: &[u16]) -> bool {
    let (Ok(a_len), Ok(b_len)) = (i32::try_from(a.len()), i32::try_from(b.len())) else {
        return false;
    };
    // SAFETY: both pointers are valid for the lengths passed, and the call
    // only reads them.
    unsafe { CompareStringOrdinal(a.as_ptr(), a_len, b.as_ptr(), b_len, 1) == CSTR_EQUAL }
}

// The machine's own variables only. The firewall service expands a rule's
// path in its own context, where %USERPROFILE% is not this user's folder,
// and a user can set their own variables to anything, the elevated helper's
// included. A name the machine does not have stays as written.
#[allow(unsafe_code)]
fn expand_system(text: &[u16]) -> Option<Vec<u16>> {
    if text.contains(&0) {
        return None;
    }
    let source: Vec<u16> = text.iter().copied().chain([0]).collect();
    let mut out = vec![0u16; PATH_CHARS];
    // SAFETY: `source` ends in a zero, `out` is writable for the count
    // passed, and a null token asks for the system's variables only.
    let ok = unsafe {
        ExpandEnvironmentStringsForUserW(
            ptr::null_mut(),
            source.as_ptr(),
            out.as_mut_ptr(),
            PATH_CHARS as u32,
        )
    };
    if ok == 0 {
        return None;
    }
    let end = out.iter().position(|&unit| unit == 0)?;
    out.truncate(end);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXE: &str = r"D:\Code\target\release\booth.exe";

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().collect()
    }

    // Stands in for the machine's variables, so the tests do not depend on
    // where Windows is installed.
    fn machine(text: &[u16]) -> Option<Vec<u16>> {
        let text = String::from_utf16(text).ok()?;
        let text = text
            .replace("%CODE%", r"D:\Code")
            .replace("%code%", r"D:\Code")
            .replace("%Code%", r"D:\Code")
            .replace("%SystemRoot%", r"C:\Windows");
        Some(wide(&text))
    }

    fn same(rule_program: &str) -> bool {
        same_exe(&wide(rule_program), &wide(EXE), machine)
    }

    #[test]
    fn paths_match_without_case() {
        assert!(same(EXE));
        assert!(same(r"d:\code\target\release\booth.exe"));
        assert!(same(r"D:\CODE\TARGET\RELEASE\BOOTH.EXE"));
        assert!(!same(r"D:\Code\target\debug\booth.exe"));
        assert!(!same(r"D:\Code\target\release\booth.exe.old"));
        assert!(!same(""));
    }

    #[test]
    fn paths_match_after_quotes_and_prefix() {
        assert!(same(r#""D:\Code\target\release\booth.exe""#));
        assert!(same(r#"  "d:\code\target\release\booth.exe"  "#));
        assert!(same(r"\\?\D:\Code\target\release\booth.exe"));
        assert!(same_exe(
            &wide(EXE),
            &wide(r"\\?\D:\Code\target\release\booth.exe"),
            machine
        ));
        assert!(!same(r#""D:\Code\target\release\booth.exe"#));
        assert!(!same(r#"""#));
    }

    #[test]
    fn paths_match_after_environment_variables() {
        assert!(same(r"%CODE%\target\release\booth.exe"));
        assert!(same(r"%code%\target\release\booth.exe"));
        assert!(same(r#""%Code%\target\release\booth.exe""#));
        assert!(!same(r"%SystemRoot%\target\release\booth.exe"));
        assert!(!same(r"%NOT_SET%\target\release\booth.exe"));
        assert!(!same_exe(&wide(r"%CODE%\x"), &wide(r"D:\Code\x"), |_| None));
    }

    // Two names that differ only in characters a lossy conversion would turn
    // into the same replacement character must stay different.
    #[test]
    fn broken_names_stay_apart() {
        let mut lone = wide(r"C:\a\");
        lone.push(0xD800);
        lone.extend(wide(r"\booth.exe"));
        let mut replaced = wide(r"C:\a\");
        replaced.push(0xFFFD);
        replaced.extend(wide(r"\booth.exe"));
        assert!(same_exe(&lone, &lone, machine));
        assert!(!same_exe(&replaced, &lone, machine));
        assert!(!same_exe(&lone, &replaced, machine));
    }

    #[test]
    fn user_variables_are_not_expanded() {
        let home = std::env::var("USERPROFILE").expect("USERPROFILE is set for any user");
        let exe = wide(&format!(r"{home}\Downloads\booth.exe"));
        let rule = wide(r"%USERPROFILE%\Downloads\booth.exe");
        assert!(!same_exe(&rule, &exe, expand_system));
        let windows = std::env::var("SystemRoot").expect("SystemRoot is set on every PC");
        let exe = wide(&format!(r"{windows}\Temp\booth.exe"));
        assert!(same_exe(
            &wide(r"%SystemRoot%\Temp\booth.exe"),
            &exe,
            expand_system
        ));
        assert!(same_exe(
            &wide(r"%systemroot%\temp\BOOTH.exe"),
            &exe,
            expand_system
        ));
        assert_eq!(
            expand_system(&wide("%NOT_SET_ANYWHERE%x")),
            Some(wide("%NOT_SET_ANYWHERE%x"))
        );
        assert_eq!(expand_system(&[b'%' as u16, 0, b'%' as u16]), None);
    }

    fn rule(allow: bool, profiles: Profiles) -> RuleFacts {
        RuleFacts {
            inbound: true,
            allow,
            enabled: true,
            udp: true,
            profiles,
            local_ports: String::from("*"),
            any_scope: true,
        }
    }

    fn needing(needed: Profiles) -> Settings {
        Settings {
            active: needed,
            needed,
            ..Settings::default()
        }
    }

    const PRIVATE: Profiles = Profiles::PRIVATE;
    const PUBLIC: Profiles = Profiles::PUBLIC;
    const ALL: Profiles = Profiles(NET_FW_PROFILE2_ALL.0);
    const PORT: u16 = 41000;

    fn judged(rules: &[RuleFacts], needed: Profiles) -> FirewallState {
        judge(rules, &needing(needed), PORT)
    }

    #[test]
    fn allowed_needs_every_active_profile() {
        let both = PRIVATE | PUBLIC;
        let state = judged(&[rule(true, PRIVATE)], PUBLIC);
        assert_eq!(state, FirewallState::Missing(PUBLIC));
        let state = judged(&[rule(true, PRIVATE)], both);
        assert_eq!(state, FirewallState::Missing(both));
        let state = judged(&[rule(true, PRIVATE), rule(true, PUBLIC)], both);
        assert_eq!(state, FirewallState::Allowed(both));
        let state = judged(&[rule(true, both)], PUBLIC);
        assert_eq!(state, FirewallState::Allowed(PUBLIC));
        let state = judged(&[rule(true, ALL)], both);
        assert_eq!(state, FirewallState::Allowed(both));
        assert_eq!(judged(&[], PUBLIC), FirewallState::Missing(PUBLIC));
    }

    #[test]
    fn block_wins_on_an_active_profile_only() {
        let allow = rule(true, ALL);
        let state = judged(&[allow.clone(), rule(false, PUBLIC)], PUBLIC);
        assert_eq!(state, FirewallState::Blocked(PUBLIC));
        let state = judged(&[rule(false, PUBLIC), allow.clone()], PUBLIC);
        assert_eq!(state, FirewallState::Blocked(PUBLIC));
        let state = judged(&[allow, rule(false, Profiles::DOMAIN)], PUBLIC);
        assert_eq!(state, FirewallState::Allowed(PUBLIC));
        let narrow_block = RuleFacts {
            local_ports: String::from("5000"),
            any_scope: false,
            ..rule(false, PUBLIC)
        };
        assert_eq!(
            judged(&[narrow_block], PUBLIC),
            FirewallState::Blocked(PUBLIC)
        );
    }

    #[test]
    fn only_enabled_inbound_udp_rules_count() {
        let disabled = RuleFacts {
            enabled: false,
            ..rule(true, ALL)
        };
        let outbound = RuleFacts {
            inbound: false,
            ..rule(true, ALL)
        };
        let tcp = RuleFacts {
            udp: false,
            ..rule(true, ALL)
        };
        for other in [disabled, outbound, tcp] {
            assert_eq!(judged(&[other], PUBLIC), FirewallState::Missing(PUBLIC));
        }
        let quiet_blocks = [
            RuleFacts {
                enabled: false,
                ..rule(false, ALL)
            },
            RuleFacts {
                udp: false,
                ..rule(false, ALL)
            },
        ];
        for block in quiet_blocks {
            let state = judged(&[block, rule(true, ALL)], PUBLIC);
            assert_eq!(state, FirewallState::Allowed(PUBLIC));
        }
    }

    #[test]
    fn allow_rule_scope_and_ports() {
        let lan_only = RuleFacts {
            any_scope: false,
            ..rule(true, ALL)
        };
        assert_eq!(judged(&[lan_only], PUBLIC), FirewallState::Missing(PUBLIC));
        for ports in ["41000", "40000-42000", "137, 41000", ""] {
            let allow = RuleFacts {
                local_ports: String::from(ports),
                ..rule(true, ALL)
            };
            assert_eq!(
                judged(&[allow], PUBLIC),
                FirewallState::Allowed(PUBLIC),
                "{ports}"
            );
        }
        let other_port = RuleFacts {
            local_ports: String::from("41094"),
            ..rule(true, ALL)
        };
        assert_eq!(
            judged(&[other_port], PUBLIC),
            FirewallState::Missing(PUBLIC)
        );
    }

    #[test]
    fn local_ports_read_like_the_firewall_writes_them() {
        assert!(ports_cover("*", PORT));
        assert!(ports_cover("", PORT));
        assert!(ports_cover("41000", PORT));
        assert!(ports_cover("7777,41000,7779", PORT));
        assert!(ports_cover("5000-5020, 40999-41001", PORT));
        assert!(!ports_cover("41001", PORT));
        assert!(!ports_cover("5000-5020", PORT));
        assert!(!ports_cover("RPC-EPMap", PORT));
        assert!(!ports_cover("Teredo,", PORT));
        assert!(!ports_cover("Ply2Disc,", PORT));
        assert!(!ports_cover("41000-", PORT));
    }

    #[test]
    fn switches_decide_before_any_rule() {
        let both = PRIVATE | PUBLIC;
        assert_eq!(needing(both).decide(), None);
        let off = Settings {
            active: PUBLIC,
            ..Settings::default()
        };
        assert!(matches!(off.decide(), Some(FirewallState::Unknown(_))));
        let nothing = Settings::default();
        assert!(matches!(nothing.decide(), Some(FirewallState::Unknown(_))));
        let managed = Settings {
            managed: true,
            ..needing(both)
        };
        assert!(matches!(managed.decide(), Some(FirewallState::Unknown(_))));
        let shut = Settings {
            shut: PUBLIC,
            ..needing(both)
        };
        assert_eq!(shut.decide(), Some(FirewallState::BlockingAll(PUBLIC)));
    }

    #[test]
    fn profile_open_by_default() {
        let both = PRIVATE | PUBLIC;
        let open_private = Settings {
            open: PRIVATE,
            ..needing(both)
        };
        let state = judge(&[rule(true, PUBLIC)], &open_private, PORT);
        assert_eq!(state, FirewallState::Allowed(both));
        assert_eq!(
            judge(&[], &open_private, PORT),
            FirewallState::Missing(both)
        );
        let state = judge(&[rule(false, PUBLIC)], &open_private, PORT);
        assert_eq!(state, FirewallState::Blocked(both));
    }

    #[test]
    fn only_inbound_block_rules_are_removed() {
        assert!(rule(false, ALL).blocks());
        assert!(!rule(true, ALL).blocks());
        let outbound = RuleFacts {
            inbound: false,
            ..rule(false, ALL)
        };
        assert!(!outbound.blocks());
    }

    #[test]
    fn profiles_read_as_words() {
        assert_eq!((PRIVATE | PUBLIC).to_string(), "private, public");
        assert_eq!(ALL.to_string(), "domain, private, public");
        assert_eq!(Profiles::default().to_string(), "none");
    }
}
