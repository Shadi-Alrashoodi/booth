use std::io;

use input::Action;
use invite::{CodeError, Version};
use keys::KeyError;
use room::view::{AddressChanged, Notice, PasteState, ReplyState, RouterState};
use room::{ChatRefused, DamagedList, KnownError, List, ReplyRefused, RoomError};
use voice::audio::Microphone;

use crate::elevated::Exit;
use crate::running::{self, PortHolder};
use crate::settings::SaveError;
use crate::update::RELEASES_PAGE;
use crate::win;

// The lower crates write errors the way a log line reads: lower case, parts
// joined with a semicolon. The panel shows sentences.
pub fn sentence(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 1);
    let mut capital = true;
    for part in text.trim().split("; ") {
        if !out.is_empty() {
            out.push_str(". ");
            capital = true;
        }
        for c in part.chars() {
            if capital {
                out.extend(c.to_uppercase());
                capital = false;
            } else {
                out.push(c);
            }
        }
    }
    if !out.ends_with(['.', '?']) {
        out.push('.');
    }
    out
}

// A friend's name set into an English line goes between Unicode's first
// strong isolate and its end. An Arabic name then reads right to left in its
// place, instead of being the first letter of the line and turning the whole
// line around, or taking the number after it into its own run.
pub fn isolated(name: &str) -> String {
    format!("\u{2068}{name}\u{2069}")
}

// Who has the port, as Windows named it right after the bind failed
// (running::port_holder).
const BIND_IN_USE: &str =
    "Could not bind UDP port {port}: another program is using it. Change the port in settings.";
const BIND_IN_USE_BY: &str =
    "Could not bind UDP port {port}: {program} is using it. Change the port in settings.";
const BIND_IN_USE_BY_BOOTH: &str = "Could not bind UDP port {port}: another copy of Booth is using it. Close that copy, or change the port in settings.";
// No promise of a wait: the one time it was seen, a Bluetooth headset slow
// to close kept the port for over 30 s, and starting Booth again freed it.
const BIND_NOT_LET_GO: &str = "Could not bind UDP port {port}: the last room has not let go of it yet. Close Booth and start it again.";

pub fn in_use(port: u16, holder: &PortHolder) -> String {
    let text = match holder {
        PortHolder::ThisCopy => BIND_NOT_LET_GO,
        PortHolder::AnotherCopy => BIND_IN_USE_BY_BOOTH,
        PortHolder::Program(_) => BIND_IN_USE_BY,
        PortHolder::Unknown => BIND_IN_USE,
    };
    let text = text.replace("{port}", &port.to_string());
    match holder {
        PortHolder::Program(name) => text.replace("{program}", &isolated(name)),
        _ => text,
    }
}

pub fn room_error(err: &RoomError) -> String {
    match err {
        RoomError::Bind(bind) if err.is_in_use() => {
            let this_exe = std::env::current_exe().ok();
            let holder =
                running::port_holder(bind.holder(), std::process::id(), this_exe.as_deref());
            in_use(bind.port, &holder)
        }
        // Hyper-V and WSL set port ranges aside at boot, and Windows reports
        // a bind inside one as an access error that reads like permissions.
        RoomError::Bind(bind) if bind.kind == io::ErrorKind::PermissionDenied => format!(
            "Could not bind UDP port {}: Windows has set it aside, often for Hyper-V or WSL. Change the port in settings, to 41500 for example.",
            bind.port
        ),
        // BindError's own text names the step that failed, which is the
        // useful part when it is not the bind itself.
        RoomError::Bind(bind) => format!(
            "{} Try again, and restart Windows if it keeps happening.",
            sentence(&os_text(&bind.to_string()).replace("udp", "UDP"))
        ),
        RoomError::InviteExpired => {
            String::from("The invite has expired. Ask the host for a new one.")
        }
        RoomError::OwnInvite => String::from(
            "This invite was made on this PC. Send it to a friend and have them paste it.",
        ),
        RoomError::BadHostKey => String::from(
            "This invite carries a host key that cannot be used. Ask the host for a new invite.",
        ),
        RoomError::LocalAddresses(os) => format!(
            "Could not list this PC's network addresses. Windows said: {}. Try again, and restart Windows if it keeps happening.",
            os_text(&os.to_string())
        ),
        RoomError::Start(os) => format!(
            "Could not start the room's network threads. Windows said: {}. Close some programs and try again.",
            os_text(&os.to_string())
        ),
        RoomError::Log { path, source } => format!(
            "Could not open the log file {}. Windows said: {}. Start Booth without --log, or free some disk space and try again.",
            path.display(),
            os_text(&source.to_string())
        ),
    }
}

// Windows' own message ends with a full stop before the error number, which
// reads badly in the middle of a sentence.
fn os_text(text: &str) -> String {
    text.replace(". (os error", " (os error")
}

pub fn code_error(err: &CodeError) -> String {
    match err {
        CodeError::Empty => String::from(
            "Nothing was pasted. Copy the invite from the message you were sent and paste it here.",
        ),
        CodeError::OtherVersion { protocol, version } => format!(
            "{} {}",
            sentence(&err.to_string()),
            host_version_step(Some((*version, *protocol)))
        ),
        CodeError::Unversioned => {
            format!("{} {}", sentence(&err.to_string()), host_version_step(None))
        }
        other => sentence(&other.to_string()),
    }
}

// The host has another version, or with None a test build from before
// version numbers: whoever has the older one gets the newer.
fn host_version_step(host: Option<(Version, u16)>) -> String {
    if host.is_some_and(|host| host > (invite::VERSION, invite::PROTOCOL)) {
        format!("Get the same version as the host from {RELEASES_PAGE}.")
    } else {
        format!(
            "Ask the host to get Booth {} from {RELEASES_PAGE}.",
            invite::VERSION
        )
    }
}

