// One GET over HTTPS with WinHTTP, Windows' own HTTP client: it follows the
// PC's proxy settings and trusts what Windows trusts, and it needs no crate.
// The request carries the host, the program name Booth and nothing else: no
// cookies, no credentials, no Referer. Redirects are followed here and not
// by WinHTTP, so each one goes only to https on port 443, and a room opening
// or the deadline passing is seen before every connection and every wait.

use std::ffi::c_void;
use std::fmt;
use std::io::{self, Read};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use windows_sys::Win32::Networking::WinHttp::{
    ERROR_WINHTTP_AUTO_PROXY_SERVICE_ERROR, ERROR_WINHTTP_CANNOT_CONNECT,
    ERROR_WINHTTP_CONNECTION_ERROR, ERROR_WINHTTP_INVALID_SERVER_RESPONSE,
    ERROR_WINHTTP_NAME_NOT_RESOLVED, ERROR_WINHTTP_SECURE_CERT_CN_INVALID,
    ERROR_WINHTTP_SECURE_CERT_DATE_INVALID, ERROR_WINHTTP_SECURE_CERT_REV_FAILED,
    ERROR_WINHTTP_SECURE_CERT_REVOKED, ERROR_WINHTTP_SECURE_CERT_WRONG_USAGE,
    ERROR_WINHTTP_SECURE_CHANNEL_ERROR, ERROR_WINHTTP_SECURE_FAILURE,
    ERROR_WINHTTP_SECURE_FAILURE_PROXY, ERROR_WINHTTP_SECURE_INVALID_CA,
    ERROR_WINHTTP_SECURE_INVALID_CERT, ERROR_WINHTTP_TIMEOUT, WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
    WINHTTP_DISABLE_AUTHENTICATION, WINHTTP_DISABLE_COOKIES, WINHTTP_FLAG_REFRESH,
    WINHTTP_FLAG_SECURE, WINHTTP_FLAG_SECURE_PROTOCOL_TLS1_2, WINHTTP_FLAG_SECURE_PROTOCOL_TLS1_3,
    WINHTTP_OPTION_DISABLE_FEATURE, WINHTTP_OPTION_RECEIVE_RESPONSE_TIMEOUT,
    WINHTTP_OPTION_RECEIVE_TIMEOUT, WINHTTP_OPTION_REDIRECT_POLICY,
    WINHTTP_OPTION_REDIRECT_POLICY_NEVER, WINHTTP_OPTION_SECURE_PROTOCOLS,
    WINHTTP_QUERY_CONTENT_LENGTH, WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_LOCATION,
    WINHTTP_QUERY_STATUS_CODE, WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest,
    WinHttpQueryHeaders, WinHttpReadData, WinHttpReceiveResponse, WinHttpSendRequest,
    WinHttpSetOption, WinHttpSetTimeouts,
};

// GitHub sends a release asset on through two redirects.
const MOST_REDIRECTS: u32 = 5;
// GitHub's signed address for an asset is about 1 KB.
const MOST_LOCATION_BYTES: u32 = 16 * 1024;
const CHUNK: usize = 64 * 1024;

pub struct Rules {
    // Bytes of body, past which the fetch stops and fails.
    pub most: u64,
    // For each single wait on the network: the name lookup, the connection,
    // sending, the answer's headers and each read.
    pub wait: Duration,
    // For the whole fetch, redirects included; every wait is cut to what is
    // left of it. None for the zip, which can take minutes on a slow line
    // and is ended by a stalled read or a room opening instead.
    pub deadline: Option<Duration>,
    // Asks caches on the way for a fresh copy, for latest.txt.
    pub fresh: bool,
}

