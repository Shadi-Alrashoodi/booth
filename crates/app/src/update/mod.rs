// The opt-in check for a newer Booth: off unless the user turns it on, and
// then only against a signed manifest. At most once a day, when Booth
// starts and never in a room, it fetches latest.txt and its minisign
// signature, checks the signature against the release key built in below,
// and only then reads the manifest. Downloading is the user's own step, and
// nothing is ever run or unzipped.

mod download;
mod http;
mod manifest;

use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// One address for the update check and for the sentences that tell a
// friend where to get the host's version.
pub use invite::RELEASES_PAGE;

use download::{DownloadError, Kept};
use http::{FetchError, Rules};
use manifest::{MOST_MANIFEST_BYTES, MOST_SIGNATURE_BYTES, Manifest, Version};

// GitHub's address for a file of the newest release.
fn latest_url(file: &str) -> String {
    format!("{RELEASES_PAGE}/latest/download/{file}")
}

// The public half of the release key, as the release tool's keygen prints
// it. release.ps1 reads this line and refuses to sign with any other key.
pub const RELEASE_KEY: &str = "RWRGAGxPnSU2yismU1Cuan+osuxNCa8kqHZW8JobYrUoz+Y0Zwh7e6Ty";

const DAY_SECS: u64 = 24 * 60 * 60;
const CHECK_WAIT: Duration = Duration::from_secs(10);
const CHECK_DEADLINE: Duration = Duration::from_secs(30);
// The start screen's progress line changes at most this often.
const PROGRESS_EVERY: Duration = Duration::from_millis(200);
// The release's two small files. A newer release a check finds is kept
// under the same names in the data folder, so a second start that day
// still offers it.
const MANIFEST_FILE: &str = "latest.txt";
const SIGNATURE_FILE: &str = "latest.txt.minisig";
// In the data folder: when the last check was. What Booth writes there is
// about 25 bytes, so a longer file is not read to the end.
const LAST_CHECK: &str = "update.txt";
const MOST_RECORD_BYTES: usize = 64;

// Under the setting in Settings while it is on.
pub const CHECK_ABOUT: &str = "Once a day, when Booth starts, it asks GitHub whether a newer version is out. GitHub sees your address, as it would for any download. The new version is not downloaded until you press Download.";

// In Settings, in place of the setting, in the Store's copy.
pub const FROM_STORE: &str = concat!(
    "Booth ",
    env!("CARGO_PKG_VERSION"),
    ", from the Microsoft Store. The Store keeps it up to date."
);

pub type Notify = Arc<dyn Fn() + Send + Sync>;

// A day since the last check, either way: a clock set back by more than a
// day would otherwise stop the checks until it caught up.
pub fn due(last: Option<u64>, now: u64) -> bool {
    last.is_none_or(|last| now.abs_diff(last) >= DAY_SECS)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

// "checked = 1791936000". Anything else counts as never checked.
fn last_check(dir: &Path, log: &mut Vec<String>) -> Option<u64> {
    let path = dir.join(LAST_CHECK);
    let bytes = match read_small(&path, MOST_RECORD_BYTES) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return None,
        Err(err) => {
            log.push(format!(
                "update: could not read {}: {err}; counted as never checked",
                path.display()
            ));
            return None;
        }
    };
    let secs = bytes
        .as_deref()
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        .and_then(|text| text.strip_suffix("\r\n"))
        .and_then(|text| text.strip_prefix("checked = "))
        .filter(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|digits| digits.parse().ok());
    if secs.is_none() {
        log.push(format!(
            "update: {} is not what Booth writes there; counted as never checked",
            path.display()
        ));
    }
    secs
}

// Up to `most` bytes of one of Booth's own small files; None past that.
fn read_small(path: &Path, most: usize) -> io::Result<Option<Vec<u8>>> {
    let mut bytes = Vec::new();
    File::open(path)?
        .take(most as u64 + 1)
        .read_to_end(&mut bytes)?;
    Ok((bytes.len() <= most).then_some(bytes))
}

fn forget_kept(dir: &Path) {
    let _ = fs::remove_file(dir.join(MANIFEST_FILE));
    let _ = fs::remove_file(dir.join(SIGNATURE_FILE));
}

