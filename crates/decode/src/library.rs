// FFmpeg's DLLs, loaded once per process on first use and only from the
// folder of the running exe, by full path. booth.exe takes its own imports
// from System32 only (crates/app/build.rs); these two are the one exception,
// and their own imports of Windows DLLs come from System32 too. Nothing else
// is searched: not PATH, not the working directory, and not the exe's folder
// for anything but these two. A missing DLL is a sentence for the viewer,
// never a failed start.

use std::ffi::{CStr, c_char, c_int, c_uint};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use windows::Win32::Foundation::{FreeLibrary, HMODULE};
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};
use windows::core::{HSTRING, PCSTR};

use crate::error::DecodeError;
use crate::ffi::{
    AVBufferRef, AVCodec, AVCodecContext, AVDictionary, AVFrame, AVPacket, booth_avcodec_major,
    booth_avcodec_minor, booth_avutil_major, booth_avutil_minor, booth_log_quiet,
};

pub(crate) const AVCODEC: &str = "avcodec-62.dll";
pub(crate) const AVUTIL: &str = "avutil-60.dll";

// Signatures copied from FFmpeg's headers by hand; src/fields.c fails to
// compile if the headers say otherwise. C enums are passed as c_int.
pub(crate) struct Library {
    pub(crate) av_log_set_level: unsafe extern "C" fn(c_int),
    pub(crate) av_strerror: unsafe extern "C" fn(c_int, *mut c_char, usize) -> c_int,
    pub(crate) av_hwdevice_ctx_alloc: unsafe extern "C" fn(c_int) -> *mut AVBufferRef,
    pub(crate) av_hwdevice_ctx_init: unsafe extern "C" fn(*mut AVBufferRef) -> c_int,
    pub(crate) av_buffer_unref: unsafe extern "C" fn(*mut *mut AVBufferRef),
    pub(crate) av_frame_alloc: unsafe extern "C" fn() -> *mut AVFrame,
    pub(crate) av_frame_free: unsafe extern "C" fn(*mut *mut AVFrame),
    pub(crate) av_frame_unref: unsafe extern "C" fn(*mut AVFrame),
    pub(crate) av_frame_move_ref: unsafe extern "C" fn(*mut AVFrame, *mut AVFrame),
    pub(crate) avcodec_find_decoder: unsafe extern "C" fn(c_int) -> *const AVCodec,
    pub(crate) avcodec_profile_name: unsafe extern "C" fn(c_int, c_int) -> *const c_char,
    pub(crate) avcodec_alloc_context3: unsafe extern "C" fn(*const AVCodec) -> *mut AVCodecContext,
    pub(crate) avcodec_open2:
        unsafe extern "C" fn(*mut AVCodecContext, *const AVCodec, *mut *mut AVDictionary) -> c_int,
    pub(crate) avcodec_free_context: unsafe extern "C" fn(*mut *mut AVCodecContext),
    pub(crate) avcodec_send_packet:
        unsafe extern "C" fn(*mut AVCodecContext, *const AVPacket) -> c_int,
    pub(crate) avcodec_receive_frame:
        unsafe extern "C" fn(*mut AVCodecContext, *mut AVFrame) -> c_int,
    pub(crate) avcodec_flush_buffers: unsafe extern "C" fn(*mut AVCodecContext),
    pub(crate) av_packet_alloc: unsafe extern "C" fn() -> *mut AVPacket,
    pub(crate) av_packet_free: unsafe extern "C" fn(*mut *mut AVPacket),
}

static LIBRARY: OnceLock<Result<Library, DecodeError>> = OnceLock::new();

pub(crate) fn library() -> Result<&'static Library, DecodeError> {
    LIBRARY
        .get_or_init(|| load_from(&exe_folder()?))
        .as_ref()
        .map_err(Clone::clone)
}

fn exe_folder() -> Result<PathBuf, DecodeError> {
    let exe = std::env::current_exe().map_err(|err| DecodeError::NoExeFolder {
        detail: err.to_string(),
    })?;
    exe.parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| DecodeError::NoExeFolder {
            detail: format!("{} has no folder", exe.display()),
        })
}

