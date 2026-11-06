// What the panel draws on. eframe picks it once, as the window opens, and the
// panel keeps it for the run. While a graphics driver installs the card is in
// no adapter list, and the panel lands on the Microsoft Basic Render Driver,
// which draws every repaint on the CPU's cores.

use std::fmt::Display;
use std::time::{Duration, Instant};

use capture::{Adapter, CaptureError};
use eframe::wgpu::{AdapterInfo, DeviceType};

// Soon enough after a driver install, and each look is one DXGI factory and
// its list.
pub const LOOK_EVERY: Duration = Duration::from_secs(5);

// The Basic Render Driver by its ids, as the viewer and capture know it too.
const BASIC_RENDER: (u32, u32) = (0x1414, 0x8c);

pub fn on_cpu(info: &AdapterInfo) -> bool {
    info.device_type == DeviceType::Cpu || (info.vendor, info.device) == BASIC_RENDER
}

pub fn start_line(info: &AdapterInfo) -> String {
    let kind = if on_cpu(info) {
        "cpu"
    } else {
        match info.device_type {
            DeviceType::DiscreteGpu => "discrete",
            DeviceType::IntegratedGpu => "integrated",
            DeviceType::VirtualGpu => "virtual",
            DeviceType::Cpu | DeviceType::Other => "other",
        }
    };
    format!("panel: draws on {} ({kind})", info.name)
}

pub fn scrolls(animations_on: bool, on_cpu: bool) -> bool {
    animations_on && !on_cpu
}

// While the panel draws on the CPU: whether a graphics card is listed now,
// one already listed as the panel started included, since the driver can
// come up while the window opens. A card Direct3D 12 cannot use is listed
// too, and starting again lands on the CPU just the same; the start line in
// the log names what the panel took.
pub struct CardWatch<E = CaptureError> {
    // capture::adapters, which leaves the software ones out, or a test's.
    list: fn() -> Result<Vec<Adapter>, E>,
    next_look: Instant,
    came: bool,
    // A list that could not be had goes to the log once, and the looks go
    // on.
    failed: bool,
}

impl CardWatch {
    pub fn start(now: Instant) -> CardWatch {
        CardWatch::listing(capture::adapters, now)
    }
}

impl<E: Display> CardWatch<E> {
    // The first look is due at once.
    pub fn listing(list: fn() -> Result<Vec<Adapter>, E>, now: Instant) -> CardWatch<E> {
        CardWatch {
            list,
            next_look: now,
            came: false,
            failed: false,
        }
    }

    // The list is asked only when a look is due, and never once a card has
    // come. The log's line when a card has come, or when the list could not
    // be had for the first time.
    pub fn look(&mut self, now: Instant) -> Option<String> {
        if self.came || now < self.next_look {
            return None;
        }
        self.next_look = now + LOOK_EVERY;
        match (self.list)() {
            Ok(listed) => {
                let card = listed.into_iter().next()?;
                self.came = true;
                Some(format!(
                    "panel: {} is listed now; the panel draws on the cpu until Booth starts again",
                    card.description
                ))
            }
            Err(err) => {
                (!std::mem::replace(&mut self.failed, true)).then(|| format!("panel: {err}"))
            }
        }
    }

    pub fn card_came(&self) -> bool {
        self.came
    }