// The release a check found earlier today, checked again from its own
// signed files: no network.
fn kept(dir: &Path, running: Version, key: &str, log: &mut Vec<String>) -> Option<Manifest> {
    let manifest_file = read_small(&dir.join(MANIFEST_FILE), MOST_MANIFEST_BYTES);
    let signature_file = read_small(&dir.join(SIGNATURE_FILE), MOST_SIGNATURE_BYTES);
    let (Ok(Some(bytes)), Ok(Some(signature))) = (manifest_file, signature_file) else {
        return None;
    };
    let manifest = manifest::verify(&bytes, &signature, key)
        .map_err(|why| why.to_string())
        .and_then(|()| manifest::parse(&bytes).map_err(|why| why.to_string()));
    match manifest {
        Ok(manifest) if manifest.version > running => {
            log.push(format!(
                "update: {} found at the last check is still newer",
                manifest.version
            ));
            Some(manifest)
        }
        Ok(_) => {
            forget_kept(dir);
            None
        }
        Err(why) => {
            log.push(format!("update: the kept release is dropped: {why}"));
            forget_kept(dir);
            None
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Checked {
    Newer(Box<Manifest>),
    Newest,
    // A room opened while it ran.
    Stopped,
    // The start screen's sentence.
    Failed(String),
}

fn check_failed(why: &str) -> Checked {
    Checked::Failed(format!(
        "Could not check for a new version: {why}. Booth looks again after a day."
    ))
}

impl From<FetchError> for Checked {
    fn from(err: FetchError) -> Checked {
        match err {
            FetchError::Stopped => Checked::Stopped,
            err => check_failed(&err.to_string()),
        }
    }
}

type Fetch<'a> = dyn FnMut(&str, usize) -> Result<Vec<u8>, FetchError> + 'a;

// The whole check, with the fetch passed in so tests can stand in for the
// network. The time is written down before anything is fetched, so a check
// that fails still counts, and one that cannot be written down is not made.
fn check(
    dir: &Path,
    running: Version,
    key: &str,
    now: u64,
    fetch: &mut Fetch<'_>,
    log: &mut Vec<String>,
) -> Checked {
    if !due(last_check(dir, log), now) {
        log.push(String::from(
            "update: checked less than a day ago, so not now",
        ));
        return match kept(dir, running, key, log) {
            Some(manifest) => Checked::Newer(Box::new(manifest)),
            None => Checked::Newest,
        };
    }
    if !manifest::usable_key(key) {
        return Checked::Failed(String::from(
            "This copy of Booth was built without the release key, so it cannot check for new versions.",
        ));
    }
    let record = dir.join(LAST_CHECK);
    if let Err(err) = fs::write(&record, format!("checked = {now}\r\n")) {
        return check_failed(&format!(
            "{} could not be written ({err})",
            record.display()
        ));
    }
    let (manifest_url, signature_url) = (latest_url(MANIFEST_FILE), latest_url(SIGNATURE_FILE));
    log.push(format!("update: fetching {manifest_url} and its signature"));
    let latest = match fetch(&manifest_url, MOST_MANIFEST_BYTES) {
        Ok(bytes) => bytes,
        Err(err) => return err.into(),
    };
    let signature = match fetch(&signature_url, MOST_SIGNATURE_BYTES) {
        Ok(bytes) => bytes,
        Err(err) => return err.into(),
    };
    if let Err(why) = manifest::verify(&latest, &signature, key) {
        return check_failed(&why.to_string());
    }
    let manifest = match manifest::parse(&latest) {
        Ok(manifest) => manifest,
        Err(why) => return check_failed(&why.to_string()),
    };
    if manifest.version <= running {
        forget_kept(dir);
        log.push(format!(
            "update: {} is the newest release, and this is {running}",
            manifest.version
        ));
        return Checked::Newest;
    }
    let installer = manifest
        .installer
        .as_ref()
        .map_or(String::new(), |installer| {
            format!(", {} sha256 {}", installer.name, installer.sha256_hex())
        });
    log.push(format!(
        "update: {} is out, published {}, signed by the release key: {} sha256 {}{installer}",
        manifest.version,
        manifest.published,
        manifest.zip,
        manifest.sha256_hex()
    ));
    let kept = fs::write(dir.join(MANIFEST_FILE), &latest)
        .and_then(|()| fs::write(dir.join(SIGNATURE_FILE), &signature));
    if let Err(err) = kept {
        forget_kept(dir);
        log.push(format!(
            "update: could not keep the release's signed files in {}: {err}; a second start today will not offer it",
            dir.display()
        ));
    }
    Checked::Newer(Box::new(manifest))
}

// A download from an earlier run that Booth closed in the middle of.
pub fn tidy(dir: &Path) -> Option<String> {
    let part = download::part_path(dir);
    fs::remove_file(&part).ok()?;
    Some(format!(
        "update: removed {}, left by a download that did not finish",
        part.display()
    ))
}

enum Event {
    Log(String),
    Checked(Checked),
    Progress { got: u64, total: Option<u64> },
    Downloaded(Result<Kept, DownloadError>),
}

enum State {
    Quiet,
    Checking,
    CheckFailed(String),
    Newer(Box<Manifest>),
    Downloading {
        manifest: Box<Manifest>,
        got: u64,
        total: Option<u64>,
    },
    // The sentence that says where it is.
    Saved(String),
    DownloadFailed {
        manifest: Box<Manifest>,
        why: String,
    },
}

struct Worker {
    events: Receiver<Event>,
    stop: Arc<AtomicBool>,
}

// Wakes the start screen when a worker ends, however it ends.
struct Wake(Notify);

impl Drop for Wake {
    fn drop(&mut self) {
        (self.0)();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Plain,
    Quiet,
    Problem,
}

// What the start screen shows: one line, and whether Download goes with it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shown {
    pub text: String,
    pub tone: Tone,
    pub download: bool,
}

pub struct Update {
    state: State,
    running: Version,
    worker: Option<Worker>,
}

impl Update {
    pub fn off() -> Update {
        Update {
            state: State::Quiet,
            running: Version::running(),
            worker: None,
        }
    }

    // At start, with the setting on.
    pub fn start(dir: PathBuf, notify: Notify) -> Update {
        let mut update = Update {
            state: State::Checking,
            ..Update::off()
        };
        let running = update.running;
        update.spawn("update check", notify, move |stop, send| {
            let mut log = Vec::new();
            let rules = |most: usize| Rules {
                most: most as u64,
                wait: CHECK_WAIT,
                deadline: Some(CHECK_DEADLINE),
                fresh: true,
            };
            let mut fetch = |url: &str, most: usize| http::get_small(url, &rules(most), stop);
            let checked = check(&dir, running, RELEASE_KEY, unix_now(), &mut fetch, &mut log);
            match &checked {
                Checked::Failed(why) => log.push(format!("update: check failed: {why}")),
                Checked::Stopped => log.push(String::from("update: check stopped, a room opened")),
                Checked::Newer(_) | Checked::Newest => {}
            }
            for line in log {
                let _ = send.send(Event::Log(line));
            }
            let _ = send.send(Event::Checked(checked));
        });
        update
    }

    fn spawn(
        &mut self,
        name: &str,
        notify: Notify,
        work: impl FnOnce(&AtomicBool, &Sender<Event>) + Send + 'static,
    ) {
        let (send, events) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        let spawned = thread::Builder::new().name(name.to_owned()).spawn(move || {
            // Locals drop in reverse, in a panic too: the sender goes first,
            // so the pass the wake brings finds the answer, or that there
            // will be none.
            let _wake = Wake(notify);
            let sender = send;
            work(&stopped, &sender);
        });
        match spawned {
            Ok(_) => self.worker = Some(Worker { events, stop }),
            Err(err) => {
                self.worker_failed(&format!("Windows would not start a thread for it ({err})"));
            }
        }
    }

    fn worker_failed(&mut self, why: &str) {
        self.state = match std::mem::replace(&mut self.state, State::Quiet) {
            State::Downloading { manifest, .. } => {
                let why = format!(
                    "Could not download Booth {}: {why}. Press Download to try again.",
                    manifest.version
                );
                State::DownloadFailed { manifest, why }
            }
            _ => State::CheckFailed(format!("Could not check for a new version: {why}.")),
        };
    }

    // What the workers sent since the last pass; the lines are for the log.
    pub fn follow(&mut self) -> Vec<String> {
        let mut lines = Vec::new();
        let Some(worker) = &self.worker else {
            return lines;
        };
        let mut events = Vec::new();
        let ended = loop {
            match worker.events.try_recv() {
                Ok(event) => events.push(event),
                Err(TryRecvError::Empty) => break false,
                Err(TryRecvError::Disconnected) => break true,
            }
        };
        let mut answered = false;
        for event in events {
            match event {
                Event::Log(line) => lines.push(line),
                Event::Checked(_) | Event::Downloaded(_) => {
                    answered = true;
                    self.apply(event);
                }
                Event::Progress { .. } => self.apply(event),
            }
        }
        // Only a panic ends a worker before its answer. Without this the
        // line would stay at "Downloading" and Download would be refused
        // for the rest of the run.
        if ended && !answered {
            lines.push(String::from(
                "update: the update step ended without an answer",
            ));
            self.worker_failed("an error in Booth stopped it");
        }
        if answered || ended {
            self.worker = None;
        }
        lines
    }

    fn apply(&mut self, event: Event) {
        let state = std::mem::replace(&mut self.state, State::Quiet);
        self.state = match (state, event) {
            (_, Event::Checked(Checked::Newer(manifest))) => State::Newer(manifest),
            (_, Event::Checked(Checked::Newest | Checked::Stopped)) => State::Quiet,
            (_, Event::Checked(Checked::Failed(why))) => State::CheckFailed(why),
            (State::Downloading { manifest, .. }, Event::Progress { got, total }) => {
                State::Downloading {
                    manifest,
                    got,
                    total,
                }
            }
            (State::Downloading { .. }, Event::Downloaded(Ok(kept))) => {
                State::Saved(saved(&kept, download::own_folder()))
            }
            (State::Downloading { manifest, .. }, Event::Downloaded(Err(err))) => {
                let why = download_failed(&manifest, &err);
                State::DownloadFailed { manifest, why }
            }
            (state, _) => state,
        };
    }

    // Never in a room: whatever runs stops before its next connection, its
    // next wait on the network or its next 64 KB. A WinHTTP call already
    // under way is not cut short; it ends on its own timeouts, 10 s a wait
    // for the check and 30 s for the zip.
    pub fn room_opened(&mut self) -> Option<String> {
        let worker = self.worker.as_ref()?;
        if worker.stop.swap(true, Ordering::AcqRel) {
            return None;
        }
        Some(String::from(
            "update: a room opened, so the update step stops",
        ))
    }

    pub fn download(&mut self, dir: PathBuf, notify: Notify) {
        if self.worker.is_some() {
            return;
        }
        let manifest = match &self.state {
            State::Newer(manifest) | State::DownloadFailed { manifest, .. } => manifest.clone(),
            _ => return,
        };
        self.state = State::Downloading {
            manifest: manifest.clone(),
            got: 0,
            total: None,
        };
        let wake = Arc::clone(&notify);
        self.spawn("update download", notify, move |stop, send| {
            let part = download::part_path(&dir);
            let _ = send.send(Event::Log(format!(
                "update: downloading {} to {}",
                manifest.url,
                part.display()
            )));
            let mut shown = Instant::now();
            let mut progress = |got: u64, total: Option<u64>| {
                if shown.elapsed() >= PROGRESS_EVERY {
                    shown = Instant::now();
                    let _ = send.send(Event::Progress { got, total });
                    wake();
                }
            };
            let result = download::downloads_folder()
                .map_err(DownloadError::NoDownloads)
                .and_then(|downloads| {
                    download::fetch(&manifest, &dir, &downloads, stop, &mut progress)
                });
            let line = match &result {
                Ok(Kept::Saved(path)) => {
                    if let Err(err) = download::mark_from_internet(path, &manifest.url) {
                        let _ = send.send(Event::Log(format!(
                            "update: {} is kept without the mark that says it came from the internet: {err}",
                            path.display()
                        )));
                    }
                    format!(
                        "update: saved {}, sha256 {} as signed",
                        path.display(),
                        manifest.sha256_hex()
                    )
                }
                Ok(Kept::AlreadyThere(path)) => format!(
                    "update: {} was already there with the signed sha256",
                    path.display()
                ),
                Err(err) => format!("update: download failed: {err}"),
            };
            let _ = send.send(Event::Log(line));
            let _ = send.send(Event::Downloaded(result));
        });
    }

    pub fn shown(&self) -> Option<Shown> {
        let plain = |text: String, download: bool| Shown {
            text,
            tone: Tone::Plain,
            download,
        };
        Some(match &self.state {
            State::Quiet | State::Checking => return None,
            State::CheckFailed(why) => Shown {
                text: why.clone(),
                tone: Tone::Quiet,
                download: false,
            },
            State::Newer(manifest) => plain(
                format!(
                    "Booth {} is out. You have {}.",
                    manifest.version, self.running
                ),
                true,
            ),
            State::Downloading {
                manifest,
                got,
                total,
            } => plain(downloading(manifest.version, *got, *total), false),
            State::Saved(sentence) => plain(sentence.clone(), false),
            State::DownloadFailed { why, .. } => Shown {
                text: why.clone(),
                tone: Tone::Problem,
                download: true,
            },
        })
    }
}

const MB: u64 = 1024 * 1024;

fn downloading(version: Version, got: u64, total: Option<u64>) -> String {
    match total {
        Some(total) => format!(
            "Downloading Booth {version}: {} of {} MB.",
            got / MB,
            total.div_ceil(MB)
        ),
        None => format!("Downloading Booth {version}: {} MB so far.", got / MB),
    }
}

fn saved(kept: &Kept, own: Option<PathBuf>) -> String {
    let next = match own {
        Some(folder) => format!(
            "Close Booth and unzip it into {}, over this copy.",
            folder.display()
        ),
        None => String::from("Close Booth and unzip it over this copy."),
    };
    match kept {
        Kept::Saved(path) => format!("Saved {}. {next}", path.display()),
        Kept::AlreadyThere(path) => format!(
            "{} is already there and matches the signed release. {next}",
            path.display()
        ),
    }
}

fn clause(err: &io::Error) -> String {
    err.to_string().trim_end().trim_end_matches('.').to_owned()
}

fn download_failed(manifest: &Manifest, err: &DownloadError) -> String {
    let version = manifest.version;
    match err {
        DownloadError::Fetch(FetchError::Stopped) => String::from(
            "The download stopped when a room opened. Press Download again after you leave.",
        ),
        DownloadError::Fetch(FetchError::TooBig(most)) => format!(
            "Could not download Booth {version}: it is larger than the {} MB Booth takes. Get it from {RELEASES_PAGE} instead.",
            most / MB
        ),
        DownloadError::Fetch(err) => {
            format!("Could not download Booth {version}: {err}. Press Download to try again.")
        }
        DownloadError::Mismatch => String::from(
            "The download did not match the signed release, so Booth deleted it. Press Download to try again.",
        ),
        DownloadError::Taken(path) => format!(
            "A different {} is already in {}. Move it away, then press Download again.",
            manifest.zip,
            path.parent().unwrap_or(path).display()
        ),
        DownloadError::NoDownloads(err) => format!(
            "Could not find the Downloads folder: {}. Press Download to try again.",
            clause(err)
        ),
        DownloadError::Part { path, source } => format!(
            "Could not write {}: {}. Check that the disk has room, then press Download again.",
            path.display(),
            clause(source)
        ),
        DownloadError::Move { to, source } => format!(
            "Could not move the download to {}: {}. Press Download to try again.",
            to.display(),
            clause(source)
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::manifest::tests::{
        NEWER, NEWER_SIGNED, SAME, SAME_SIGNED, TEST_KEY, UNKNOWN_KEY, UNKNOWN_KEY_SIGNED,
    };
    use super::*;

    struct Folder(PathBuf);

    impl Folder {
        fn new(name: &str) -> Folder {
            let path =
                std::env::temp_dir().join(format!("booth-check-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Folder(path)
        }
    }

    impl Drop for Folder {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    const NOW: u64 = 1_791_936_000;
    const RUNNING: Version = Version::new(0, 1, 0);

    // Stands in for GitHub: the two files, and a count of what was asked.
    struct Server {
        manifest: Vec<u8>,
        signature: Vec<u8>,
        asked: Vec<(String, usize)>,
    }

    impl Server {
        fn new(manifest: &str, signature: &str) -> Server {
            Server {
                manifest: manifest.as_bytes().to_vec(),
                signature: signature.as_bytes().to_vec(),
                asked: Vec::new(),
            }
        }

        fn check(&mut self, dir: &Path, key: &str, now: u64) -> (Checked, Vec<String>) {
            let mut log = Vec::new();
            let mut fetch = |url: &str, most: usize| {
                self.asked.push((url.to_owned(), most));
                if url == latest_url(MANIFEST_FILE) {
                    Ok(self.manifest.clone())
                } else if url == latest_url(SIGNATURE_FILE) {
                    Ok(self.signature.clone())
                } else {
                    panic!("{url} is not one Booth fetches")
                }
            };
            let checked = check(dir, RUNNING, key, now, &mut fetch, &mut log);
            (checked, log)
        }
    }

    fn newer() -> Checked {
        Checked::Newer(Box::new(manifest::parse(NEWER.as_bytes()).unwrap()))
    }

    // GitHub's "latest" address follows the newest release that is not a
    // pre-release, so the two files always come from one release unless
    // one is published between the two fetches, which fails the signature.
    #[test]
    fn the_files_come_from_the_newest_release_on_github() {
        assert!(RELEASES_PAGE.starts_with("https://github.com/"));
        assert!(RELEASES_PAGE.ends_with("/releases"));
        assert_eq!(
            latest_url(MANIFEST_FILE),
            format!("{RELEASES_PAGE}/latest/download/latest.txt")
        );
        assert_eq!(
            latest_url(SIGNATURE_FILE),
            format!("{RELEASES_PAGE}/latest/download/latest.txt.minisig")
        );
    }

    #[test]
    fn once_a_day_either_way() {
        assert!(due(None, NOW));
        assert!(!due(Some(NOW), NOW));
        assert!(!due(Some(NOW - DAY_SECS + 1), NOW));
        assert!(due(Some(NOW - DAY_SECS), NOW));
        assert!(due(Some(0), NOW));
        // The clock went back an hour, then by more than a day.
        assert!(!due(Some(NOW + 3600), NOW));
        assert!(due(Some(NOW + DAY_SECS), NOW));
    }

    #[test]
    fn newer_release_kept_for_the_day() {
        let folder = Folder::new("newer");
        let mut server = Server::new(NEWER, NEWER_SIGNED);
        let (checked, _) = server.check(&folder.0, TEST_KEY, NOW);
        assert_eq!(checked, newer());
        assert_eq!(
            server.asked,
            [
                (latest_url(MANIFEST_FILE), MOST_MANIFEST_BYTES),
                (latest_url(SIGNATURE_FILE), MOST_SIGNATURE_BYTES)
            ]
        );
        assert_eq!(
            fs::read_to_string(folder.0.join(LAST_CHECK)).unwrap(),
            format!("checked = {NOW}\r\n")
        );

        // Started again that day: nothing is fetched, and the kept release
        // is checked again from its signed files.
        let (checked, log) = server.check(&folder.0, TEST_KEY, NOW + 3600);
        assert_eq!(checked, newer());
        assert_eq!(server.asked.len(), 2, "{log:?}");

        // Its files changed on disk are not believed, and go.
        fs::write(
            folder.0.join(MANIFEST_FILE),
            NEWER.replace("0.2.0\n", "0.9.0\n"),
        )
        .unwrap();
        let (checked, _) = server.check(&folder.0, TEST_KEY, NOW + 7200);
        assert_eq!(checked, Checked::Newest);
        assert!(!folder.0.join(MANIFEST_FILE).exists());
        assert_eq!(server.asked.len(), 2);
    }

    #[test]
    fn already_newest() {
        let folder = Folder::new("newest");
        Server::new(NEWER, NEWER_SIGNED).check(&folder.0, TEST_KEY, NOW);
        assert!(folder.0.join(MANIFEST_FILE).exists());
        let mut server = Server::new(SAME, SAME_SIGNED);
        let (checked, _) = server.check(&folder.0, TEST_KEY, NOW + DAY_SECS);
        assert_eq!(checked, Checked::Newest);
        assert_eq!(server.asked.len(), 2);
        assert!(!folder.0.join(MANIFEST_FILE).exists());
        assert!(!folder.0.join(SIGNATURE_FILE).exists());
    }

    // The record of the check is written first, so a check that fails, or
    // a server that answers with junk, is not asked again until tomorrow.
    #[test]
    fn bad_answer_counts_as_checked() {
        let cases = [
            (
                NEWER.replace("0.2.0\npublished", "0.3.0\npublished"),
                NEWER_SIGNED.to_owned(),
                "Could not check for a new version: latest.txt does not match its signature. Booth looks again after a day.",
            ),
            (
                UNKNOWN_KEY.to_owned(),
                UNKNOWN_KEY_SIGNED.to_owned(),
                "Could not check for a new version: latest.txt line 6 has \"note\", which is not a key this version knows. Booth looks again after a day.",
            ),
            (
                NEWER.to_owned(),
                String::from("<html>Not Found</html>"),
                "Could not check for a new version: latest.txt.minisig is not a minisign signature. Booth looks again after a day.",
            ),
        ];
        for (i, (manifest, signature, sentence)) in cases.into_iter().enumerate() {
            let folder = Folder::new(&format!("bad-{i}"));
            let mut server = Server::new(&manifest, &signature);
            let (checked, _) = server.check(&folder.0, TEST_KEY, NOW);
            assert_eq!(checked, Checked::Failed(String::from(sentence)));
            assert!(!folder.0.join(MANIFEST_FILE).exists());
            let (checked, _) = server.check(&folder.0, TEST_KEY, NOW + 60);
            assert_eq!(checked, Checked::Newest);
            assert_eq!(server.asked.len(), 2, "asked again the same day");
        }
    }

    #[test]
    fn a_fetch_that_fails_or_is_stopped_says_so() {
        let folder = Folder::new("fetch");
        let mut log = Vec::new();
        let mut not_found = |_: &str, _: usize| {
            Err(FetchError::Status {
                host: String::from("github.com"),
                code: 404,
            })
        };
        assert_eq!(
            check(&folder.0, RUNNING, TEST_KEY, NOW, &mut not_found, &mut log),
            Checked::Failed(String::from(
                "Could not check for a new version: github.com answered 404, not found. Booth looks again after a day."
            ))
        );
        let mut stopped = |_: &str, _: usize| Err(FetchError::Stopped);
        let later = NOW + DAY_SECS;
        assert_eq!(
            check(&folder.0, RUNNING, TEST_KEY, later, &mut stopped, &mut log),
            Checked::Stopped
        );
    }

    #[test]
    fn without_the_release_key_nothing_is_fetched() {
        let folder = Folder::new("no-key");
        let mut server = Server::new(NEWER, NEWER_SIGNED);
        let (checked, _) = server.check(&folder.0, "not made yet", NOW);
        assert_eq!(
            checked,
            Checked::Failed(String::from(
                "This copy of Booth was built without the release key, so it cannot check for new versions."
            ))
        );
        assert!(server.asked.is_empty());
    }

    #[test]
    fn unrecorded_check_not_made() {
        let folder = Folder::new("unwritable");
        // Windows will not write a file over a folder.
        fs::create_dir(folder.0.join(LAST_CHECK)).unwrap();
        let mut server = Server::new(NEWER, NEWER_SIGNED);
        let (checked, log) = server.check(&folder.0, TEST_KEY, NOW);
        let Checked::Failed(sentence) = checked else {
            panic!("{checked:?}");
        };
        assert!(
            sentence.starts_with("Could not check for a new version: "),
            "{sentence}"
        );
        assert!(server.asked.is_empty());
        assert_eq!(log.len(), 1, "{log:?}");
    }

    #[test]
    fn foreign_record_ignored() {
        let folder = Folder::new("record");
        // Would read as NOW if it were read to the end.
        let long = format!("checked = {}{NOW}\r\n", "0".repeat(MOST_RECORD_BYTES));
        for text in [
            "checked = \r\n",
            "checked = 12x\r\n",
            "checked=1\r\n",
            "1791936000",
            long.as_str(),
        ] {
            fs::write(folder.0.join(LAST_CHECK), text).unwrap();
            let mut log = Vec::new();
            assert_eq!(last_check(&folder.0, &mut log), None, "{text:?}");
            assert_eq!(log.len(), 1, "{text:?}");
        }
        fs::write(folder.0.join(LAST_CHECK), "checked = 1791936000\r\n").unwrap();
        assert_eq!(last_check(&folder.0, &mut Vec::new()), Some(NOW));
    }

    #[test]
    fn a_leftover_part_file_is_removed_at_start() {
        let folder = Folder::new("tidy");
        assert_eq!(tidy(&folder.0), None);
        fs::write(download::part_path(&folder.0), b"half").unwrap();
        assert!(tidy(&folder.0).is_some());
        assert!(!download::part_path(&folder.0).exists());
    }

    fn update_in(state: State) -> Update {
        Update {
            state,
            running: RUNNING,
            worker: None,
        }
    }

    fn manifest() -> Box<Manifest> {
        Box::new(manifest::parse(NEWER.as_bytes()).unwrap())
    }

    #[test]
    fn the_start_screen_line_follows_each_step() {
        let mut update = update_in(State::Checking);
        assert_eq!(update.shown(), None);
        update.apply(Event::Checked(newer()));
        assert_eq!(
            update.shown(),
            Some(Shown {
                text: String::from("Booth 0.2.0 is out. You have 0.1.0."),
                tone: Tone::Plain,
                download: true,
            })
        );

        update.state = State::Downloading {
            manifest: manifest(),
            got: 0,
            total: None,
        };
        update.apply(Event::Progress {
            got: 12 * MB + 5,
            total: Some(40 * MB + 1),
        });
        let shown = update.shown().unwrap();
        assert_eq!(shown.text, "Downloading Booth 0.2.0: 12 of 41 MB.");
        assert!(!shown.download);
        update.apply(Event::Progress {
            got: 3 * MB,
            total: None,
        });
        assert_eq!(
            update.shown().unwrap().text,
            "Downloading Booth 0.2.0: 3 MB so far."
        );

        let saved = PathBuf::from(r"C:\Users\Mara\Downloads\booth-0.2.0-windows-x64.zip");
        update.apply(Event::Downloaded(Ok(Kept::Saved(saved))));
        let shown = update.shown().unwrap();
        assert!(
            shown
                .text
                .starts_with(r"Saved C:\Users\Mara\Downloads\booth-0.2.0-windows-x64.zip. Close Booth and unzip it into "),
            "{}",
            shown.text
        );
        assert!(shown.text.ends_with(", over this copy."), "{}", shown.text);
        assert!(!shown.download);
    }

    #[test]
    fn every_download_failure_is_a_sentence_with_download_again() {
        let path = PathBuf::from(r"C:\Users\Mara\Downloads\booth-0.2.0-windows-x64.zip");
        let cases = [
            (
                DownloadError::Mismatch,
                "The download did not match the signed release, so Booth deleted it. Press Download to try again.",
            ),
            (
                DownloadError::Fetch(FetchError::Stopped),
                "The download stopped when a room opened. Press Download again after you leave.",
            ),
            (
                DownloadError::Taken(path.clone()),
                r"A different booth-0.2.0-windows-x64.zip is already in C:\Users\Mara\Downloads. Move it away, then press Download again.",
            ),
            (
                DownloadError::Fetch(FetchError::TooBig(download::MOST_ZIP_BYTES)),
                "Could not download Booth 0.2.0: it is larger than the 256 MB Booth takes. Get it from https://github.com/Shadi-Alrashoodi/booth/releases instead.",
            ),
            (
                DownloadError::Fetch(FetchError::Status {
                    host: String::from("github.com"),
                    code: 503,
                }),
                "Could not download Booth 0.2.0: github.com answered 503. Press Download to try again.",
            ),
        ];
        for (err, sentence) in cases {
            let mut update = update_in(State::Downloading {
                manifest: manifest(),
                got: 0,
                total: None,
            });
            update.apply(Event::Downloaded(Err(err)));
            assert_eq!(
                update.shown(),
                Some(Shown {
                    text: String::from(sentence),
                    tone: Tone::Problem,
                    download: true,
                })
            );
        }
    }

    #[test]
    fn failed_and_stopped_checks() {
        let mut update = update_in(State::Checking);
        update.apply(Event::Checked(Checked::Failed(String::from("Could not."))));
        assert_eq!(update.shown().map(|shown| shown.tone), Some(Tone::Quiet));
        let mut update = update_in(State::Checking);
        update.apply(Event::Checked(Checked::Stopped));
        assert_eq!(update.shown(), None);
    }

    // A room opening sets the worker's stop once, and the worker's last
    // word still lands.
    #[test]
    fn room_stops_worker() {
        let (send, events) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let mut update = update_in(State::Downloading {
            manifest: manifest(),
            got: 0,
            total: None,
        });
        update.worker = Some(Worker {
            events,
            stop: Arc::clone(&stop),
        });
        assert!(update.room_opened().is_some());
        assert!(stop.load(Ordering::Acquire));
        assert_eq!(update.room_opened(), None);
        send.send(Event::Log(String::from(
            "update: download failed: a room opened",
        )))
        .unwrap();
        send.send(Event::Downloaded(Err(DownloadError::Fetch(
            FetchError::Stopped,
        ))))
        .unwrap();
        assert_eq!(update.follow(), ["update: download failed: a room opened"]);
        assert!(update.worker.is_none());
        assert_eq!(update.room_opened(), None);
        assert!(update.shown().unwrap().download);
    }

    fn follows(update: &mut Update) -> (Sender<Event>, Arc<AtomicBool>) {
        let (send, events) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        update.worker = Some(Worker {
            events,
            stop: Arc::clone(&stop),
        });
        (send, stop)
    }

    #[test]
    fn worker_without_answer() {
        let mut update = update_in(State::Downloading {
            manifest: manifest(),
            got: 0,
            total: None,
        });
        let (send, _) = follows(&mut update);
        send.send(Event::Progress {
            got: MB,
            total: None,
        })
        .unwrap();
        assert!(update.follow().is_empty());
        assert!(update.worker.is_some(), "still running");
        drop(send);
        assert_eq!(
            update.follow(),
            ["update: the update step ended without an answer"]
        );
        assert!(update.worker.is_none());
        assert_eq!(
            update.shown(),
            Some(Shown {
                text: String::from(
                    "Could not download Booth 0.2.0: an error in Booth stopped it. Press Download to try again."
                ),
                tone: Tone::Problem,
                download: true,
            })
        );

        let mut update = update_in(State::Checking);
        drop(follows(&mut update));
        update.follow();
        assert_eq!(
            update.shown().map(|shown| (shown.text, shown.tone)),
            Some((
                String::from("Could not check for a new version: an error in Booth stopped it."),
                Tone::Quiet
            ))
        );
    }

    // The wake comes after the worker's sender is gone, so the pass it
    // brings already sees the end.
    #[test]
    fn a_worker_that_panics_still_wakes_the_screen() {
        let (woke, wakes) = mpsc::channel();
        let notify: Notify = Arc::new(move || {
            let _ = woke.send(());
        });
        let mut update = update_in(State::Checking);
        update.spawn("update test", notify, |_, _| {
            panic!("a panic the test makes on purpose")
        });
        wakes.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(update.follow().len(), 1);
        assert!(update.worker.is_none());
        assert_eq!(update.shown().map(|shown| shown.tone), Some(Tone::Quiet));
    }

    #[test]
    fn download_only_for_found_release() {
        let notify: Notify = Arc::new(|| {});
        let mut update = update_in(State::Quiet);
        update.download(std::env::temp_dir(), Arc::clone(&notify));
        assert!(update.worker.is_none());
        let mut update = update_in(State::CheckFailed(String::from("Could not.")));
        update.download(std::env::temp_dir(), notify);
        assert!(update.worker.is_none());
    }
}
