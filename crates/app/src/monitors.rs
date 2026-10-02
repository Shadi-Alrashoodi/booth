// The monitors Share can start on: the list under the title row on a PC with
// more than one, and the one the share key takes, which is the last one
// picked there.

use room::{Monitor, MonitorId};

// Shares go up to 120 fps, and capture holds a faster monitor to that. A
// slower one is asked for at its own rate, so the encoder's rate control and
// the viewer's frame interval match the frames that come.
pub const MOST_FPS: u8 = 120;

pub struct Pick {
    pub id: MonitorId,
    pub fps: u8,
    // On the button: its size and where it sits.
    pub text: String,
    // What a screen reader says for the button.
    pub spoken: String,
}

// Asked of Windows each time, since monitors come and go between shares.
// It takes a few milliseconds.
pub fn list() -> Result<Vec<Monitor>, String> {
    room::monitors().map_err(|err| format!("could not list the monitors: {err}"))
}

// In the order they sit on the desk, left to right or top to bottom.
pub fn picks(monitors: &[Monitor]) -> Vec<Pick> {
    let rects: Vec<Place> = monitors.iter().map(Place::of).collect();
    let order = order(&rects);
    let words = places(&rects, &order);
    order
        .iter()
        .zip(words)
        .map(|(&i, place)| {
            let monitor = &monitors[i];
            let size = format!("{}x{}", monitor.width, monitor.height);
            Pick {
                id: monitor.id.clone(),
                fps: fps(monitor.refresh_hz),
                spoken: format!("Share the {size} monitor, {place}"),
                text: format!("{size}, {place}"),
            }
        })
        .collect()
}

// The one picked last time, by its Windows name, or the primary one when
// that is not attached now. The name can change when monitors are plugged
// in again, and then the primary is what Share without the list would take.
pub fn remembered<'a>(monitors: &'a [Monitor], device: Option<&str>) -> Option<&'a Monitor> {
    device
        .and_then(|device| monitors.iter().find(|m| m.id.device_name == device))
        .or_else(|| monitors.iter().find(|m| m.primary))
        .or_else(|| monitors.first())
}

