// The device lists as the settings screen shows them, built from what Windows
// reports, so the rules can be tested without Windows.

use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Direction {
    Input,
    Output,
}

impl Direction {
    // How errors and logs name the device.
    pub fn noun(self) -> &'static str {
        match self {
            Direction::Input => "the microphone",
            Direction::Output => "the output device",
        }
    }
}

// What settings.txt keeps: nothing for Windows default, or the endpoint id,
// which stays the same for a device across restarts and replugging.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub enum Choice {
    #[default]
    Default,
    Device(String),
}

impl Choice {
    // An empty id is how settings.txt says Windows default.
    pub fn from_id(id: &str) -> Choice {
        let id = id.trim();
        if id.is_empty() {
            Choice::Default
        } else {
            Choice::Device(id.to_owned())
        }
    }

    pub fn id(&self) -> Option<&str> {
        match self {
            Choice::Default => None,
            Choice::Device(id) => Some(id),
        }
    }
}

// One endpoint as Windows reported it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    pub id: String,
    // None when the property store would not give one.
    pub name: Option<String>,
    pub active: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListedDevice {
    pub id: String,
    pub name: String,
    // The Windows default for the console role, which is what the sound
    // settings page and the taskbar call the default.
    pub is_default: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeviceList {
    // In Windows' order, which is the order the sound settings page uses.
    pub devices: Vec<ListedDevice>,
}

// What a choice comes to against the devices connected now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolved<'a> {
    Device(&'a ListedDevice),
    // Windows default, and there is no default because nothing is connected.
    NoDevice,
    // A device that was chosen by name and is not connected.
    Missing,
}

impl DeviceList {
    // Only active endpoints: a disabled or unplugged one cannot be opened.
    // A missing or blank name gets a plain stand-in rather than an empty
    // row, and an id Windows lists twice is kept once.
    pub fn from_endpoints(endpoints: Vec<Endpoint>, default_id: Option<&str>) -> DeviceList {
        let mut devices: Vec<ListedDevice> = Vec::with_capacity(endpoints.len());
        for endpoint in endpoints {
            if !endpoint.active || endpoint.id.is_empty() {
                continue;
            }
            if devices.iter().any(|device| device.id == endpoint.id) {
                continue;
            }
            let name = endpoint
                .name
                .map(|name| name.trim().to_owned())
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| String::from("Unnamed device"));
            devices.push(ListedDevice {
                is_default: default_id == Some(endpoint.id.as_str()),
                id: endpoint.id,
                name,
            });
        }
        DeviceList { devices }
    }

    pub fn default_device(&self) -> Option<&ListedDevice> {
        self.devices.iter().find(|device| device.is_default)
    }

    pub fn find(&self, id: &str) -> Option<&ListedDevice> {
        self.devices.iter().find(|device| device.id == id)
    }

    pub fn resolve(&self, choice: &Choice) -> Resolved<'_> {
        match choice {
            Choice::Default => self
                .default_device()
                .map_or(Resolved::NoDevice, Resolved::Device),
            Choice::Device(id) => self.find(id).map_or(Resolved::Missing, Resolved::Device),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Lists {
    pub inputs: DeviceList,
    pub outputs: DeviceList,
}

impl Lists {
    pub fn get(&self, direction: Direction) -> &DeviceList {
        match direction {
            Direction::Input => &self.inputs,
            Direction::Output => &self.outputs,
        }
    }
}

impl fmt::Display for Direction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Direction::Input => "input",
            Direction::Output => "output",
        })
    }
}