#[derive(Debug)]
pub enum FetchError {
    // Not an address of the one shape Booth fetches from.
    Address,
    Io { host: String, source: io::Error },
    Status { host: String, code: u64 },
    // A redirect to somewhere other than https on port 443.
    Redirect { host: String },
    Redirects { host: String },
    TooBig(u64),
    TooSlow { host: String, seconds: u64 },
    Stopped,
    Write(io::Error),
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FetchError::Address => write!(f, "the address is not one Booth fetches from"),
            FetchError::Io { host, source } => f.write_str(&io_text(host, source)),
            FetchError::Status { host, code: 404 } => write!(f, "{host} answered 404, not found"),
            // WINHTTP_DISABLE_AUTHENTICATION: Booth never sends a sign-in,
            // not even the PC's own to a proxy.
            FetchError::Status { code: 407, .. } => {
                write!(
                    f,
                    "the proxy asked Booth to sign in, which Booth does not do"
                )
            }
            FetchError::Status { host, code } => write!(f, "{host} answered {code}"),
            FetchError::Redirect { host } => {
                write!(f, "{host} sent Booth on to an address it does not follow")
            }
            FetchError::Redirects { host } => {
                write!(f, "{host} sent Booth on more than {MOST_REDIRECTS} times")
            }
            FetchError::TooBig(most) => {
                write!(f, "the file is larger than the {most} bytes Booth takes")
            }
            FetchError::TooSlow { host, seconds } => {
                write!(f, "{host} took longer than {seconds} seconds")
            }
            FetchError::Stopped => write!(f, "a room opened"),
            FetchError::Write(err) => write!(f, "the download could not be written: {err}"),
        }
    }
}

// WinHTTP's own error numbers are not in the system's message table, so
// io::Error would only say "unknown error".
fn io_text(host: &str, err: &io::Error) -> String {
    let code = err.raw_os_error().and_then(|code| u32::try_from(code).ok());
    match code {
        Some(ERROR_WINHTTP_TIMEOUT) => format!("the connection to {host} timed out"),
        Some(ERROR_WINHTTP_NAME_NOT_RESOLVED) => {
            format!("{host} could not be found, which usually means this PC is offline")
        }
        Some(ERROR_WINHTTP_CANNOT_CONNECT) => format!("no connection could be made to {host}"),
        Some(ERROR_WINHTTP_CONNECTION_ERROR) => format!("the connection to {host} was cut"),
        Some(
            ERROR_WINHTTP_SECURE_FAILURE
            | ERROR_WINHTTP_SECURE_CERT_CN_INVALID
            | ERROR_WINHTTP_SECURE_CERT_DATE_INVALID
            | ERROR_WINHTTP_SECURE_CERT_REV_FAILED
            | ERROR_WINHTTP_SECURE_CERT_REVOKED
            | ERROR_WINHTTP_SECURE_CERT_WRONG_USAGE
            | ERROR_WINHTTP_SECURE_INVALID_CA
            | ERROR_WINHTTP_SECURE_INVALID_CERT
            | ERROR_WINHTTP_SECURE_CHANNEL_ERROR,
        ) => format!("Windows did not accept the secure connection to {host}"),
        Some(ERROR_WINHTTP_SECURE_FAILURE_PROXY) => {
            String::from("Windows did not accept the secure connection to the proxy")
        }
        Some(ERROR_WINHTTP_INVALID_SERVER_RESPONSE) => {
            format!("the answer from {host} could not be read")
        }
        Some(ERROR_WINHTTP_AUTO_PROXY_SERVICE_ERROR) => {
            String::from("the Windows proxy service did not answer")
        }
        Some(code) if (12000..13000).contains(&code) => {
            format!("WinHTTP error {code} while fetching from {host}")
        }
        _ => format!(
            "{} while fetching from {host}",
            err.to_string().trim_end_matches('.')
        ),
    }
}

// https://host/path, as the constants and a checked manifest have it. The
// host has no port and no user name in it.
fn split_https(url: &str) -> Option<(&str, &str)> {
    let rest = url.strip_prefix("https://")?;
    let slash = rest.find('/')?;
    let (host, path) = rest.split_at(slash);
    let plain = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'.' || byte == b'-';
    (!host.is_empty() && host.bytes().all(plain)).then_some((host, path))
}