pub fn copy_error(err: &arboard::Error) -> String {
    match err {
        arboard::Error::ClipboardOccupied => String::from(
            "Could not copy: another program is using the clipboard. Press Copy again.",
        ),
        other => format!(
            "Could not copy: {}. Press Copy again.",
            other.to_string().trim_end_matches('.')
        ),
    }
}

// `port` is the one this PC is bound to, which a forward would lead to.
pub fn router(state: RouterState, port: u16) -> String {
    let text = match state {
        RouterState::Testing => "Checking your router.",
        RouterState::Easy => {
            "Your router did not open a port. If a friend cannot connect, they will get a short code to send back to you."
        }
        RouterState::Hard => {
            return format!(
                "Your router changes ports for every connection, so codes sent back will not help. Forward UDP {port}, or use IPv6, Tailscale or WireGuard."
            );
        }
        RouterState::Unknown => "Could not test your router. Friends can still try.",
        RouterState::Mapped => "Your router says it opened the port.",
        RouterState::MappedVerified => {
            "Your router opened the port. Friends can reach you directly."
        }
        RouterState::SecondRouter => {
            "There is another router between you and the internet (your provider's, or a box in front of yours). A code sent back may still work; otherwise use IPv6, Tailscale or WireGuard, or bridge the front box."
        }
        RouterState::CarrierNat => {
            "This PC is behind your provider's shared address. Friends on the same network, Tailscale or WireGuard can join."
        }
    };
    String::from(text)
}

pub const MAPPED_SINCE: &str =
    "Your router opened the port after this invite was made. A new invite includes it.";
pub const ADDRESS_CHANGED_SINCE: &str = "Your address changed since this invite was made.";

// The host's side of an address change mid-session. The friends' side is in
// the notices.
pub const CODES_CANNOT_HELP: &str = "Your address changed. A code from a friend will not help; they need the new address (address name in settings) or a new invite.";

// With a name set, the friends did look it up; it led somewhere else.
pub fn friends_lost(changed: Option<AddressChanged>) -> Option<&'static str> {
    let changed = changed.filter(|changed| changed.friends_lost)?;
    Some(if changed.name_set {
        "Your address changed and friends could not follow. Check that your address name points to this PC."
    } else {
        "Your address changed and friends could not follow. Set an address name in settings."
    })
}

// The reply code screen on a friend's PC, and the paste field on the host's.
pub const SEND_CODE_BACK: &str = "Send this code back to the host:";
pub const CODE_SECOND_ROUTER: &str =
    "The host is behind a second router; this code is the one thing that can still work over IPv4.";
pub const CODE_EXPIRED: &str = "The code did not get through.";
pub const PASTE_HINT: &str = "Paste a code a friend sent back";
pub const PASTE_SENT: &str = "Sent. Your friend should get in within a few seconds.";

// What a forward on the host would need. The invite nearly always shows the
// port; when it does not, the words still point at the right thing.
fn forward(port: Option<u16>) -> String {
    match port {
        Some(port) => format!("UDP {port}"),
        None => String::from("Booth's UDP port"),
    }
}

// The line that takes the code's place, or none while the code is on show.
pub fn reply(state: ReplyState, port: Option<u16>) -> Option<String> {
    let port = forward(port);
    Some(match state {
        ReplyState::Code { .. } => return None,
        ReplyState::HostHard => format!(
            "The host's router changes ports for every connection. Ask them to forward {port}, or connect over IPv6, Tailscale or WireGuard."
        ),
        ReplyState::OwnHard => String::from(
            "Your router changes ports for every connection, so a code back will not help. Use IPv6, Tailscale or WireGuard.",
        ),
        ReplyState::NoAddress => format!(
            "Could not learn your outside address, so a code back cannot help. Ask the host to forward {port}, or connect over IPv6, Tailscale or WireGuard."
        ),
        ReplyState::Expired {
            second_router: false,
        } => format!(
            "Ask the host to turn on UPnP or forward {port}, or connect over IPv6, Tailscale or WireGuard."
        ),
        ReplyState::Expired {
            second_router: true,
        } => String::from(
            "There is another router in front of the host's, so the code could not get through. Connect over IPv6, Tailscale or WireGuard.",
        ),
    })
}

// The line under the paste field. None once the friend is in, since the
// person list says it.
pub fn paste(state: &PasteState) -> Option<String> {
    let refused = match state {
        PasteState::Sent => return Some(String::from(PASTE_SENT)),
        PasteState::Joined => return None,
        PasteState::Refused(refused) => refused,
    };
    Some(match refused {
        ReplyRefused::Expired => {
            String::from("This code has expired. Ask your friend for a new one.")
        }
        ReplyRefused::InviteNotLive => String::from(
            "This code answers an invite that is no longer open. Make a new invite and send it.",
        ),
        ReplyRefused::NotKnown => String::from(
            "This code is from a device that has not been in this room. Make a new invite and send it.",
        ),
        ReplyRefused::Blocked => {
            String::from("This code is from a device you blocked. Nothing was sent to it.")
        }
        ReplyRefused::FriendHard => String::from(
            "This friend's router changes ports too. The code cannot help; they need IPv6, Tailscale or WireGuard.",
        ),
        ReplyRefused::TooSoon => String::from("Wait a few seconds before pasting this code again."),
        ReplyRefused::AlreadyHere { name } => {
            format!("{} is already in the room.", isolated(name))
        }
        ReplyRefused::NoAddress => String::from(
            "This code has no address this PC can send to. Ask your friend for a new one.",
        ),
        ReplyRefused::BadCode(_) => String::from(
            "This code names an address Booth does not send to. Ask your friend for a new one.",
        ),
        ReplyRefused::AddressTaken { .. } => String::from(
            "Someone else here already uses the address in this code. Ask your friend for a new one.",
        ),
        ReplyRefused::HostHard => String::from(
            "Your router changes ports for every connection, so codes sent back will not help.",
        ),
        ReplyRefused::Closed => String::from("The room has closed."),
    })
}

