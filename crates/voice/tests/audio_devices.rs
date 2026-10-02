use voice::audio::{AudioError, Choice, DeviceList, Direction, Endpoint, Resolved};

fn endpoint(id: &str, name: Option<&str>, active: bool) -> Endpoint {
    Endpoint {
        id: id.to_owned(),
        name: name.map(str::to_owned),
        active,
    }
}

// As this PC reported them on 2026-09-26, and a few things Windows can do.
fn outputs() -> Vec<Endpoint> {
    vec![
        endpoint(
            "{0.0.0.00000000}.{5b321707-8e8b-434d-99ae-8184db26e6ca}",
            Some("Headphones (AirPods Pro)"),
            true,
        ),
        endpoint(
            "{0.0.0.00000000}.{6527a832-3f63-4688-9d67-99b415aca6b8}",
            Some("PG32UCDMR (NVIDIA High Definition Audio)"),
            true,
        ),
        endpoint("{0.0.0.00000000}.{unplugged}", Some("Speakers"), false),
        endpoint(
            "{0.0.0.00000000}.{a740f3a6-3901-4723-8a6f-a782d19da0fb}",
            Some("  Realtek Digital Output (Realtek(R) Audio) "),
            true,
        ),
        endpoint("{0.0.0.00000000}.{no name}", None, true),
        endpoint("{0.0.0.00000000}.{blank}", Some("   "), true),
        // Listed twice; kept once.
        endpoint(
            "{0.0.0.00000000}.{6527a832-3f63-4688-9d67-99b415aca6b8}",
            Some("PG32UCDMR again"),
            true,
        ),
        endpoint("", Some("No id"), true),
    ]
}

#[test]
fn lists_openable_devices_in_windows_order() {
    let default = "{0.0.0.00000000}.{6527a832-3f63-4688-9d67-99b415aca6b8}";
    let list = DeviceList::from_endpoints(outputs(), Some(default));
    let names: Vec<&str> = list.devices.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "Headphones (AirPods Pro)",
            "PG32UCDMR (NVIDIA High Definition Audio)",
            "Realtek Digital Output (Realtek(R) Audio)",
            "Unnamed device",
            "Unnamed device",
        ]
    );
    let defaults: Vec<&str> = list
        .devices
        .iter()
        .filter(|d| d.is_default)
        .map(|d| d.name.as_str())
        .collect();
    assert_eq!(defaults, ["PG32UCDMR (NVIDIA High Definition Audio)"]);
    assert_eq!(list.default_device().unwrap().id, default);
}

#[test]
fn choices_resolve_against_what_is_connected() {
    let airpods = "{0.0.0.00000000}.{5b321707-8e8b-434d-99ae-8184db26e6ca}";
    let list = DeviceList::from_endpoints(outputs(), Some(airpods));
    match list.resolve(&Choice::Default) {
        Resolved::Device(device) => assert_eq!(device.id, airpods),
        other => panic!("{other:?}"),
    }
    let realtek = Choice::Device(String::from(
        "{0.0.0.00000000}.{a740f3a6-3901-4723-8a6f-a782d19da0fb}",
    ));
    match list.resolve(&realtek) {
        Resolved::Device(device) => assert!(device.name.starts_with("Realtek")),
        other => panic!("{other:?}"),
    }
    // Unplugged, or never here.
    let unplugged = Choice::Device(String::from("{0.0.0.00000000}.{unplugged}"));
    assert_eq!(list.resolve(&unplugged), Resolved::Missing);
    assert_eq!(
        list.resolve(&Choice::Device(String::from("gone"))),
        Resolved::Missing
    );
}

#[test]
fn a_default_that_is_not_active_is_no_default() {
    let list = DeviceList::from_endpoints(outputs(), Some("{0.0.0.00000000}.{unplugged}"));
    assert!(list.default_device().is_none());
    assert!(list.devices.iter().all(|d| !d.is_default));
    assert_eq!(list.resolve(&Choice::Default), Resolved::NoDevice);
    let empty = DeviceList::from_endpoints(Vec::new(), None);
    assert_eq!(empty.resolve(&Choice::Default), Resolved::NoDevice);
}

#[test]
fn an_empty_id_in_settings_is_windows_default() {
    assert_eq!(Choice::from_id(""), Choice::Default);
    assert_eq!(Choice::from_id("   "), Choice::Default);
    let chosen = Choice::from_id(" {0.0.1.00000000}.{1ebb} ");
    assert_eq!(
        chosen,
        Choice::Device(String::from("{0.0.1.00000000}.{1ebb}"))
    );
    assert_eq!(chosen.id(), Some("{0.0.1.00000000}.{1ebb}"));
    assert_eq!(Choice::Default.id(), None);
}

#[test]
fn errors_say_what_happened_and_what_to_do() {
    assert_eq!(
        AudioError::AccessDenied(Direction::Input).to_string(),
        "could not open the microphone: Windows says access is denied. Allow microphone access for desktop apps in Settings, Privacy and security, Microphone"
    );
    assert_eq!(
        AudioError::NotConnected {
            direction: Direction::Output,
            name: Some(String::from("Realtek Digital Output")),
        }
        .to_string(),
        "could not open the output device: Realtek Digital Output is not connected. Connect it, or choose another device in Booth's settings"
    );
    let windows = AudioError::Windows {
        step: String::from("start the microphone (Headset)"),
        code: 0x8889_0017,
        text: String::from("The audio engine is out of time"),
    };
    assert_eq!(
        windows.to_string(),
        "could not start the microphone (Headset): The audio engine is out of time (0x88890017). Try again, and restart Windows if it keeps happening"
    );
    assert_eq!(windows.direction(), None);
    assert_eq!(
        AudioError::NoDevice(Direction::Input).direction(),
        Some(Direction::Input)
    );
}
