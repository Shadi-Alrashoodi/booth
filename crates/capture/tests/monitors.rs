#[test]
fn this_pc_has_sane_monitors() {
    let adapters = capture::adapters().unwrap();
    for adapter in &adapters {
        println!(
            "adapter: {}, vendor {:#06x}, device {:#06x}, luid {:#x}",
            adapter.description, adapter.vendor_id, adapter.device_id, adapter.luid
        );
    }
    let monitors = capture::monitors().unwrap();
    for monitor in &monitors {
        println!("monitor: {monitor}");
    }
    if adapters.is_empty() {
        println!("skipped: this PC has no hardware graphics adapter");
        return;
    }
    assert!(!monitors.is_empty(), "no monitor attached to the desktop");
    assert!(
        monitors.iter().filter(|m| m.primary).count() <= 1,
        "more than one primary monitor"
    );
    for monitor in &monitors {
        assert!(!monitor.name.is_empty());
        assert!(monitor.id.device_name.starts_with(r"\\.\DISPLAY"));
        assert!(monitor.width >= 320 && monitor.width <= 16384, "{monitor}");
        assert!(
            monitor.height >= 200 && monitor.height <= 16384,
            "{monitor}"
        );
        assert!(
            monitor.refresh_hz == 0.0 || (20.0..=1000.0).contains(&monitor.refresh_hz),
            "{monitor}"
        );
        assert!(adapters.contains(&monitor.adapter), "{monitor}");
    }
}