// A paste that is not a reply code. Nothing at all is no mistake worth a
// line: Enter on an empty field does nothing.
pub fn reply_code_error(err: &CodeError) -> Option<String> {
    match err {
        CodeError::Empty => None,
        other => Some(sentence(&other.to_string())),
    }
}

pub const FIREWALL_ASK: &str = "Windows blocks incoming connections to new programs. One administrator prompt lets Booth receive UDP. Nothing else runs as administrator.";
pub const FIREWALL_STANDARD_USER: &str = "You are not an administrator on this PC, so the port cannot be opened. Ask an administrator, or join only. Without the rule you will not follow the host if their address changes.";
pub const FIREWALL_BLOCKED: &str =
    "Windows has a rule that blocks Booth, probably from an earlier prompt. Allow removes it.";
pub const FIREWALL_WAITING: &str = "Waiting for the administrator prompt.";
pub const FIREWALL_CANCELLED: &str = "The administrator prompt was closed. Friends may not reach you; Booth will ask again next time.";
pub const FIREWALL_STILL_BLOCKED: &str = "Booth added its firewall rule, but Windows still does not let it through. If a company or school manages this PC, ask them to allow Booth on UDP.";
pub const FIREWALL_BLOCKING_ALL: &str = "Windows is set to block all incoming connections on this network. Turn that off in Windows Security, under Firewall and network protection.";
pub const FIREWALL_MAY_ASK: &str =
    "Windows may ask about Booth when you host or join. Choose Allow, or friends cannot reach you.";
// The same line after Not now when Windows would not ask: a Block rule is
// already there, or this account cannot allow anything.
pub const FIREWALL_NOT_NOW_BLOCKED: &str =
    "Windows blocks Booth on this PC, so friends cannot reach you. Booth will ask again next time.";
pub const FIREWALL_NOT_NOW_STANDARD_USER: &str =
    "Without an administrator, friends cannot reach you here. You can still join a room.";

pub fn firewall_exit(code: u32) -> String {
    let again = "Restart Booth and press Allow again.";
    match Exit::from_code(code) {
        Some(Exit::AccessDenied) => String::from(
            "Windows did not let Booth change the firewall, even after the administrator prompt. If a company or school manages this PC, ask them to allow Booth on UDP.",
        ),
        Some(Exit::Reach) => format!(
            "Could not reach Windows Firewall to add the rule. Check that the Windows Defender Firewall service is running. {again}"
        ),
        Some(Exit::RemoveBlock) => String::from(
            "Could not remove the Windows rule that blocks Booth. Delete the Block rules for booth.exe under Inbound rules in Windows Defender Firewall with Advanced Security, then restart Booth.",
        ),
        Some(Exit::AddRule) => format!("Could not add the firewall rule for Booth. {again}"),
        // Done is not a failure and never gets here; the rest are a start
        // Booth did not make, or a crash, whose code is all there is.
        Some(Exit::Done | Exit::Usage | Exit::NoExePath) | None => {
            let code = if code > 0xFF {
                format!("0x{code:08X}")
            } else {
                code.to_string()
            };
            format!(
                "The firewall step stopped with code {code} and the rule was not added. {again}"
            )
        }
    }
}

// Also for a failure after the user said yes, such as an exe the
// administrator session cannot see, so it does not claim the prompt never
// opened.
pub fn firewall_prompt_error(err: &io::Error) -> String {
    format!(
        "Could not start the firewall step as administrator. Windows said: {}. Restart Booth and press Allow again.",
        os_text(&err.to_string())
    )
}

pub const RUNNING_ELEVATED: &str = "Booth was started as administrator. Start it again the usual way, not with Run as administrator. The firewall step asks for administrator by itself when it needs it.";

pub const ADDRESS_NAME_ABOUT: &str = "If you run dynamic DNS, put the name here so friends can find you again after your address changes. Nothing here updates it; your own dynamic DNS client does.";
// One sentence for every way a name can fail: the rules behind it (a number
// at the end, localhost) are not worth a lesson each on this screen.
pub const ADDRESS_NAME_REFUSED: &str = "That is not an address name Booth can use. Use letters, digits, hyphens and dots, like myroom.duckdns.org.";

// The lines under the settings fields.
pub const PORT_REFUSED: &str = "The port must be a number from 1024 to 65535.";
const STUN_REFUSED: &str =
    "Line {n} is not a server Booth can use. Write it as name:port, like stun.cloudflare.com:3478.";
pub const NO_STUN: &str = "With no STUN servers Booth cannot learn your outside address, so friends on the internet may not get in.";
// Under the level meter: the one time Booth opens the microphone outside a
// room, said where it happens.
pub const MICROPHONE_OPEN: &str = "The microphone is open while this screen shows the meter.";

pub const OPEN_MIC: &str = "Booth sends while it hears you, and a moment after.";
pub const CONSTANT_RATE_OFF: &str =
    "Off: packet sizes follow your speech, so anyone on the path can tell what language you speak.";

// Under the input device when the microphone makes voice worse by itself.
// The first loses "over Bluetooth hands-free" for a narrowband microphone
// that is not.
const MICROPHONE_THIN: &str = "This microphone runs at {rate} kHz over Bluetooth hands-free. Voice will sound thin and arrive later. A wired or USB headset is better.";
const MICROPHONE_HANDS_FREE: &str = "This microphone works through Bluetooth hands-free. Voice will arrive later. A wired or USB headset is better.";

