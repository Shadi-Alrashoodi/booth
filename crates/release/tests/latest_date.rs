// release.ps1 writes the date in latest.txt and nothing reads it before it is
// signed, so a wrong calendar reaches every copy. This takes the line from the
// script and runs it in Windows PowerShell under regional formats that are not
// Gregorian, since the script has no way to be run short of a whole release.

use std::os::windows::process::CommandExt;
use std::process::Command;

const RELEASE_SCRIPT: &str = include_str!("../../../tools/release.ps1");
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const TODAY: &str =
    "[DateTime]::UtcNow.ToString('yyyy-MM-dd', [Globalization.CultureInfo]::InvariantCulture)";

#[test]
fn published_date_is_gregorian_under_any_regional_format() {
    let lines: Vec<&str> = RELEASE_SCRIPT
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("\"published = "))
        .collect();
    let [line] = lines[..] else {
        panic!("release.ps1 should have one published line, found {lines:?}");
    };
    let published = line.trim_end_matches(',');

    // The date is read on both sides of the line, as midnight UTC can pass
    // between the two.
    let script = format!(
        "foreach ($culture in 'ar-SA', 'fa-IR', 'th-TH') {{
            [Threading.Thread]::CurrentThread.CurrentCulture = [Globalization.CultureInfo]::new($culture, $false)
            $calendar = [Globalization.CultureInfo]::CurrentCulture.Calendar.GetType().Name
            $before = {TODAY}
            $published = {published}
            $after = {TODAY}
            \"$calendar|$before|$after|$published\"
        }}"
    );
    let output = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .expect("could not run powershell");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stdout}{stderr}");
    assert!(stderr.is_empty(), "{stderr}");

    let results: Vec<&str> = stdout.lines().collect();
    assert_eq!(results.len(), 3, "{stdout}");
    for result in results {
        let fields: Vec<&str> = result.split('|').collect();
        let [calendar, before, after, published] = fields[..] else {
            panic!("could not read {result:?}");
        };
        // Otherwise the culture did not take and this shows nothing.
        assert_ne!(calendar, "GregorianCalendar", "{result}");
        assert!(
            published == format!("published = {before}")
                || published == format!("published = {after}"),
            "{result}"
        );
    }
}