// Where a redirect sends the fetch: an https address of the same shape as
// the first, or a path on the same host. The fragment is never sent.
fn redirect_target(host: &str, location: &str) -> Option<String> {
    let location = location
        .split_once('#')
        .map_or(location, |(before, _)| before);
    if !location.bytes().all(|byte| (0x21..=0x7e).contains(&byte)) {
        return None;
    }
    let url = match location.strip_prefix('/') {
        Some(rest) if !rest.starts_with('/') => format!("https://{host}{location}"),
        _ => location.to_owned(),
    };
    split_https(&url)?;
    Some(url)
}

// The time one fetch has. WinHTTP's own calls do the name lookup, the
// connection and the TLS handshake out of sight, so the deadline holds only
// if each of their waits is cut to what is left of it.
#[derive(Clone, Copy)]
struct Clock {
    wait: Duration,
    until: Option<Instant>,
    seconds: u64,
}

impl Clock {
    fn new(rules: &Rules) -> Clock {
        Clock {
            wait: rules.wait,
            until: rules.deadline.map(|deadline| Instant::now() + deadline),
            seconds: rules.deadline.map_or(0, |deadline| deadline.as_secs()),
        }
    }

    // None once the deadline has passed.
    fn next_wait(&self) -> Option<Duration> {
        let Some(until) = self.until else {
            return Some(self.wait);
        };
        let left = until.saturating_duration_since(Instant::now());
        (!left.is_zero()).then(|| left.min(self.wait))
    }

    fn too_slow(&self, host: &str) -> FetchError {
        FetchError::TooSlow {
            host: host.to_owned(),
            seconds: self.seconds,
        }
    }

    // Asked before each step, and the answer is the wait that step gets.
    fn go_on(&self, stop: &AtomicBool, host: &str) -> Result<Duration, FetchError> {
        if stop.load(Ordering::Acquire) {
            return Err(FetchError::Stopped);
        }
        self.next_wait().ok_or_else(|| self.too_slow(host))
    }

    // A call that failed once the deadline had passed timed out because its
    // wait was cut to it, and says so in the deadline's words.
    fn failed(&self, host: &str) -> FetchError {
        let err = last_error(host);
        if self.next_wait().is_none() {
            return self.too_slow(host);
        }
        err
    }
}

// WinHTTP reads 0 as no limit at all.
fn millis(wait: Duration) -> i32 {
    i32::try_from(wait.as_millis()).unwrap_or(i32::MAX).max(1)
}

struct Handle(*mut c_void);

impl Handle {
    fn new(raw: *mut c_void, host: &str) -> Result<Handle, FetchError> {
        if raw.is_null() {
            return Err(last_error(host));
        }
        Ok(Handle(raw))
    }

    fn set(&self, option: u32, value: u32) -> bool {
        // SAFETY: a live handle, and a DWORD option read from a u32 that
        // lives across the call.
        unsafe { WinHttpSetOption(self.0, option, (&raw const value).cast(), 4) != 0 }
    }

    // WinHttpSetTimeouts leaves out the wait for the answer's headers, which
    // is 90 s unless set, so both waits for the answer are set here.
    fn set_answer_waits(&self, wait: Duration) -> bool {
        let wait = millis(wait).unsigned_abs();
        self.set(WINHTTP_OPTION_RECEIVE_RESPONSE_TIMEOUT, wait)
            && self.set(WINHTTP_OPTION_RECEIVE_TIMEOUT, wait)
    }

    // A header as a number; None when the answer has no such header. 32 bits
    // is plenty: the largest file Booth takes is 256 MB, and a length past
    // what 32 bits hold is refused while reading instead.
    fn number(&self, header: u32) -> Option<u64> {
        let mut value = 0u32;
        let mut size = 4u32;
        // SAFETY: a live request handle with its answer received; the buffer
        // is a u32 and size says so.
        let ok = unsafe {
            WinHttpQueryHeaders(
                self.0,
                header | WINHTTP_QUERY_FLAG_NUMBER,
                ptr::null(),
                (&raw mut value).cast(),
                &mut size,
                ptr::null_mut(),
            )
        };
        (ok != 0).then_some(u64::from(value))
    }