pub(crate) fn load_from(folder: &Path) -> Result<Library, DecodeError> {
    for file in [AVCODEC, AVUTIL] {
        if !folder.join(file).is_file() {
            return Err(DecodeError::Missing {
                file,
                folder: folder.to_path_buf(),
            });
        }
    }
    // Each one looks for its imports in System32 only. avcodec imports
    // avutil by name, and a name that is loaded already binds to that
    // module, so avutil goes first.
    let avutil = Module::load(folder, AVUTIL)?;
    let avcodec = Module::load(folder, AVCODEC)?;

    // SAFETY: the types are the ones in FFmpeg's headers, which fields.c
    // checks; both take nothing and return a number.
    let (avutil_found, avcodec_found) = unsafe {
        let avutil_version: unsafe extern "C" fn() -> c_uint = avutil.symbol(c"avutil_version")?;
        let avcodec_version: unsafe extern "C" fn() -> c_uint =
            avcodec.symbol(c"avcodec_version")?;
        (avutil_version(), avcodec_version())
    };
    check_version(
        AVUTIL,
        avutil_found,
        (booth_avutil_major, booth_avutil_minor),
    )?;
    check_version(
        AVCODEC,
        avcodec_found,
        (booth_avcodec_major, booth_avcodec_minor),
    )?;

    // SAFETY: each field's type is the signature FFmpeg's header declares
    // for that name, which fields.c checks.
    let library = unsafe {
        Library {
            av_log_set_level: avutil.symbol(c"av_log_set_level")?,
            av_strerror: avutil.symbol(c"av_strerror")?,
            av_hwdevice_ctx_alloc: avutil.symbol(c"av_hwdevice_ctx_alloc")?,
            av_hwdevice_ctx_init: avutil.symbol(c"av_hwdevice_ctx_init")?,
            av_buffer_unref: avutil.symbol(c"av_buffer_unref")?,
            av_frame_alloc: avutil.symbol(c"av_frame_alloc")?,
            av_frame_free: avutil.symbol(c"av_frame_free")?,
            av_frame_unref: avutil.symbol(c"av_frame_unref")?,
            av_frame_move_ref: avutil.symbol(c"av_frame_move_ref")?,
            avcodec_find_decoder: avcodec.symbol(c"avcodec_find_decoder")?,
            avcodec_profile_name: avcodec.symbol(c"avcodec_profile_name")?,
            avcodec_alloc_context3: avcodec.symbol(c"avcodec_alloc_context3")?,
            avcodec_open2: avcodec.symbol(c"avcodec_open2")?,
            avcodec_free_context: avcodec.symbol(c"avcodec_free_context")?,
            avcodec_send_packet: avcodec.symbol(c"avcodec_send_packet")?,
            avcodec_receive_frame: avcodec.symbol(c"avcodec_receive_frame")?,
            avcodec_flush_buffers: avcodec.symbol(c"avcodec_flush_buffers")?,
            av_packet_alloc: avcodec.symbol(c"av_packet_alloc")?,
            av_packet_free: avcodec.symbol(c"av_packet_free")?,
        }
    };
    // FFmpeg logs to stderr, which booth.exe does not have; every failure
    // comes back as a return code anyway.
    // SAFETY: a setter for a global.
    unsafe { (library.av_log_set_level)(booth_log_quiet) };

    // The function pointers above are only good while the DLLs stay loaded,
    // which is until the process ends.
    std::mem::forget(avcodec);
    std::mem::forget(avutil);
    Ok(library)
}

// FFmpeg packs its version as major << 16 | minor << 8 | micro. Within a
// major version a DLL may be newer than the headers, never older: fields the
// headers know may be missing from an older one's structs.
pub(crate) fn check_version(
    file: &'static str,
    found: c_uint,
    (major, minor): (c_int, c_int),
) -> Result<(), DecodeError> {
    let found = (found >> 16, found >> 8 & 0xff);
    let needed = (major as u32, minor as u32);
    if found.0 != needed.0 || found.1 < needed.1 {
        return Err(DecodeError::WrongVersion {
            file,
            found,
            needed,
        });
    }
    Ok(())
}

// A loaded DLL, unloaded again if loading fails further on.
struct Module {
    handle: HMODULE,
    file: &'static str,
}

