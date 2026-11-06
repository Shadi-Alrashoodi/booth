use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::process::Command;

#[test]
fn malformed_link_runs_exit_without_the_invite_text() {
    let code = OsString::from("booth://join/booth1-private");
    for args in [
        vec![OsString::from("--link")],
        vec![
            "--profile".into(),
            "evil".into(),
            "--link".into(),
            code.clone(),
        ],
        vec![
            "--link".into(),
            code.clone(),
            "--profile".into(),
            "evil".into(),
        ],
        vec!["--link".into(), code.clone(), "--link".into(), code.clone()],
        vec!["--link".into(), OsString::from_wide(&[0xd800])],
        vec!["--link".into(), "x".repeat(2049).into()],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_booth"))
            .args(args)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        assert!(output.stderr.is_empty());
    }
}

#[test]
fn a_link_cannot_add_a_firewall_rule() {
    let output = Command::new(env!("CARGO_BIN_EXE_booth"))
        .args([
            "--link",
            "booth://join/booth1-private",
            "--add-firewall-rule",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let expected = if cfg!(feature = "store") {
        "booth: the Store package brings its own firewall rule, so --add-firewall-rule does nothing in this copy"
    } else {
        "booth: --add-firewall-rule takes nothing else, not even another option"
    };
    assert_eq!(String::from_utf8(output.stderr).unwrap().trim(), expected);
}
