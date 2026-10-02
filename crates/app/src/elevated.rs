// booth.exe --add-firewall-rule: the one code path that runs as
// administrator. The panel starts it through the UAC prompt and reads its
// exit code. It opens no window and touches no profile, key, font or room.
// It acts on its own exe path only and takes nothing else from its
// arguments.

use std::ffi::OsString;
use std::process::ExitCode;

use net::firewall::{self, FirewallError};

pub const FLAG: &str = "--add-firewall-rule";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    Done,
    Usage,
    NoExePath,
    Reach,
    AccessDenied,
    RemoveBlock,
    AddRule,
}

impl Exit {
    const ALL: [Exit; 7] = [
        Exit::Done,
        Exit::Usage,
        Exit::NoExePath,
        Exit::Reach,
        Exit::AccessDenied,
        Exit::RemoveBlock,
        Exit::AddRule,
    ];

    // Clear of 1, which any crash or failed start can produce, and of 101,
    // a Rust panic, so a number the parent knows means this step said it.
    pub fn code(self) -> u8 {
        match self {
            Exit::Done => 0,
            Exit::Usage => 2,
            Exit::NoExePath => 20,
            Exit::Reach => 21,
            Exit::AccessDenied => 22,
            Exit::RemoveBlock => 23,
            Exit::AddRule => 24,
        }
    }

    pub fn from_code(code: u32) -> Option<Exit> {
        Exit::ALL
            .into_iter()
            .find(|exit| u32::from(exit.code()) == code)
    }

    fn from_error(err: &FirewallError) -> Exit {
        match err {
            FirewallError::Reach(_) => Exit::Reach,
            FirewallError::AccessDenied => Exit::AccessDenied,
            FirewallError::RemoveBlock(_) => Exit::RemoveBlock,
            FirewallError::AddRule(_) => Exit::AddRule,
        }
    }
}

// The flag anywhere means this run is the helper, so a mistyped start never
// falls through to opening the panel with administrator rights.
pub fn asked_for(args: &[OsString]) -> bool {
    args.iter().any(|arg| arg == FLAG)
}

pub fn run(args: &[OsString]) -> ExitCode {
    let exit = if args.len() == 1 {
        add_rule()
    } else {
        eprintln!("booth: {FLAG} takes nothing else, not even another option");
        Exit::Usage
    };
    ExitCode::from(exit.code())
}

fn add_rule() -> Exit {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(err) => {
            eprintln!("booth: could not find this exe's own path: {err}");
            return Exit::NoExePath;
        }
    };
    match firewall::add_rule_for(&exe) {
        Ok(()) => Exit::Done,
        Err(err) => {
            eprintln!("booth: {err}");
            Exit::from_error(&err)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    #[test]
    fn only_the_bare_flag_runs_the_helper() {
        assert!(asked_for(&args(&[FLAG])));
        assert_eq!(run(&args(&[FLAG, "--log"])), ExitCode::from(2));
        assert_eq!(run(&args(&[FLAG, FLAG])), ExitCode::from(2));
        assert_eq!(
            run(&args(&[FLAG, r"C:\Windows\notepad.exe"])),
            ExitCode::from(2)
        );
        assert_eq!(run(&args(&["--profile", "a", FLAG])), ExitCode::from(2));
    }

    #[test]
    fn the_flag_anywhere_keeps_the_panel_closed() {
        assert!(asked_for(&args(&["--port", "41000", FLAG])));
        assert!(!asked_for(&args(&[])));
        assert!(!asked_for(&args(&["--log"])));
        assert!(!asked_for(&args(&["--add-firewall-rule=x"])));
        assert!(!asked_for(&args(&["--ADD-FIREWALL-RULE"])));
    }

    #[test]
    fn exit_codes_are_distinct_and_read_back() {
        for exit in Exit::ALL {
            assert_eq!(Exit::from_code(u32::from(exit.code())), Some(exit));
        }
        let mut codes: Vec<u8> = Exit::ALL.iter().map(|exit| exit.code()).collect();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), Exit::ALL.len());
        assert_eq!(Exit::Done.code(), 0);
        for foreign in [1, 101, 0xC000_0005] {
            assert_eq!(Exit::from_code(foreign), None);
        }
    }

    #[test]
    fn every_firewall_error_has_its_own_code() {
        let text = String::from("x");
        assert_eq!(
            Exit::from_error(&FirewallError::Reach(text.clone())),
            Exit::Reach
        );
        assert_eq!(
            Exit::from_error(&FirewallError::AccessDenied),
            Exit::AccessDenied
        );
        assert_eq!(
            Exit::from_error(&FirewallError::RemoveBlock(text.clone())),
            Exit::RemoveBlock
        );
        assert_eq!(
            Exit::from_error(&FirewallError::AddRule(text)),
            Exit::AddRule
        );
    }
}