impl Module {
    fn load(folder: &Path, file: &'static str) -> Result<Module, DecodeError> {
        let path = HSTRING::from(folder.join(file).as_path());
        // Not LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR: with it, every Windows DLL
        // FFmpeg imports that is not loaded yet and not on the KnownDLLs
        // list (bcrypt.dll, and whatever another build of the same version
        // imports) would be looked for in this folder first, and booth.exe
        // usually sits in Downloads.
        // SAFETY: a full path in a live string; the flag needs no file handle.
        let handle = unsafe { LoadLibraryExW(&path, None, LOAD_LIBRARY_SEARCH_SYSTEM32) }.map_err(
            |source| DecodeError::Load {
                file,
                folder: folder.to_path_buf(),
                source,
            },
        )?;
        Ok(Module { handle, file })
    }

    // SAFETY: T must be the pointer type of the function as FFmpeg's header
    // declares it.
    unsafe fn symbol<T: Copy>(&self, name: &'static CStr) -> Result<T, DecodeError> {
        const { assert!(size_of::<T>() == size_of::<usize>()) };
        // SAFETY: a live module handle and a NUL-terminated name.
        let found = unsafe { GetProcAddress(self.handle, PCSTR(name.as_ptr().cast())) };
        let Some(found) = found else {
            return Err(DecodeError::MissingExport {
                file: self.file,
                name: name.to_str().unwrap_or("a function"),
            });
        };
        // SAFETY: T is a function pointer of the same size, and the caller
        // names the type the export has.
        Ok(unsafe { std::mem::transmute_copy::<_, T>(&found) })
    }
}