pub fn microphone_warning(mic: Microphone) -> Option<String> {
    if mic.narrowband() {
        let text = MICROPHONE_THIN.replace("{rate}", &khz(mic.rate));
        Some(if mic.hands_free {
            text
        } else {
            text.replace(" over Bluetooth hands-free", "")
        })
    } else if mic.hands_free {
        Some(String::from(MICROPHONE_HANDS_FREE))
    } else {
        None
    }
}

// 8, 11.025 or 44.1: exact, and no zeros after the point.
pub fn khz(rate: u32) -> String {
    (f64::from(rate) / 1000.0).to_string()
}

pub fn stun_refused(line: usize) -> String {
    STUN_REFUSED.replace("{n}", &line.to_string())
}

pub const HOTKEYS_PAUSED: &str = "Hotkeys paused while an administrator window has focus.";
pub const PRESS_NEW_KEY: &str = "Press the new key, or Esc to keep the old one.";

pub fn hotkey_label(action: Action) -> &'static str {
    match action {
        Action::PushToTalk => "Push to talk",
        Action::Mute => "Mute",
        Action::Deafen => "Deafen",
        Action::Share => "Share (press twice) or stop sharing",
        Action::Panic => "Panic key",
        Action::ShowPanel => "Show or hide the panel",
        Action::StatsPanel => "Stats panel",
    }
}

pub fn key_used(other: Action) -> String {
    let what = match other {
        Action::Panic => "the panic key",
        Action::StatsPanel => "the stats panel",
        other => other.name(),
    };
    format!("That key is already used for {what}.")
}

pub fn wants_control(name: &str) -> String {
    format!("{} wants to control your screen.", isolated(name))
}

pub fn controlling_this_pc(name: &str) -> String {
    format!("{} is controlling this PC.", isolated(name))
}

pub fn panic_stops(key: &str) -> String {
    format!("{key} stops it at any time.")
}

// What a screen reader hears on a row that has a menu.
pub fn row_menu(name: &str) -> String {
    format!("{name}, Enter opens the row menu")
}

// While this PC is controlled.
pub const SETTINGS_LOCKED: &str = "The hotkeys, the panic key and the sharing settings cannot change while someone controls this PC. Stop control, then save again.";

// Nothing can read a new key or act on one; the buttons still work.
pub fn hotkeys_off(why: &str) -> String {
    format!(
        "Hotkeys could not start: {}. Use the buttons in your row, and start Booth again to try once more.",
        os_text(why).trim_end_matches('.')
    )
}

// A known host's row, opened.
pub const ADDRESS_OR_NAME: &str = "Address or name";
pub const MANUAL_HINT: &str = "For example 203.0.113.5:41000 or myroom.duckdns.org";
pub const MANUAL_REFUSED: &str = "That is not an address or name Booth can use. Write an address and port like 203.0.113.5:41000, or a name like myroom.duckdns.org.";
const FORGOT: &str = "Forgot {room}. A new invite is needed to join it again.";
const DAMAGED: &str = "The list of known {list} could not be read, so Booth started with an empty one. The old file is kept as {file}.";

pub fn forgot(room: &str) -> String {
    FORGOT.replace("{room}", room)
}

// A list left where it was (ListProblem::Unusable). Its error already says
// what happened and what to do.
pub fn unusable(why: &str) -> String {
    sentence(&os_text(why))
}

// A Remove or Unblock that could not be written, or whose list could not be
// read first.
pub fn list_error(err: &KnownError) -> String {
    let text = sentence(&os_text(&err.to_string()));
    match err {
        KnownError::Write(KeyError::Write { .. }) => {
            format!("{text} Close any program that has the file open, then press Save again.")
        }
        KnownError::Write(_) => format!("{text} Press Save again."),
        _ => text,
    }
}

// A second copy on the same data folder would write the same known lists
// from its own memory. Said only when the open copy's panel did not come
// forward in its place (running.rs).
pub const ALREADY_OPEN: &str =
    "Booth is already open on this PC. Use that window, or close it and start Booth again.";

pub fn damaged(damaged: &DamagedList) -> String {
    let list = match damaged.list {
        List::Hosts => "hosts",
        List::Devices => "devices",
    };
    DAMAGED
        .replace("{list}", list)
        .replace("{file}", &damaged.kept_as)
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

// "24 Sep 2026": no order of day and month to guess at.
pub fn date(year: u16, month: u16, day: u16) -> String {
    let month = MONTHS
        .get(usize::from(month).wrapping_sub(1))
        .copied()
        .unwrap_or("?");
    format!("{day} {month} {year}")
}

pub fn last_seen(unix: u64) -> String {
    match win::local_date(unix) {
        Some((year, month, day)) => format!("Last seen {}", date(year, month, day)),
        None => String::from("Last seen at a time Windows could not show"),
    }
}

pub fn settings_error(err: &SaveError) -> String {
    let close_it = "Close any program that has the file open, then press Save again.";
    match err {
        SaveError::Read { path, source } => format!(
            "Could not read {}, so saving now could lose what else is in it. Windows said: {}. {close_it}",
            path.display(),
            os_text(&source.to_string())
        ),
        // The path is the temporary file, which means nothing to anyone;
        // the folder is where to look.
        SaveError::Write { path, source } => format!(
            "Could not write the new settings in {}. Windows said: {}. Check that the disk is not full, then press Save again.",
            path.parent().unwrap_or(path).display(),
            os_text(&source.to_string())
        ),
        SaveError::Replace { path, source } => format!(
            "Could not replace {} with the new settings. Windows said: {}. {close_it}",
            path.display(),
            os_text(&source.to_string())
        ),
    }
}

pub const ANYONE: &str = "Anyone who sees this code can join until tomorrow.";
pub const EMPTY_ROOM: &str = "Send the invite to your friends.";
// In place of the invite code and the stats panel's address lines, from the
// ask until the capture has closed.
pub const HIDDEN_WHILE_SHARING: &str = "Hidden while you share";

pub const UPLOAD_ABOUT: &str =
    "Keep it under your internet upload speed, with room left for voice and the game.";
pub const VSYNC_ON: &str =
    "The picture waits for the monitor's refresh: no tearing, up to one refresh later.";
pub const HIDE_STRIP_ON: &str = "Moving the mouse brings it back.";
pub const HIDE_STRIP_STAYS: &str = "Animation effects are off in Windows, so the strip stays.";

// The name a screen reader hears for Share while someone else shares, which
// the button in ash cannot say by itself.
pub fn one_share(name: &str) -> String {
    format!("Share. {name} is sharing. One share at a time.")
}

// The placeholder is the verb and nothing more.
pub const MESSAGE_HINT: &str = "Message";
pub const MESSAGE_TOO_LONG: &str = "This message is too long. Split it in two.";

// The line under the composer. An empty message is not sent, and a
// lost host already shows in the strip and a disabled composer.
pub fn chat_refused(why: ChatRefused) -> Option<&'static str> {
    match why {
        ChatRefused::TooLong | ChatRefused::TooManyLines => Some(MESSAGE_TOO_LONG),
        ChatRefused::Empty | ChatRefused::NotLive => None,
    }
}

