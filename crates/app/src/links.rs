use std::ffi::OsString;

use zeroize::Zeroizing;

use crate::elevated;

pub const FLAG: &str = "--link";
pub const LINK_BYTES: usize = 2048;
pub const MESSAGE_TAG: usize = 0x424c4e4b;

pub enum RunKind {
    Helper,
    Link(Zeroizing<String>),
    Refused,
    Panel,
}

pub fn run_kind(args: &[OsString]) -> RunKind {
    if elevated::asked_for(args) {
        return RunKind::Helper;
    }
    if !args.iter().any(|arg| arg == FLAG) {
        return RunKind::Panel;
    }
    if let [flag, text] = args
        && flag == FLAG
        && let Some(text) = text.to_str().filter(|text| text.len() <= LINK_BYTES)
    {
        return RunKind::Link(Zeroizing::new(text.to_owned()));
    }
    RunKind::Refused
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::ffi::OsStringExt;
    use std::process::ExitCode;

    fn args(words: &[&str]) -> Vec<OsString> {
        words.iter().map(OsString::from).collect()
    }

    #[test]
    fn link_run_kind() {
        assert!(matches!(run_kind(&[]), RunKind::Panel));
        assert!(matches!(
            run_kind(&args(&["--profile", "friend"])),
            RunKind::Panel
        ));
        let RunKind::Link(link) = run_kind(&args(&[FLAG, "booth://join/booth1-abc"])) else {
            panic!("one link is a link run");
        };
        assert_eq!(link.as_str(), "booth://join/booth1-abc");
        for words in [
            vec![FLAG],
            vec!["--profile", "evil", FLAG, "booth1-secret"],
            vec![FLAG, "booth1-secret", "--profile", "evil"],
            vec![FLAG, "booth1-secret", FLAG, "booth1-other"],
        ] {
            assert!(matches!(run_kind(&args(&words)), RunKind::Refused));
        }
        assert!(matches!(
            run_kind(&[FLAG.into(), OsString::from_wide(&[0xd800])]),
            RunKind::Refused
        ));
        assert!(matches!(
            run_kind(&args(&[FLAG, &"x".repeat(2049)])),
            RunKind::Refused
        ));
        assert!(matches!(
            run_kind(&args(&[FLAG, &"x".repeat(2048)])),
            RunKind::Link(_)
        ));
        assert!(matches!(
            run_kind(&args(&[FLAG, &"\u{e9}".repeat(1025)])),
            RunKind::Refused
        ));
    }

    #[test]
    fn helper_precedes_link_runs() {
        for words in [
            vec![elevated::FLAG],
            vec![FLAG, "booth1-secret", elevated::FLAG],
            vec![elevated::FLAG, FLAG, "booth1-secret"],
        ] {
            assert!(matches!(run_kind(&args(&words)), RunKind::Helper));
        }
        assert_eq!(
            elevated::run(&args(&[FLAG, "booth1-secret", elevated::FLAG])),
            ExitCode::from(2)
        );
    }

    #[test]
    fn the_registrations_agree() {
        let (scheme, _) = invite::LINK_PREFIX.split_once("://").unwrap();
        let installer = include_str!("../../../tools/booth.iss");
        let key = format!("Root: HKA; Subkey: \"Software\\Classes\\{scheme}");
        for wanted in [
            format!(
                "{key}\"; ValueType: string; ValueName: \"URL Protocol\"; ValueData: \"\"; Tasks: links"
            ),
            format!(
                "{key}\\shell\\open\\command\"; ValueType: string; ValueName: \"\"; ValueData: \"\"\"{{app}}\\booth.exe\"\" {FLAG} \"\"%1\"\"\"; Tasks: links"
            ),
        ] {
            assert!(installer.lines().any(|line| line == wanted), "{wanted}");
        }
        assert!(installer.contains(&format!(
            "Name: \"links\"; Description: \"Open {scheme}:// invite links with Booth\""
        )));
    }
}