pub fn fps(refresh_hz: f64) -> u8 {
    if !refresh_hz.is_finite() || refresh_hz < 1.0 {
        return MOST_FPS;
    }
    refresh_hz.round().min(f64::from(MOST_FPS)) as u8
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Place {
    left: i32,
    top: i32,
    width: u32,
    height: u32,
}

impl Place {
    fn of(monitor: &Monitor) -> Place {
        Place {
            left: monitor.left,
            top: monitor.top,
            width: monitor.width,
            height: monitor.height,
        }
    }

    fn centre(&self) -> (f64, f64) {
        (
            f64::from(self.left) + f64::from(self.width) / 2.0,
            f64::from(self.top) + f64::from(self.height) / 2.0,
        )
    }
}

// Side by side unless the monitors are spread further up and down than
// across.
fn across(rects: &[Place]) -> bool {
    let spread = |axis: fn(&Place) -> f64| {
        let values = rects.iter().map(axis);
        let most = values.clone().fold(f64::MIN, f64::max);
        let least = values.fold(f64::MAX, f64::min);
        most - least
    };
    spread(|place| place.centre().0) >= spread(|place| place.centre().1)
}

fn order(rects: &[Place]) -> Vec<usize> {
    let across = across(rects);
    let mut order: Vec<usize> = (0..rects.len()).collect();
    order.sort_by(|&a, &b| {
        let (a, b) = (rects[a].centre(), rects[b].centre());
        let (a, b) = if across {
            (a, b)
        } else {
            ((a.1, a.0), (b.1, b.0))
        };
        a.partial_cmp(&b).unwrap_or(std::cmp::Ordering::Equal)
    });
    order
}

// One word for where each sits, in `order`. Past three a word would be a
// guess, so the corner's position is written out instead.
fn places(rects: &[Place], order: &[usize]) -> Vec<String> {
    let words: &[&str] = match (order.len(), across(rects)) {
        (1, _) => &["this one"],
        (2, true) => &["left", "right"],
        (2, false) => &["top", "bottom"],
        (3, true) => &["left", "middle", "right"],
        (3, false) => &["top", "middle", "bottom"],
        _ => &[],
    };
    if !words.is_empty() {
        return words.iter().map(|word| (*word).to_owned()).collect();
    }
    order
        .iter()
        .map(|&i| format!("at {},{}", rects[i].left, rects[i].top))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn place(left: i32, top: i32, width: u32, height: u32) -> Place {
        Place {
            left,
            top,
            width,
            height,
        }
    }

    fn named(rects: &[Place]) -> Vec<(usize, String)> {
        let order = order(rects);
        let words = places(rects, &order);
        order.into_iter().zip(words).collect()
    }

    // A 1440p main monitor with a 1080p one on its left, which Windows puts
    // at negative coordinates.
    #[test]
    fn side_by_side_monitors_are_named_left_to_right() {
        let rects = [place(0, 0, 2560, 1440), place(-1920, 200, 1920, 1080)];
        assert_eq!(
            named(&rects),
            [(1, String::from("left")), (0, String::from("right"))]
        );
        let three = [
            place(1920, 0, 2560, 1440),
            place(4480, 0, 1920, 1080),
            place(0, 0, 1920, 1080),
        ];
        assert_eq!(
            named(&three),
            [
                (2, String::from("left")),
                (0, String::from("middle")),
                (1, String::from("right")),
            ]
        );
    }

    #[test]
    fn stacked_monitors_are_named_top_to_bottom() {
        let rects = [place(0, 0, 1920, 1080), place(320, -1440, 2560, 1440)];
        assert_eq!(
            named(&rects),
            [(1, String::from("top")), (0, String::from("bottom"))]
        );
    }

    #[test]
    fn past_three_the_position_is_written_out() {
        let rects = [
            place(0, 0, 1920, 1080),
            place(1920, 0, 1920, 1080),
            place(0, 1080, 1920, 1080),
            place(1920, 1080, 1920, 1080),
        ];
        let words: Vec<String> = named(&rects).into_iter().map(|(_, word)| word).collect();
        assert_eq!(words, ["at 0,0", "at 0,1080", "at 1920,0", "at 1920,1080"]);
    }

    fn monitor(device: &str, left: i32, primary: bool) -> Monitor {
        Monitor {
            id: MonitorId {
                device_name: device.to_owned(),
                adapter_luid: 7,
            },
            name: String::from("Generic PnP Monitor"),
            left,
            top: 0,
            width: 2560,
            height: 1440,
            refresh_hz: 143.97,
            rotation: capture::Rotation::Identity,
            primary,
            hdr: false,
            adapter: capture::Adapter {
                description: String::from("Test adapter"),
                vendor_id: 0x10de,
                device_id: 1,
                luid: 7,
            },
        }
    }

    #[test]
    fn share_key_monitor() {
        let monitors = [
            monitor(r"\\.\DISPLAY1", 0, true),
            monitor(r"\\.\DISPLAY2", 2560, false),
        ];
        let device = |found: Option<&Monitor>| found.map(|m| m.id.device_name.clone());
        assert_eq!(
            device(remembered(&monitors, Some(r"\\.\DISPLAY2"))).as_deref(),
            Some(r"\\.\DISPLAY2")
        );
        // Unplugged since, or never picked.
        for gone in [Some(r"\\.\DISPLAY5"), None] {
            assert_eq!(
                device(remembered(&monitors, gone)).as_deref(),
                Some(r"\\.\DISPLAY1")
            );
        }
        assert!(remembered(&[], None).is_none());

        let picks = picks(&monitors);
        let texts: Vec<&str> = picks.iter().map(|pick| pick.text.as_str()).collect();
        assert_eq!(texts, ["2560x1440, left", "2560x1440, right"]);
        assert_eq!(picks[1].spoken, "Share the 2560x1440 monitor, right");
        assert_eq!(picks[1].id.device_name, r"\\.\DISPLAY2");
        assert_eq!(picks[1].fps, 120);
    }

    #[test]
    fn fps_up_to_120() {
        assert_eq!(fps(59.94), 60);
        assert_eq!(fps(144.0), 120);
        assert_eq!(fps(239.76), 120);
        assert_eq!(fps(0.0), 120);
        assert_eq!(fps(f64::NAN), 120);
    }
}