    // None when the answer has no Location, or one longer than any address
    // Booth follows.
    fn location(&self) -> Option<String> {
        let mut size = 0u32;
        // SAFETY: a live request handle with its answer received; with no
        // buffer WinHTTP only writes the size it needs, in bytes.
        unsafe {
            WinHttpQueryHeaders(
                self.0,
                WINHTTP_QUERY_LOCATION,
                ptr::null(),
                ptr::null_mut(),
                &mut size,
                ptr::null_mut(),
            )
        };
        if size == 0 || size > MOST_LOCATION_BYTES {
            return None;
        }
        let mut units = vec![0u16; (size as usize).div_ceil(2)];
        // SAFETY: the buffer holds at least `size` bytes, as size says.
        let ok = unsafe {
            WinHttpQueryHeaders(
                self.0,
                WINHTTP_QUERY_LOCATION,
                ptr::null(),
                units.as_mut_ptr().cast(),
                &mut size,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            return None;
        }
        units.truncate(size as usize / 2);
        String::from_utf16(&units).ok()
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: made by WinHTTP and closed once, here.
        unsafe { WinHttpCloseHandle(self.0) };
    }
}

fn last_error(host: &str) -> FetchError {
    FetchError::Io {
        host: host.to_owned(),
        source: io::Error::last_os_error(),
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain([0]).collect()
}

// A request made up with every option and not sent yet. Making one touches
// no network, which is what lets a test check that WinHTTP takes each
// option on the handle it is set on.
struct Request {
    // Dropped in this order: the request before its connection before its
    // session.
    request: Handle,
    _connection: Handle,
    _session: Handle,
    host: String,
}

fn request(url: &str, fresh: bool, wait: Duration) -> Result<Request, FetchError> {
    let (host, path) = split_https(url).ok_or(FetchError::Address)?;
    let program_name = wide("Booth");
    // SAFETY: the program name is zero terminated and outlives the call; no
    // proxy name or bypass list is given, so both are null.
    let session = Handle::new(
        unsafe {
            WinHttpOpen(
                program_name.as_ptr(),
                WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
                ptr::null(),
                ptr::null(),
                0,
            )
        },
        host,
    )?;
    let wait_ms = millis(wait);
    // SAFETY: a live session handle and plain numbers.
    if unsafe { WinHttpSetTimeouts(session.0, wait_ms, wait_ms, wait_ms, wait_ms) } == 0 {
        return Err(last_error(host));
    }
    // TLS 1.3 where Windows has it, and 1.2, which GitHub needs at least.
    let tls13 = WINHTTP_FLAG_SECURE_PROTOCOL_TLS1_2 | WINHTTP_FLAG_SECURE_PROTOCOL_TLS1_3;
    if !session.set(WINHTTP_OPTION_SECURE_PROTOCOLS, tls13)
        && !session.set(
            WINHTTP_OPTION_SECURE_PROTOCOLS,
            WINHTTP_FLAG_SECURE_PROTOCOL_TLS1_2,
        )
    {
        return Err(last_error(host));
    }
    let server = wide(host);
    // SAFETY: a live session and a zero-terminated host that outlives the
    // call. 443 is the https port; a URL here never names another.
    let connection = Handle::new(
        unsafe { WinHttpConnect(session.0, server.as_ptr(), 443, 0) },
        host,
    )?;
    let verb = wide("GET");
    let object = wide(path);
    let mut flags = WINHTTP_FLAG_SECURE;
    if fresh {
        flags |= WINHTTP_FLAG_REFRESH;
    }
    // SAFETY: a live connection and zero-terminated strings that outlive
    // the call. Null version, referrer and accept types mean HTTP/1.1, no
    // Referer and no Accept header.
    let request = Handle::new(
        unsafe {
            WinHttpOpenRequest(
                connection.0,
                verb.as_ptr(),
                object.as_ptr(),
                ptr::null(),
                ptr::null(),
                ptr::null(),
                flags,
            )
        },
        host,
    )?;
    let kept_quiet = request.set(
        WINHTTP_OPTION_DISABLE_FEATURE,
        WINHTTP_DISABLE_COOKIES | WINHTTP_DISABLE_AUTHENTICATION,
    ) && request.set(
        WINHTTP_OPTION_REDIRECT_POLICY,
        WINHTTP_OPTION_REDIRECT_POLICY_NEVER,
    ) && request.set_answer_waits(wait);
    if !kept_quiet {
        return Err(last_error(host));
    }
    Ok(Request {
        request,
        _connection: connection,
        _session: session,
        host: host.to_owned(),
    })
}

// An answer whose status and length were fine, ready to be read.
pub struct Body {
    request: Request,
    length: Option<u64>,
    clock: Clock,
}

impl Body {
    pub fn length(&self) -> Option<u64> {
        self.length
    }
}

impl Read for Body {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.clock.until.is_some() {
            let wait = self.clock.next_wait().ok_or(io::ErrorKind::TimedOut)?;
            if !self.request.request.set_answer_waits(wait) {
                return Err(io::Error::last_os_error());
            }
        }
        let len = u32::try_from(buffer.len()).unwrap_or(u32::MAX);
        let mut read = 0u32;
        // SAFETY: a live request handle; the buffer is writable for len
        // bytes and read receives how many were written.
        let ok = unsafe {
            WinHttpReadData(
                self.request.request.0,
                buffer.as_mut_ptr().cast(),
                len,
                &mut read,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(read as usize)
    }
}

pub fn open(url: &str, rules: &Rules, stop: &AtomicBool) -> Result<Body, FetchError> {
    let clock = Clock::new(rules);
    let mut url = url.to_owned();
    let mut redirects = 0;
    loop {
        let (host, _) = split_https(&url).ok_or(FetchError::Address)?;
        let made = request(&url, rules.fresh, clock.go_on(stop, host)?)?;
        let handle = &made.request;
        // SAFETY: a live request; no extra headers and no body.
        if unsafe { WinHttpSendRequest(handle.0, ptr::null(), 0, ptr::null(), 0, 0, 0) } == 0 {
            return Err(clock.failed(host));
        }
        if !handle.set_answer_waits(clock.go_on(stop, host)?) {
            return Err(last_error(host));
        }
        // SAFETY: the request was sent; the reserved argument must be null.
        if unsafe { WinHttpReceiveResponse(handle.0, ptr::null_mut()) } == 0 {
            return Err(clock.failed(host));
        }
        let code = handle
            .number(WINHTTP_QUERY_STATUS_CODE)
            .ok_or_else(|| last_error(host))?;
        if matches!(code, 301 | 302 | 303 | 307 | 308) {
            if redirects == MOST_REDIRECTS {
                return Err(FetchError::Redirects {
                    host: host.to_owned(),
                });
            }
            redirects += 1;
            url = handle
                .location()
                .and_then(|location| redirect_target(host, &location))
                .ok_or_else(|| FetchError::Redirect {
                    host: host.to_owned(),
                })?;
            continue;
        }
        if code != 200 {
            return Err(FetchError::Status {
                host: host.to_owned(),
                code,
            });
        }
        let length = handle.number(WINHTTP_QUERY_CONTENT_LENGTH);
        if length.is_some_and(|length| length > rules.most) {
            return Err(FetchError::TooBig(rules.most));
        }
        return Ok(Body {
            request: made,
            length,
            clock,
        });
    }
}

pub fn pour_body(
    body: &mut Body,
    most: u64,
    stop: &AtomicBool,
    sink: &mut dyn FnMut(&[u8]) -> io::Result<()>,
) -> Result<u64, FetchError> {
    let host = body.request.host.clone();
    let clock = body.clock;
    pour(body, most, stop, &clock, &host, sink).map_err(|err| match err {
        Poured::Read(_) if clock.next_wait().is_none() => clock.too_slow(&host),
        Poured::Read(source) => FetchError::Io { host, source },
        Poured::Other(err) => err,
    })
}

enum Poured {
    Read(io::Error),
    Other(FetchError),
}

// Separate from WinHTTP so the limits can be tested on bytes in memory.
// Nothing past `most` is handed on, and a stop or the deadline is seen
// between reads.
fn pour(
    source: &mut impl Read,
    most: u64,
    stop: &AtomicBool,
    clock: &Clock,
    host: &str,
    sink: &mut dyn FnMut(&[u8]) -> io::Result<()>,
) -> Result<u64, Poured> {
    let mut buffer = vec![0u8; CHUNK];
    let mut total = 0u64;
    loop {
        clock.go_on(stop, host).map_err(Poured::Other)?;
        let read = match source.read(&mut buffer) {
            Ok(read) => read,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(Poured::Read(err)),
        };
        if read == 0 {
            return Ok(total);
        }
        total += read as u64;
        if total > most {
            return Err(Poured::Other(FetchError::TooBig(most)));
        }
        sink(&buffer[..read]).map_err(|err| Poured::Other(FetchError::Write(err)))?;
    }
}

// A small file whole, for latest.txt and its signature.
pub fn get_small(url: &str, rules: &Rules, stop: &AtomicBool) -> Result<Vec<u8>, FetchError> {
    let mut body = open(url, rules, stop)?;
    let mut bytes = Vec::new();
    pour_body(&mut body, rules.most, stop, &mut |chunk| {
        bytes.extend_from_slice(chunk);
        Ok(())
    })?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Hands out its bytes a few at a time, and can fail after them.
    struct Trickle {
        bytes: Vec<u8>,
        at: usize,
        step: usize,
        then: Option<io::ErrorKind>,
        interrupted_once: bool,
    }

    impl Read for Trickle {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if !self.interrupted_once {
                self.interrupted_once = true;
                return Err(io::ErrorKind::Interrupted.into());
            }
            if self.at == self.bytes.len() {
                return match self.then {
                    Some(kind) => Err(kind.into()),
                    None => Ok(0),
                };
            }
            let n = self.step.min(buffer.len()).min(self.bytes.len() - self.at);
            buffer[..n].copy_from_slice(&self.bytes[self.at..self.at + n]);
            self.at += n;
            Ok(n)
        }
    }

    fn trickle(len: usize, step: usize) -> Trickle {
        Trickle {
            bytes: (0..len).map(|i| i as u8).collect(),
            at: 0,
            step,
            then: None,
            interrupted_once: false,
        }
    }

    fn on_time() -> Clock {
        Clock {
            wait: Duration::from_secs(10),
            until: None,
            seconds: 0,
        }
    }

    fn poured(
        source: &mut Trickle,
        most: u64,
        stop: &AtomicBool,
        clock: &Clock,
    ) -> (Result<u64, Poured>, Vec<u8>) {
        let mut out = Vec::new();
        let result = pour(source, most, stop, clock, "github.com", &mut |chunk| {
            out.extend_from_slice(chunk);
            Ok(())
        });
        (result, out)
    }

    #[test]
    fn a_body_up_to_the_limit_arrives_whole() {
        let stop = AtomicBool::new(false);
        for (len, step) in [(0, 1), (1, 1), (4096, 7), (200_000, 70_000)] {
            let mut source = trickle(len, step);
            let (result, out) = poured(&mut source, len as u64, &stop, &on_time());
            assert_eq!(result.ok(), Some(len as u64), "{len}");
            assert_eq!(out, source.bytes, "{len}");
        }
    }

    #[test]
    fn body_past_the_limit() {
        let stop = AtomicBool::new(false);
        let mut source = trickle(4097, 1000);
        let (result, out) = poured(&mut source, 4096, &stop, &on_time());
        assert!(matches!(
            result,
            Err(Poured::Other(FetchError::TooBig(4096)))
        ));
        assert_eq!(out.len(), 4000, "nothing past the limit is handed on");
    }

    #[test]
    fn a_stop_or_a_deadline_ends_it_between_reads() {
        let stop = AtomicBool::new(true);
        let (result, out) = poured(&mut trickle(10, 1), 100, &stop, &on_time());
        assert!(matches!(result, Err(Poured::Other(FetchError::Stopped))));
        assert!(out.is_empty());

        let stop = AtomicBool::new(false);
        let past = Clock {
            wait: Duration::from_secs(10),
            until: Some(Instant::now() - Duration::from_millis(1)),
            seconds: 20,
        };
        let (result, _) = poured(&mut trickle(10, 1), 100, &stop, &past);
        match result {
            Err(Poured::Other(err @ FetchError::TooSlow { .. })) => {
                assert_eq!(err.to_string(), "github.com took longer than 20 seconds");
            }
            _ => panic!("the deadline did not end it"),
        }
    }

    #[test]
    fn a_failed_read_or_write_says_so() {
        let stop = AtomicBool::new(false);
        let mut source = trickle(10, 4);
        source.then = Some(io::ErrorKind::ConnectionReset);
        let (result, out) = poured(&mut source, 100, &stop, &on_time());
        assert!(
            matches!(result, Err(Poured::Read(err)) if err.kind() == io::ErrorKind::ConnectionReset)
        );
        assert_eq!(out.len(), 10);

        let result = pour(
            &mut trickle(10, 4),
            100,
            &stop,
            &on_time(),
            "github.com",
            &mut |_| Err(io::ErrorKind::StorageFull.into()),
        );
        assert!(matches!(result, Err(Poured::Other(FetchError::Write(_)))));
    }

    // Each wait WinHTTP makes is cut to what is left of the deadline, and
    // none is ever 0, which WinHTTP would read as no limit.
    #[test]
    fn waits_fit_the_deadline() {
        let wait = Duration::from_secs(10);
        let clock = |until| Clock {
            wait,
            until,
            seconds: 30,
        };
        assert_eq!(clock(None).next_wait(), Some(wait));
        let far = clock(Some(Instant::now() + Duration::from_secs(60)));
        assert_eq!(far.next_wait(), Some(wait));
        let near = clock(Some(Instant::now() + Duration::from_secs(3)));
        let left = near.next_wait().unwrap();
        assert!(left <= Duration::from_secs(3) && left > Duration::from_secs(2));
        let past = clock(Some(Instant::now() - Duration::from_millis(1)));
        assert_eq!(past.next_wait(), None);
        assert!(matches!(
            past.go_on(&AtomicBool::new(false), "github.com"),
            Err(FetchError::TooSlow { seconds: 30, .. })
        ));
        assert!(matches!(
            far.go_on(&AtomicBool::new(true), "github.com"),
            Err(FetchError::Stopped)
        ));

        assert_eq!(millis(Duration::ZERO), 1);
        assert_eq!(millis(Duration::from_micros(10)), 1);
        assert_eq!(millis(Duration::from_secs(10)), 10_000);
        assert_eq!(millis(Duration::from_secs(u64::MAX)), i32::MAX);
    }

    // The stop and the deadline are asked before the first connection, so
    // none of these reaches the network.
    #[test]
    fn stop_before_connecting() {
        let url = "https://github.com/OWNER/booth/releases/latest/download/latest.txt";
        let rules = Rules {
            most: 4096,
            wait: Duration::from_secs(10),
            deadline: Some(Duration::from_secs(30)),
            fresh: true,
        };
        assert!(matches!(
            open(url, &rules, &AtomicBool::new(true)),
            Err(FetchError::Stopped)
        ));
        assert!(matches!(
            get_small(url, &rules, &AtomicBool::new(true)),
            Err(FetchError::Stopped)
        ));
        let spent = Rules {
            deadline: Some(Duration::ZERO),
            ..rules
        };
        assert!(matches!(
            open(url, &spent, &AtomicBool::new(false)),
            Err(FetchError::TooSlow { .. })
        ));
        assert!(matches!(
            open(
                "http://github.com/latest.txt",
                &spent,
                &AtomicBool::new(false)
            ),
            Err(FetchError::Address)
        ));
    }

    // WinHTTP takes every option on the handle it is set on. Nothing is
    // sent: until WinHttpSendRequest a request is only handles.
    #[test]
    fn a_request_takes_every_option_without_being_sent() {
        let url = "https://github.com/OWNER/booth/releases/latest/download/latest.txt";
        let wait = Duration::from_secs(10);
        let made = request(url, true, wait).unwrap_or_else(|err| panic!("{err}"));
        assert_eq!(made.host, "github.com");
        assert!(made.request.set_answer_waits(Duration::ZERO));
        assert!(request(url, false, Duration::from_millis(1)).is_ok());
        assert!(matches!(
            request("http://github.com/latest.txt", false, wait),
            Err(FetchError::Address)
        ));
    }

    #[test]
    fn only_https_to_a_plain_host_is_fetched() {
        assert_eq!(
            split_https("https://github.com/OWNER/booth/releases/latest/download/latest.txt"),
            Some((
                "github.com",
                "/OWNER/booth/releases/latest/download/latest.txt"
            ))
        );
        for bad in [
            "http://github.com/latest.txt",
            "https://github.com",
            "https:///latest.txt",
            "https://github.com:443/latest.txt",
            "https://me@github.com/latest.txt",
            "HTTPS://github.com/latest.txt",
        ] {
            assert_eq!(split_https(bad), None, "{bad}");
        }
    }

    #[test]
    fn redirects_only_to_https_443() {
        let asset = "https://release-assets.githubusercontent.com/github-production-release-asset/1/2?sp=r&sig=a%2Bb%3D&rscd=attachment%3B+filename%3Dlatest.txt";
        let cases = [
            (asset, Some(asset)),
            (
                "/OWNER/booth/releases/download/v0.2.0/latest.txt",
                Some("https://github.com/OWNER/booth/releases/download/v0.2.0/latest.txt"),
            ),
            (
                "https://github.com/latest.txt#part",
                Some("https://github.com/latest.txt"),
            ),
            ("http://github.com/latest.txt", None),
            ("https://github.com:8443/latest.txt", None),
            ("https://github.com:443/latest.txt", None),
            ("https://me@github.com/latest.txt", None),
            ("//evil.example/latest.txt", None),
            ("latest.txt", None),
            ("file:///C:/latest.txt", None),
            ("https://github.com/latest .txt", None),
            ("https://github.com/latest\u{7f}.txt", None),
            ("https://github.com/l\u{e4}test.txt", None),
            ("https://github.com", None),
            ("", None),
            ("#", None),
        ];
        for (location, expected) in cases {
            assert_eq!(
                redirect_target("github.com", location).as_deref(),
                expected,
                "{location:?}"
            );
        }
    }

    #[test]
    fn winhttp_errors_read_as_sentences() {
        let text = |code: u32| io_text("github.com", &io::Error::from_raw_os_error(code as i32));
        assert_eq!(
            text(ERROR_WINHTTP_TIMEOUT),
            "the connection to github.com timed out"
        );
        assert_eq!(
            text(ERROR_WINHTTP_NAME_NOT_RESOLVED),
            "github.com could not be found, which usually means this PC is offline"
        );
        assert_eq!(
            text(ERROR_WINHTTP_SECURE_INVALID_CA),
            "Windows did not accept the secure connection to github.com"
        );
        assert_eq!(
            text(12999),
            "WinHTTP error 12999 while fetching from github.com"
        );
        assert!(
            text(5).ends_with(" while fetching from github.com"),
            "{}",
            text(5)
        );
        let status = |code| {
            FetchError::Status {
                host: String::from("github.com"),
                code,
            }
            .to_string()
        };
        assert_eq!(status(404), "github.com answered 404, not found");
        assert_eq!(
            status(407),
            "the proxy asked Booth to sign in, which Booth does not do"
        );
        assert_eq!(status(503), "github.com answered 503");
        let host = || String::from("github.com");
        assert_eq!(
            FetchError::Redirect { host: host() }.to_string(),
            "github.com sent Booth on to an address it does not follow"
        );
        assert_eq!(
            FetchError::Redirects { host: host() }.to_string(),
            "github.com sent Booth on more than 5 times"
        );
    }
}
