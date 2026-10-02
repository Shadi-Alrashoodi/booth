use voice::audio::{Format, Microphone, Sample, hands_free, plan};

fn engine(rate: u32, channels: u16) -> Format {
    Format {
        rate,
        channels,
        sample: Sample::F32,
        channel_mask: 0,
    }
}

fn microphone(format: &Format, enumerator: Option<&str>, name: &str) -> Microphone {
    Microphone::new(format, hands_free(enumerator, Some(name)))
}

#[test]
fn which_microphones_warn() {
    // My AirPods in the first real call, 2026-09-27: narrowband
    // hands-free, and the name Windows 11 gives them says nothing of it.
    let airpods = engine(8_000, 1);
    let mic = microphone(
        &airpods,
        Some("BTHHFENUM"),
        "Headset (Shadi\u{2019}s AirPods Pro #2 - Find My)",
    );
    assert_eq!(
        mic,
        Microphone {
            rate: 8_000,
            hands_free: true
        }
    );
    assert!(mic.narrowband() && mic.warns());
    assert!(plan(&airpods).resampled);

    // Wideband hands-free: not thin, still late.
    let wideband = microphone(
        &engine(16_000, 1),
        Some("BTHHFENUM"),
        "Headset (Jabra Evolve2 65)",
    );
    assert!(!wideband.narrowband());
    assert!(wideband.hands_free && wideband.warns());

    let usb = microphone(&engine(48_000, 2), Some("USB"), "Microphone (Blue Yeti)");
    assert!(!usb.warns());
    assert!(!plan(&engine(48_000, 2)).resampled);

    // Resampled by Windows, which costs the small period; the stats panel
    // shows that as the period. The microphone itself is fine.
    let onboard = engine(44_100, 2);
    let mic = microphone(&onboard, Some("HDAUDIO"), "Microphone (Realtek(R) Audio)");
    assert!(plan(&onboard).resampled);
    assert!(!mic.narrowband() && !mic.warns());

    // Narrowband without Bluetooth, such as an old USB headset at 8 kHz.
    let old = microphone(
        &engine(8_000, 1),
        Some("USB"),
        "Headset Microphone (C-Media USB)",
    );
    assert!(!old.hands_free && old.warns());
    assert!(microphone(&engine(15_999, 1), Some("USB"), "Microphone").narrowband());
    assert!(!microphone(&engine(16_000, 1), Some("USB"), "Microphone").warns());
}

#[test]
fn the_enumerator_decides_and_the_name_is_only_a_fallback() {
    assert!(hands_free(Some("BTHHFENUM"), None));
    assert!(hands_free(Some("bthhfenum"), Some("Headset (AirPods)")));
    // A2DP headphones, USB, onboard sound and software devices.
    for enumerator in ["BTHENUM", "USB", "HDAUDIO", "SWD"] {
        assert!(
            !hands_free(Some(enumerator), Some("Headset (AirPods)")),
            "{enumerator}"
        );
    }
    // A name that says hands-free does not outvote the enumerator.
    assert!(!hands_free(
        Some("USB"),
        Some("Speakerphone Hands-Free (USB)")
    ));

    // No enumerator, or a blank one: the name as Windows 10 and older
    // Bluetooth stacks write it.
    assert!(hands_free(
        None,
        Some("Headset (WH-1000XM4 Hands-Free AG Audio)")
    ));
    assert!(hands_free(
        Some("  "),
        Some("Headset Microphone (Plantronics Handsfree)")
    ));
    // A wired headset jack is called a headset as well.
    assert!(!hands_free(
        None,
        Some("Headset Microphone (Realtek(R) Audio)")
    ));
    assert!(!hands_free(None, None));
}