    // How long until the next look, while one is to come. The start screen
    // asks for no frames of its own, and without one asked for here the look
    // would wait for the mouse.
    pub fn next_look(&self, now: Instant) -> Option<Duration> {
        (!self.came).then(|| self.next_look.saturating_duration_since(now))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use eframe::wgpu::Backend;

    fn info(name: &str, device_type: DeviceType, ids: (u32, u32)) -> AdapterInfo {
        AdapterInfo {
            name: name.to_owned(),
            vendor: ids.0,
            device: ids.1,
            ..AdapterInfo::new(device_type, Backend::Dx12)
        }
    }

    fn card(description: &str) -> Adapter {
        Adapter {
            description: description.to_owned(),
            vendor_id: 0x10de,
            device_id: 0x2705,
            luid: 9,
        }
    }

    fn nothing() -> Result<Vec<Adapter>, String> {
        Ok(Vec::new())
    }

    fn nvidia() -> Result<Vec<Adapter>, String> {
        Ok(vec![card("NVIDIA GeForce RTX 4070 Ti SUPER")])
    }

    fn refused() -> Result<Vec<Adapter>, String> {
        Err(String::from(
            "could not list the graphics adapters: access denied",
        ))
    }

    fn not_asked() -> Result<Vec<Adapter>, String> {
        panic!("the list was asked when no look was due")
    }

    const CAME: &str = "panel: NVIDIA GeForce RTX 4070 Ti SUPER is listed now; the panel draws on the cpu until Booth starts again";

    #[test]
    fn the_start_line_names_the_adapter_and_its_kind() {
        let warp = info(
            "Microsoft Basic Render Driver",
            DeviceType::Cpu,
            BASIC_RENDER,
        );
        assert!(on_cpu(&warp));
        assert_eq!(
            start_line(&warp),
            "panel: draws on Microsoft Basic Render Driver (cpu)"
        );
        // By its ids alone, as the viewer and capture tell it.
        let by_ids = info(
            "Microsoft Basic Render Driver",
            DeviceType::Other,
            BASIC_RENDER,
        );
        assert!(on_cpu(&by_ids));
        assert!(start_line(&by_ids).ends_with(" (cpu)"));

        let nvidia = "NVIDIA GeForce RTX 4070 Ti SUPER";
        let discrete = info(nvidia, DeviceType::DiscreteGpu, (0x10de, 0x2705));
        assert!(!on_cpu(&discrete));
        assert_eq!(
            start_line(&discrete),
            "panel: draws on NVIDIA GeForce RTX 4070 Ti SUPER (discrete)"
        );
        let intel = info(
            "Intel(R) UHD Graphics 770",
            DeviceType::IntegratedGpu,
            (0x8086, 0xa780),
        );
        assert!(!on_cpu(&intel));
        assert!(start_line(&intel).ends_with(" (integrated)"));
        let virtual_gpu = info("Virtual GPU", DeviceType::VirtualGpu, (0x1af4, 0x1050));
        assert!(!on_cpu(&virtual_gpu));
        assert!(start_line(&virtual_gpu).ends_with(" (virtual)"));
    }

    #[test]
    fn the_trace_stands_still_on_the_cpu() {
        assert!(scrolls(true, false));
        assert!(!scrolls(true, true));
        assert!(!scrolls(false, false));
        assert!(!scrolls(false, true));
    }

    // The driver can come up between wgpu's list and the panel's first look,
    // so a card listed already at that look counts as one that came later.
    #[test]
    fn a_card_listed_at_the_first_look_counts() {
        let t = Instant::now();
        let mut watch = CardWatch::listing(nvidia, t);
        assert!(!watch.card_came());
        assert_eq!(watch.look(t).as_deref(), Some(CAME));
        assert!(watch.card_came());

        // Nothing at the start, as in the night it happened.
        let mut watch = CardWatch::listing(nothing, t);
        assert_eq!(watch.look(t), None);
        assert!(!watch.card_came());
        watch.list = nvidia;
        assert_eq!(watch.look(t + LOOK_EVERY).as_deref(), Some(CAME));
        assert!(watch.card_came());
    }

    // A look when one is due and not between, none at all once a card has
    // come.
    #[test]
    fn looks_come_every_few_seconds_until_a_card_comes() {
        let t = Instant::now();
        let mut watch = CardWatch::listing(nothing, t);
        assert_eq!(watch.next_look(t), Some(Duration::ZERO));
        assert_eq!(watch.look(t), None);
        assert_eq!(watch.next_look(t), Some(LOOK_EVERY));

        watch.list = not_asked;
        assert_eq!(watch.look(t + LOOK_EVERY / 2), None);
        assert_eq!(watch.next_look(t + LOOK_EVERY / 2), Some(LOOK_EVERY / 2));

        watch.list = nvidia;
        assert!(watch.look(t + LOOK_EVERY).is_some());
        assert_eq!(watch.next_look(t + LOOK_EVERY), None);
        watch.list = not_asked;
        assert_eq!(watch.look(t + LOOK_EVERY * 10), None);
    }

    // The log hears why no card is heard of once, and the looks go on.
    #[test]
    fn a_list_that_could_not_be_had_is_logged_once() {
        let t = Instant::now();
        let mut watch = CardWatch::listing(refused, t);
        assert_eq!(
            watch.look(t).as_deref(),
            Some("panel: could not list the graphics adapters: access denied")
        );
        assert_eq!(watch.next_look(t), Some(LOOK_EVERY));
        assert_eq!(watch.look(t + LOOK_EVERY), None);
        assert!(!watch.card_came());

        watch.list = nvidia;
        assert_eq!(watch.look(t + LOOK_EVERY * 2).as_deref(), Some(CAME));
    }
}