impl Drop for Module {
    fn drop(&mut self) {
        // SAFETY: loaded above and not used past here. A failure leaves the
        // DLL loaded, which harms nothing.
        unsafe {
            let _ = FreeLibrary(self.handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let folder =
            std::env::temp_dir().join(format!("booth-decode-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        std::fs::create_dir_all(&folder).unwrap();
        folder
    }

    #[test]
    fn file_names_follow_headers() {
        assert_eq!(AVCODEC, format!("avcodec-{booth_avcodec_major}.dll"));
        assert_eq!(AVUTIL, format!("avutil-{booth_avutil_major}.dll"));
    }

    #[test]
    fn missing_dll_named() {
        let folder = scratch("empty");
        let err = load_from(&folder)
            .err()
            .expect("an empty folder must not load");
        let text = err.to_string();
        println!("{text}");
        assert_eq!(
            text,
            format!(
                "the video decoder is missing: avcodec-62.dll was not found next to booth.exe in {}\\. Unzip Booth again with all its files",
                folder.display()
            )
        );

        std::fs::write(folder.join(AVCODEC), b"").unwrap();
        let text = load_from(&folder).err().expect("no avutil").to_string();
        println!("{text}");
        assert!(text.contains("avutil-60.dll was not found"), "{text}");
        std::fs::remove_dir_all(&folder).unwrap();
    }

    #[test]
    fn damaged_dll() {
        let folder = scratch("damaged");
        for file in [AVCODEC, AVUTIL] {
            std::fs::write(folder.join(file), b"not a dll").unwrap();
        }
        let text = load_from(&folder)
            .err()
            .expect("text is not a DLL")
            .to_string();
        println!("{text}");
        assert!(
            text.starts_with(&format!(
                "could not load avutil-60.dll from {}\\: ",
                folder.display()
            )),
            "{text}"
        );
        assert!(
            text.ends_with("Unzip Booth again with all its files"),
            "{text}"
        );
        // Nothing holds the files open, or this would fail.
        std::fs::remove_dir_all(&folder).unwrap();
    }

    #[test]
    fn version_check() {
        let needed = (62, 28);
        let pack = |major: u32, minor: u32| major << 16 | minor << 8 | 100;
        assert!(check_version(AVCODEC, pack(62, 28), needed).is_ok());
        assert!(check_version(AVCODEC, pack(62, 40), needed).is_ok());

        let older_major = check_version(AVCODEC, pack(61, 30), needed).unwrap_err();
        assert_eq!(
            older_major.to_string(),
            "avcodec-62.dll is version 61, Booth needs 62. Unzip Booth again with all its files"
        );
        let newer_major = check_version(AVCODEC, pack(63, 0), needed).unwrap_err();
        assert_eq!(
            newer_major.to_string(),
            "avcodec-62.dll is version 63, Booth needs 62. Unzip Booth again with all its files"
        );
        let older_minor = check_version(AVCODEC, pack(62, 3), needed).unwrap_err();
        assert_eq!(
            older_minor.to_string(),
            "avcodec-62.dll is version 62.3, Booth needs 62.28 or newer. Unzip Booth again with all its files"
        );
    }

    // The Windows DLLs avutil-60.dll and avcodec-62.dll import that are not
    // on the KnownDLLs list (dumpbin /dependents; the registry's KnownDLLs).
    // The rest of what they import is kernel32.dll and each other.
    const PLANTABLE: [&str; 1] = ["bcrypt.dll"];
    const PLANT_FOLDER: &str = "BOOTH_DECODE_PLANT_FOLDER";

    fn module_path(name: &str) -> Option<PathBuf> {
        use windows::Win32::System::LibraryLoader::{GetModuleFileNameW, GetModuleHandleW};
        // SAFETY: a name in a live string; the handle is not kept.
        let module = unsafe { GetModuleHandleW(&HSTRING::from(name)) }.ok()?;
        let mut path = [0u16; 1024];
        // SAFETY: a loaded module and a buffer with its length.
        let len = unsafe { GetModuleFileNameW(Some(module), &mut path) } as usize;
        Some(PathBuf::from(String::from_utf16_lossy(&path[..len])))
    }

    fn system32() -> PathBuf {
        use windows::Win32::System::SystemInformation::GetSystemDirectoryW;
        let mut path = [0u16; 1024];
        // SAFETY: a buffer with its length.
        let len = unsafe { GetSystemDirectoryW(Some(&mut path)) } as usize;
        assert!(len > 0 && len < path.len(), "no System32 folder");
        PathBuf::from(String::from_utf16_lossy(&path[..len]))
    }

    #[test]
    fn planted_windows_dlls() {
        // A DLL that is loaded already is never looked for again, and the
        // other tests here load FFmpeg, so this runs in a process of its own.
        let folder = scratch("planted");
        let exe = std::env::current_exe().unwrap();
        let output = std::process::Command::new(exe)
            .args([
                "library::tests::load_beside_planted_windows_dlls",
                "--exact",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(PLANT_FOLDER, &folder)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        println!("{stdout}{}", String::from_utf8_lossy(&output.stderr));
        // The child has exited, so nothing holds the files.
        std::fs::remove_dir_all(&folder).unwrap();
        assert!(output.status.success(), "{}", output.status);
        assert!(stdout.contains("1 passed"), "the check did not run");
    }

    #[test]
    #[ignore = "run by planted_windows_dlls in a process of its own"]
    fn load_beside_planted_windows_dlls() {
        let folder = PathBuf::from(std::env::var_os(PLANT_FOLDER).expect(PLANT_FOLDER));
        let beside_test = exe_folder().unwrap();
        for file in [AVUTIL, AVCODEC] {
            assert_eq!(module_path(file), None, "{file} is loaded already");
            let (from, to) = (beside_test.join(file), folder.join(file));
            if std::fs::hard_link(&from, &to).is_err() {
                std::fs::copy(&from, &to).unwrap();
            }
        }
        let system = system32();
        let planted: Vec<&str> = PLANTABLE
            .into_iter()
            .filter(|name| module_path(name).is_none())
            .collect();
        assert!(
            !planted.is_empty(),
            "every Windows DLL FFmpeg imports is loaded already, so none can be planted"
        );
        for name in &planted {
            std::fs::copy(system.join(name), folder.join(name)).unwrap();
        }
        println!("planted beside FFmpeg: {}", planted.join(", "));

        load_from(&folder).unwrap_or_else(|e| panic!("{e}"));
        let lower = |path: &Path| path.to_string_lossy().to_lowercase();
        for name in &planted {
            let path = module_path(name).unwrap_or_else(|| panic!("FFmpeg did not load {name}"));
            println!("{name} came from {}", path.display());
            assert_eq!(lower(&path), lower(&system.join(name)));
        }
        for file in [AVUTIL, AVCODEC] {
            let path = module_path(file).unwrap_or_else(|| panic!("{file} is not loaded"));
            assert_eq!(lower(&path), lower(&folder.join(file)));
        }
    }

    #[test]
    fn dlls_beside_the_test_load() {
        match library() {
            Ok(_) => println!(
                "loaded {AVCODEC} ({booth_avcodec_major}.{booth_avcodec_minor} or newer) and {AVUTIL} from beside the test"
            ),
            Err(err) => panic!("{err}"),
        }
    }
}