// "21:14", on a 24-hour clock whatever the locale says.
pub fn time_of_day(hour: u16, minute: u16) -> String {
    format!("{hour:02}:{minute:02}")
}

pub fn chat_time(at_unix_ms: u64) -> Option<String> {
    win::local_time(at_unix_ms / 1000).map(|(hour, minute)| time_of_day(hour, minute))
}

pub fn notice(notice: &Notice) -> String {
    let text = match notice {
        Notice::OtherVersion { protocol, version } => {
            let (host, this) = invite::two_versions(*version, *protocol);
            return format!(
                "The host has Booth {host} and you have {this}. {}",
                host_version_step(Some((*version, *protocol)))
            );
        }
        Notice::UnversionedHost => {
            return format!(
                "The host has a test build of Booth made before the first release. {}",
                host_version_step(None)
            );
        }
        Notice::StillTrying => "Could not reach the host yet. Still trying.",
        Notice::LostHost => {
            "Lost the host. If their address changed, ask for a new invite; the host can set an address name in settings so this does not happen again."
        }
        Notice::HostMoved => {
            "The host's address changed and Booth could not reach the new one. Ask for a new invite."
        }
        Notice::RoomClosed => "The host closed the room.",
        Notice::InviteExpired => "The invite has expired. Ask the host for a new one.",
        Notice::SocketFailed => {
            "Booth can no longer receive from the network, so the room has ended. Leave, then host or join again."
        }
    };
    String::from(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_in_use() {
        assert_eq!(key_used(Action::Mute), "That key is already used for mute.");
        assert_eq!(
            key_used(Action::ShowPanel),
            "That key is already used for show or hide the panel."
        );
        assert_eq!(
            key_used(Action::StatsPanel),
            "That key is already used for the stats panel."
        );
        assert_eq!(
            key_used(Action::Share),
            "That key is already used for share (press twice) or stop sharing."
        );
        assert_eq!(
            key_used(Action::Panic),
            "That key is already used for the panic key."
        );
        for action in Action::ALL {
            let label = hotkey_label(action);
            assert_eq!(label.to_lowercase(), action.name(), "{label}");
        }
    }

    #[test]
    fn hotkeys_off_line() {
        let text = hotkeys_off(
            "could not register for keyboard raw input: Access is denied. (os error 5)",
        );
        assert_eq!(
            text,
            "Hotkeys could not start: could not register for keyboard raw input: Access is denied (os error 5). Use the buttons in your row, and start Booth again to try once more."
        );
    }

    #[test]
    fn log_text_to_sentence() {
        assert_eq!(
            sentence("this code has expired; ask your friend for a new one"),
            "This code has expired. Ask your friend for a new one."
        );
        assert_eq!(sentence("no code was pasted"), "No code was pasted.");
        assert_eq!(sentence("Already a sentence."), "Already a sentence.");
    }

    #[test]
    fn code_errors() {
        for err in [
            CodeError::Empty,
            CodeError::NotACode,
            CodeError::IsReplyCode,
            CodeError::IsInvite,
            CodeError::NewerVersion,
            CodeError::OtherVersion {
                protocol: invite::PROTOCOL + 1,
                version: invite::VERSION,
            },
            CodeError::Unversioned,
            CodeError::Damaged,
            CodeError::Expired,
        ] {
            let text = code_error(&err);
            assert!(text.starts_with(char::is_uppercase), "{text}");
            assert!(text.ends_with('.'), "{text}");
            assert!(!text.contains(';'), "{text}");
        }
    }

    fn version(major: u16, minor: u16, patch: u16) -> Version {
        Version {
            major,
            minor,
            patch,
        }
    }

    // Both versions, and what to do: whoever has the older one gets the
    // newer.
    #[test]
    fn other_versions() {
        let this = invite::VERSION;
        let later = Version {
            major: this.major + 1,
            ..this
        };
        let newer = CodeError::OtherVersion {
            protocol: invite::PROTOCOL + 1,
            version: later,
        };
        assert_eq!(
            code_error(&newer),
            format!(
                "This invite is for Booth {later} and you have {this}. Get the same version as the host from {RELEASES_PAGE}."
            )
        );
        let older = CodeError::OtherVersion {
            protocol: invite::PROTOCOL + 1,
            version: version(0, 0, 1),
        };
        assert_eq!(
            code_error(&older),
            format!(
                "This invite is for Booth 0.0.1 and you have {this}. Ask the host to get Booth {this} from {RELEASES_PAGE}."
            )
        );
        assert_eq!(
            code_error(&CodeError::Unversioned),
            format!(
                "This invite is from a test build of Booth made before the first release. Ask the host to get Booth {this} from {RELEASES_PAGE}."
            )
        );
        assert_eq!(
            notice(&Notice::OtherVersion {
                protocol: invite::PROTOCOL + 1,
                version: later
            }),
            format!(
                "The host has Booth {later} and you have {this}. Get the same version as the host from {RELEASES_PAGE}."
            )
        );
        assert_eq!(
            notice(&Notice::UnversionedHost),
            format!(
                "The host has a test build of Booth made before the first release. Ask the host to get Booth {this} from {RELEASES_PAGE}."
            )
        );
    }

    #[test]
    fn empty_paste() {
        let text = code_error(&CodeError::Empty);
        assert!(text.contains("paste it here"), "{text}");
    }

    #[test]
    fn firewall_exits() {
        let failures = [
            Exit::Usage,
            Exit::NoExePath,
            Exit::Reach,
            Exit::AccessDenied,
            Exit::RemoveBlock,
            Exit::AddRule,
        ];
        let mut seen = Vec::new();
        for exit in failures {
            let text = firewall_exit(u32::from(exit.code()));
            assert!(text.starts_with(char::is_uppercase), "{text}");
            assert!(text.ends_with('.'), "{text}");
            assert!(!text.contains(';'), "{text}");
            seen.push(text);
        }
        for known in [Exit::Reach, Exit::AccessDenied, Exit::RemoveBlock] {
            let text = firewall_exit(u32::from(known.code()));
            assert_eq!(seen.iter().filter(|other| **other == text).count(), 1);
        }
    }

    #[test]
    fn crashed_helper_code() {
        let text = firewall_exit(0xC000_0005);
        assert!(text.contains("0xC0000005"), "{text}");
        assert!(firewall_exit(101).contains("code 101 "));
    }

    #[test]
    fn windows_text_mid_sentence() {
        let err = io::Error::from_raw_os_error(10055);
        let text = os_text(&err.to_string());
        assert!(!text.contains(". (os error"), "{text}");
        assert!(text.ends_with("(os error 10055)"), "{text}");
    }

    #[test]
    fn settings_errors() {
        let dir = std::path::Path::new(r"C:\Users\a\AppData\Local\Booth");
        let denied = || io::Error::from_raw_os_error(5);
        let failures = [
            SaveError::Read {
                path: dir.join("settings.txt"),
                source: denied(),
            },
            SaveError::Write {
                path: dir.join("settings.4242.tmp"),
                source: io::Error::from_raw_os_error(112),
            },
            SaveError::Replace {
                path: dir.join("settings.txt"),
                source: denied(),
            },
        ];
        let mut seen = Vec::new();
        for err in &failures {
            let text = settings_error(err);
            assert!(text.starts_with("Could not "), "{text}");
            assert!(text.ends_with("press Save again."), "{text}");
            assert!(!text.contains(". (os error"), "{text}");
            assert!(!seen.contains(&text), "{err} says the same as another");
            seen.push(text);
        }
        assert!(seen[1].contains(r"in C:\Users\a\AppData\Local\Booth."));
        assert!(!seen[1].contains(".tmp"), "{}", seen[1]);
    }

    #[test]
    fn reply_lines() {
        let states = [
            ReplyState::HostHard,
            ReplyState::OwnHard,
            ReplyState::NoAddress,
            ReplyState::Expired {
                second_router: false,
            },
            ReplyState::Expired {
                second_router: true,
            },
        ];
        let mut seen = Vec::new();
        for state in states {
            let text = reply(state, Some(41000)).expect("a line in place of the code");
            assert!(text.starts_with(char::is_uppercase), "{text}");
            assert!(text.ends_with('.') && !text.contains('!'), "{text}");
            assert!(!seen.contains(&text), "{state:?} says the same as another");
            seen.push(text);
        }
        for second_router in [false, true] {
            assert_eq!(reply(ReplyState::Code { second_router }, Some(41000)), None);
        }
        let host_hard = reply(ReplyState::HostHard, Some(41000)).unwrap();
        assert!(host_hard.contains("forward UDP 41000,"), "{host_hard}");
        let unknown_port = reply(ReplyState::NoAddress, None).unwrap();
        assert!(
            unknown_port.contains("forward Booth's UDP port,"),
            "{unknown_port}"
        );
    }

    #[test]
    fn paste_lines() {
        let refusals = [
            ReplyRefused::Closed,
            ReplyRefused::HostHard,
            ReplyRefused::Expired,
            ReplyRefused::InviteNotLive,
            ReplyRefused::NotKnown,
            ReplyRefused::Blocked,
            ReplyRefused::AlreadyHere {
                name: String::from("Tom"),
            },
            ReplyRefused::TooSoon,
            ReplyRefused::NoAddress,
            ReplyRefused::FriendHard,
            ReplyRefused::BadCode(invite::BuildError::BadAddress {
                addr: "192.168.1.20:52000".parse().unwrap(),
                reason: "it is a private address, not one the internet can reach",
            }),
            ReplyRefused::AddressTaken {
                addr: "203.0.113.9:52000".parse().unwrap(),
            },
        ];
        let mut seen = vec![paste(&PasteState::Sent).expect("a line for Sent")];
        for refused in refusals {
            let text = paste(&PasteState::Refused(refused.clone())).expect("a line");
            let first = text.trim_start_matches('\u{2068}');
            assert!(first.starts_with(char::is_uppercase), "{text}");
            assert!(text.ends_with('.') && !text.contains('!'), "{text}");
            assert!(
                !seen.contains(&text),
                "{refused:?} says the same as another"
            );
            seen.push(text);
        }
        assert_eq!(paste(&PasteState::Joined), None);
        let here = PasteState::Refused(ReplyRefused::AlreadyHere {
            name: String::from("Tom"),
        });
        assert_eq!(
            paste(&here).unwrap(),
            "\u{2068}Tom\u{2069} is already in the room."
        );
    }

    #[test]
    fn not_a_reply_code() {
        assert_eq!(reply_code_error(&CodeError::Empty), None);
        assert_eq!(
            reply_code_error(&CodeError::IsInvite).unwrap(),
            "This is an invite, not a reply code. Paste it in Join."
        );
    }

    #[test]
    fn router_lines() {
        let states = [
            RouterState::Testing,
            RouterState::Unknown,
            RouterState::Easy,
            RouterState::Hard,
            RouterState::Mapped,
            RouterState::MappedVerified,
            RouterState::SecondRouter,
            RouterState::CarrierNat,
        ];
        let mut seen = Vec::new();
        for state in states {
            let text = router(state, 41000);
            assert!(text.starts_with(char::is_uppercase), "{text}");
            assert!(text.ends_with('.') && !text.contains('!'), "{text}");
            assert!(!seen.contains(&text), "{state:?} says the same as another");
            seen.push(text);
        }
        let hard = router(RouterState::Hard, 41000);
        assert!(hard.contains("Forward UDP 41000,"), "{hard}");
    }

    fn changed(codes_cannot_help: bool, friends_lost: bool, name_set: bool) -> AddressChanged {
        AddressChanged {
            codes_cannot_help,
            friends_lost,
            name_set,
        }
    }

    // Sentence case, a full stop at the end, plain ASCII, no exclamation mark
    // and no doubled space. The marks around a friend's name do not count.
    fn assert_sentences(text: &str) {
        let bare = text.replace(['\u{2068}', '\u{2069}'], "");
        assert!(bare.is_ascii(), "not plain ASCII: {text}");
        assert!(!bare.contains('!') && !bare.contains("  "), "{text}");
        assert!(bare.ends_with('.'), "no full stop at the end: {text}");
        for start in bare.split(". ") {
            assert!(
                start.starts_with(|c: char| c.is_ascii_uppercase()),
                "no capital: {text}"
            );
        }
    }

    #[test]
    fn address_change_lines() {
        let moved = notice(&Notice::HostMoved);
        let lost = notice(&Notice::LostHost);
        let sentences = [
            moved.as_str(),
            lost.as_str(),
            CODES_CANNOT_HELP,
            ADDRESS_CHANGED_SINCE,
            friends_lost(Some(changed(false, true, false))).unwrap(),
            friends_lost(Some(changed(false, true, true))).unwrap(),
        ];
        for sentence in sentences {
            assert_sentences(sentence);
        }
    }

    #[test]
    fn known_hosts_and_settings_lines() {
        let hosts = damaged(&DamagedList {
            list: List::Hosts,
            kept_as: String::from("hosts.bin.bad"),
        });
        for sentence in [
            MANUAL_REFUSED,
            FORGOT,
            hosts.as_str(),
            PORT_REFUSED,
            STUN_REFUSED,
            NO_STUN,
        ] {
            assert_sentences(sentence);
        }
        // A placeholder, not a sentence, so no full stop.
        assert!(MANUAL_HINT.starts_with("For example "), "{MANUAL_HINT}");
        assert!(MANUAL_HINT.is_ascii() && !MANUAL_HINT.ends_with('.'));
    }

    #[test]
    fn port_in_use() {
        for sentence in [
            BIND_IN_USE,
            BIND_IN_USE_BY,
            BIND_IN_USE_BY_BOOTH,
            BIND_NOT_LET_GO,
            ALREADY_OPEN,
        ] {
            assert_sentences(sentence);
        }
        assert_eq!(
            in_use(41000, &PortHolder::Unknown),
            "Could not bind UDP port 41000: another program is using it. Change the port in settings."
        );
        assert_eq!(
            in_use(41000, &PortHolder::Program(String::from("steam.exe"))),
            "Could not bind UDP port 41000: \u{2068}steam.exe\u{2069} is using it. Change the port in settings."
        );
        assert_eq!(
            in_use(41500, &PortHolder::AnotherCopy),
            "Could not bind UDP port 41500: another copy of Booth is using it. Close that copy, or change the port in settings."
        );
        assert_eq!(
            in_use(41000, &PortHolder::ThisCopy),
            "Could not bind UDP port 41000: the last room has not let go of it yet. Close Booth and start it again."
        );
    }

    #[test]
    fn microphone_warnings() {
        for sentence in [MICROPHONE_THIN, MICROPHONE_HANDS_FREE] {
            assert_sentences(sentence);
        }
        let mic = |rate, hands_free| microphone_warning(Microphone { rate, hands_free });
        assert_eq!(
            mic(8_000, true).as_deref(),
            Some(
                "This microphone runs at 8 kHz over Bluetooth hands-free. Voice will sound thin and arrive later. A wired or USB headset is better."
            )
        );
        assert_eq!(
            mic(11_025, false).as_deref(),
            Some(
                "This microphone runs at 11.025 kHz. Voice will sound thin and arrive later. A wired or USB headset is better."
            )
        );
        assert_eq!(mic(16_000, true).as_deref(), Some(MICROPHONE_HANDS_FREE));
        assert_eq!(mic(48_000, true).as_deref(), Some(MICROPHONE_HANDS_FREE));
        assert_eq!(mic(16_000, false), None);
        assert_eq!(mic(44_100, false), None);
        assert_eq!(mic(48_000, false), None);
    }

    // Said under your row (screens/room.rs, voice_problem) while a new room
    // waits for the last room's headset to let go.
    #[test]
    fn device_still_closing() {
        use voice::audio::{AudioError, Direction};
        let closing = |direction, default| {
            sentence(&AudioError::StillClosing { direction, default }.to_string())
        };
        let lines = [
            closing(Direction::Input, true),
            closing(Direction::Output, true),
            closing(Direction::Input, false),
            closing(Direction::Output, false),
        ];
        for line in &lines {
            assert_sentences(line);
        }
        assert_eq!(
            lines[0],
            "The last room's microphone is still closing. It opens here once it lets go, or choose another microphone in Settings, System, Sound."
        );
        assert_eq!(
            lines[3],
            "The last room's speakers are still closing. They open here once they let go."
        );
    }

    #[test]
    fn rates_in_khz() {
        assert_eq!(khz(8_000), "8");
        assert_eq!(khz(11_025), "11.025");
        assert_eq!(khz(16_000), "16");
        assert_eq!(khz(44_100), "44.1");
        assert_eq!(khz(48_000), "48");
    }

    #[test]
    fn known_list_lines() {
        assert_eq!(
            forgot("Tuesday night"),
            "Forgot Tuesday night. A new invite is needed to join it again."
        );
        let devices = damaged(&DamagedList {
            list: List::Devices,
            kept_as: String::from("devices.bin.bad2"),
        });
        assert_eq!(
            devices,
            "The list of known devices could not be read, so Booth started with an empty one. The old file is kept as devices.bin.bad2."
        );
        assert_eq!(
            stun_refused(3),
            "Line 3 is not a server Booth can use. Write it as name:port, like stun.cloudflare.com:3478."
        );
    }

    #[test]
    fn unusable_list() {
        let path = std::path::PathBuf::from(r"C:\Users\a\AppData\Local\Booth\devices.bin");
        let write = KnownError::Write(KeyError::Write {
            file: keys::FileKind::Protected,
            path: path.clone(),
            source: io::Error::from_raw_os_error(5),
        });
        let text = list_error(&write);
        assert!(
            text.starts_with(r"Could not save C:\Users\a\AppData\Local\Booth\devices.bin: "),
            "{text}"
        );
        assert!(
            text.ends_with("Close any program that has the file open, then press Save again."),
            "{text}"
        );
        assert!(!text.contains(". (os error"), "{text}");

        let read = KnownError::Read {
            list: List::Devices,
            source: KeyError::Read {
                file: keys::FileKind::Protected,
                path,
                source: io::Error::from_raw_os_error(32),
            },
        };
        let text = list_error(&read);
        assert!(
            text.ends_with("Close any program that has it open, then start Booth again."),
            "{text}"
        );
        assert!(!text.contains(';'), "{text}");
        // The start screen says the same.
        let Some(room::ListProblem::Unusable { why, .. }) = read.problem() else {
            panic!("an unreadable list is a line on the start screen");
        };
        assert_eq!(unusable(&why), text);
    }

    #[test]
    fn dates() {
        assert_eq!(date(2026, 9, 24), "24 Sep 2026");
        assert_eq!(date(2027, 1, 1), "1 Jan 2027");
        assert_eq!(date(2027, 12, 31), "31 Dec 2027");
        assert_eq!(date(2027, 13, 1), "1 ? 2027");
        // Noon UTC on 24 Sep. Windows' zones run from UTC-12 to UTC+14, so
        // on this PC's calendar that is the 24th or, from UTC+12 on (New
        // Zealand, Fiji, Tonga, Samoa, Kiribati), already the 25th.
        let text = last_seen(1_790_251_200);
        assert!(
            ["Last seen 24 Sep 2026", "Last seen 25 Sep 2026"].contains(&text.as_str()),
            "{text}"
        );
    }

    #[test]
    fn message_too_long() {
        // The same sentence for both limits.
        for why in [ChatRefused::TooLong, ChatRefused::TooManyLines] {
            assert_eq!(
                chat_refused(why),
                Some("This message is too long. Split it in two.")
            );
        }
        assert_eq!(chat_refused(ChatRefused::Empty), None);
        assert_eq!(chat_refused(ChatRefused::NotLive), None);
    }

    #[test]
    fn chat_times() {
        assert_eq!(time_of_day(21, 14), "21:14");
        assert_eq!(time_of_day(9, 5), "09:05");
        assert_eq!(time_of_day(0, 0), "00:00");
        // Noon UTC on 24 Sep, which is a whole or half hour off on this PC's
        // clock in every zone Windows has but a few, so the minutes are
        // 00, 15, 30 or 45 and the form is HH:MM.
        let text = chat_time(1_790_251_200_000).expect("Windows says");
        assert_eq!(text.len(), 5, "{text}");
        assert_eq!(&text[2..3], ":", "{text}");
        assert!(["00", "15", "30", "45"].contains(&&text[3..]), "{text}");
    }

    #[test]
    fn friends_lost_line() {
        assert_eq!(friends_lost(None), None);
        // Friends still reconnecting is the paste field's line, not this.
        assert_eq!(friends_lost(Some(changed(true, false, false))), None);
        assert_eq!(friends_lost(Some(changed(true, false, true))), None);
        let no_name = friends_lost(Some(changed(true, true, false))).unwrap();
        assert!(no_name.ends_with("Set an address name in settings."));
        let named = friends_lost(Some(changed(false, true, true))).unwrap();
        assert!(named.ends_with("Check that your address name points to this PC."));
    }

    #[test]
    fn notice_lines() {
        let notices = [
            Notice::StillTrying,
            Notice::LostHost,
            Notice::HostMoved,
            Notice::RoomClosed,
            Notice::InviteExpired,
            Notice::SocketFailed,
            Notice::OtherVersion {
                protocol: invite::PROTOCOL + 1,
                version: invite::VERSION,
            },
            Notice::UnversionedHost,
        ];
        let mut seen = Vec::new();
        for shown in &notices {
            let text = notice(shown);
            assert!(text.starts_with(char::is_uppercase), "{text}");
            assert!(text.ends_with('.') && !text.contains('!'), "{text}");
            assert!(!seen.contains(&text), "{shown:?} says the same as another");
            seen.push(text);
        }
    }
}
