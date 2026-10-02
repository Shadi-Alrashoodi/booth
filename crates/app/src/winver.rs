// Booth needs Windows 10 version 2004 or later, or Windows 11. GetVersionEx
// answers with the newest Windows the exe's manifest names, and RtlGetVersion
// with whatever a compatibility mode says: on Windows 11, "Run this program in
// compatibility mode for Windows 8" makes it say 6.2. The page the kernel maps
// into every process has the version the kernel really is.

use windows_sys::Wdk::System::SystemServices::{KUSER_SHARED_DATA, RtlGetVersion};
use windows_sys::Win32::System::SystemInformation::OSVERSIONINFOW;

// Where every Windows since NT maps that page in a process (ntddk.h,
// MM_SHARED_USER_DATA_VA).
const SHARED_PAGE: usize = 0x7ffe_0000;

// Windows 10 version 2004.
const OLDEST_BUILD: u32 = 19041;

// Windows 10's releases before 2004, by the build each one shipped as.
const OLDER_RELEASES: [(u32, &str); 9] = [
    (10240, "1507"),
    (10586, "1511"),
    (14393, "1607"),
    (15063, "1703"),
    (16299, "1709"),
    (17134, "1803"),
    (17763, "1809"),
    (18362, "1903"),
    (18363, "1909"),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowsVersion {
    pub major: u32,
    pub minor: u32,
    pub build: u32,
}

// None if Windows would not say, and then Booth goes on and finds out.
pub fn this_pc() -> Option<WindowsVersion> {
    real(shared_page(), reported())
}

fn shared_page() -> WindowsVersion {
    let page = std::ptr::with_exposed_provenance::<KUSER_SHARED_DATA>(SHARED_PAGE);
    // SAFETY: Windows maps this page read-only at this address in every
    // process for as long as it runs, and all three fields lie inside it on
    // every version (wdm.h asserts their offsets). The kernel writes other
    // parts of it at any time, hence volatile.
    unsafe {
        WindowsVersion {
            major: (&raw const (*page).NtMajorVersion).read_volatile(),
            minor: (&raw const (*page).NtMinorVersion).read_volatile(),
            build: (&raw const (*page).NtBuildNumber).read_volatile(),
        }
    }
}

fn reported() -> Option<WindowsVersion> {
    let mut info = OSVERSIONINFOW {
        dwOSVersionInfoSize: size_of::<OSVERSIONINFOW>() as u32,
        dwMajorVersion: 0,
        dwMinorVersion: 0,
        dwBuildNumber: 0,
        dwPlatformId: 0,
        szCSDVersion: [0; 128],
    };
    // SAFETY: info is an OSVERSIONINFOW that lives across the call, with its
    // size filled in as the call requires.
    let status = unsafe { RtlGetVersion(&mut info) };
    (status == 0).then_some(WindowsVersion {
        major: info.dwMajorVersion,
        minor: info.dwMinorVersion,
        build: info.dwBuildNumber,
    })
}

// The page's build number is newer than its major and minor: on older
// Windows the field is reserved and reads 0. Then RtlGetVersion's build is
// taken, but only when no compatibility mode has changed the rest of what it
// says. Windows 8.1 and older are named without a build.
fn real(shared: WindowsVersion, reported: Option<WindowsVersion>) -> Option<WindowsVersion> {
    if shared.build != 0 || shared.major != 10 {
        return Some(shared);
    }
    reported.filter(|reported| (reported.major, reported.minor) == (shared.major, shared.minor))
}

pub fn too_old(version: WindowsVersion) -> Option<String> {
    let new_enough = match version.major {
        10 => version.build >= OLDEST_BUILD,
        major => major > 10,
    };
    if new_enough {
        return None;
    }
    Some(format!(
        "Booth needs Windows 10 version 2004 or later, or Windows 11. This PC runs {}.",
        name(version)
    ))
}

fn name(version: WindowsVersion) -> String {
    let WindowsVersion {
        major,
        minor,
        build,
    } = version;
    let old = match (major, minor) {
        (10, 0) => {
            return match OLDER_RELEASES.iter().find(|(shipped, _)| *shipped == build) {
                Some((_, release)) => format!("Windows 10 version {release}"),
                // Insider builds between releases.
                None => format!("Windows 10 build {build}"),
            };
        }
        (6, 3) => "Windows 8.1",
        (6, 2) => "Windows 8",
        (6, 1) => "Windows 7",
        (6, 0) => "Windows Vista",
        _ => return format!("Windows {major}.{minor} build {build}"),
    };
    String::from(old)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::process::Command;

    fn windows(major: u32, minor: u32, build: u32) -> WindowsVersion {
        WindowsVersion {
            major,
            minor,
            build,
        }
    }

    #[test]
    fn version_2004_and_later_and_windows_11_run() {
        for build in [19041, 19042, 19045, 22000, 22631, 26100, 26200, 30000] {
            assert_eq!(too_old(windows(10, 0, build)), None, "build {build}");
        }
        assert_eq!(too_old(windows(11, 0, 1)), None);
    }

    #[test]
    fn older_windows_is_refused_by_name() {
        let refused = |major, minor, build| too_old(windows(major, minor, build)).unwrap();
        assert_eq!(
            refused(10, 0, 18363),
            "Booth needs Windows 10 version 2004 or later, or Windows 11. This PC runs Windows 10 version 1909."
        );
        assert!(refused(10, 0, 17763).ends_with("This PC runs Windows 10 version 1809."));
        assert!(refused(10, 0, 10240).ends_with("This PC runs Windows 10 version 1507."));
        assert!(refused(10, 0, 19040).ends_with("This PC runs Windows 10 build 19040."));
        assert!(refused(6, 3, 9600).ends_with("This PC runs Windows 8.1."));
        assert!(refused(6, 1, 7601).ends_with("This PC runs Windows 7."));
        assert!(refused(5, 1, 2600).ends_with("This PC runs Windows 5.1 build 2600."));
    }

    #[test]
    fn the_page_wins_over_a_compatibility_mode() {
        let windows_8 = Some(windows(6, 2, 9200));
        assert_eq!(
            real(windows(10, 0, 26200), windows_8),
            Some(windows(10, 0, 26200))
        );
        assert_eq!(
            real(windows(10, 0, 0), Some(windows(10, 0, 10586))),
            Some(windows(10, 0, 10586))
        );
        assert_eq!(real(windows(10, 0, 0), windows_8), None);
        assert_eq!(real(windows(10, 0, 0), None), None);
        assert_eq!(real(windows(6, 3, 0), windows_8), Some(windows(6, 3, 0)));
    }

    // Whatever this PC is, it is one Booth is built and tested on.
    #[test]
    fn this_pc_reads_and_passes() {
        let version = this_pc().expect("windows answers");
        assert_eq!(too_old(version), None, "{version:?}");
        // Proof that the layer the test below sets took hold, or it tests
        // nothing.
        if std::env::var_os("__COMPAT_LAYER").is_some_and(|layer| layer == "WIN8RTM") {
            let said = reported().map(|reported| (reported.major, reported.minor));
            assert_eq!(said, Some((6, 2)), "rtlgetversion under the layer");
        }
    }

    // What Windows does for "Run this program in compatibility mode for
    // Windows 8" on the exe, for this test exe alone.
    #[test]
    fn a_compatibility_mode_does_not_fool_it() {
        let exe = std::env::current_exe().expect("the test exe's path");
        let output = Command::new(exe)
            .args([
                "winver::tests::this_pc_reads_and_passes",
                "--exact",
                "--nocapture",
            ])
            .env("__COMPAT_LAYER", "WIN8RTM")
            .output()
            .expect("run the test exe again");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "{}\n{stdout}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(stdout.contains("1 passed"), "{stdout}");
    }
}
