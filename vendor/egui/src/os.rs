/// An `enum` of common operating systems.
#[expect(clippy::upper_case_acronyms)] // `Ios` looks too ugly
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OperatingSystem {
    /// Unknown OS - could be wasm
    Unknown,

    /// Android OS
    Android,

    /// Apple iPhone OS
    IOS,

    /// Linux or Unix other than Android
    Nix,

    /// macOS
    Mac,

    /// Windows
    Windows,
}

impl Default for OperatingSystem {
    fn default() -> Self {
        Self::from_target_os()
    }
}

impl OperatingSystem {
    /// Uses the compile-time `target_arch` to identify the OS.
    pub const fn from_target_os() -> Self {
        if cfg!(target_arch = "wasm32") {
            Self::Unknown
        } else if cfg!(target_os = "android") {
            Self::Android
        } else if cfg!(target_os = "ios") {
            Self::IOS
        } else if cfg!(target_os = "macos") {
            Self::Mac
        } else if cfg!(target_os = "windows") {
            Self::Windows
        } else if cfg!(target_os = "linux")
            || cfg!(target_os = "dragonfly")
            || cfg!(target_os = "freebsd")
            || cfg!(target_os = "netbsd")
            || cfg!(target_os = "openbsd")
        {
            Self::Nix
        } else {
            Self::Unknown
        }
    }

    /// Helper: try to guess from the identification string of a browser.
    pub fn from_browser_id(browser_id: &str) -> Self {
        if browser_id.contains("Android") {
            Self::Android
        } else if browser_id.contains("like Mac") {
            Self::IOS
        } else if browser_id.contains("Win") {
            Self::Windows
        } else if browser_id.contains("Mac") {
            Self::Mac
        } else if browser_id.contains("Linux")
            || browser_id.contains("X11")
            || browser_id.contains("Unix")
        {
            Self::Nix
        } else {
            log::warn!(
                "egui: Failed to guess operating system from the browser id {browser_id:?}. Please file an issue at https://github.com/emilk/egui/issues"
            );

            Self::Unknown
        }
    }

    /// Are we either macOS or iOS?
    pub fn is_mac(&self) -> bool {
        matches!(self, Self::Mac | Self::IOS)
    }
}
