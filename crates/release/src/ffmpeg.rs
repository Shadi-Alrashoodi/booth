// The FFmpeg DLLs ship under the LGPL, which asks for its text, the exact
// source, and (for version 3) the GPL's text next to them. Which LGPL is read
// from the DLLs themselves, so a different build cannot ship under the wrong
// text, and a GPL or nonfree build stops the release. So does a build with
// outside libraries compiled in (dav1d, libvpx, zlib and the like): each has
// its own license and notice, which this tool does not collect. The notices
// of the few FFmpeg files under a permissive license of their own come from
// NOTICES.txt, which tools\ffmpeg\build.sh writes.
//
// third_party\ffmpeg\SOURCE.txt, written by tools\build-ffmpeg.ps1, names
// the build and its source exactly, one per line:
//   Build: <the compiler and tools that built it>
//   SHA-256 <dll>: <hash of that DLL>, one line for each DLL
//   Source: <address of the FFmpeg source archive it was built from>
//   Source SHA-256: <hash of that archive>
//   Recipe: <the build recipe and its zip's name, then "in the release
//           <name> at <address>", the release of Booth's that holds the
//           recipe and the source archive>
// Here the lines are only checked for their form. The script checks the DLLs
// and the archive against its pins before the release gets here, and
// tools\release.ps1 holds the DLLs it puts in the zip to these SHA-256 lines.
// The source and the recipe go up once, to that release of their own, not
// with every release of Booth, so the notice gives its address.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use crate::licenses::{normalize, wrap, write_crlf};

const GPL_3: &str = include_str!("../licenses/GPL-3.0.txt");

// Options of FFmpeg's configure that compile code from another project into
// the DLLs, besides every --enable-lib*. The Windows parts (d3d11va, dxva2,
// mediafoundation, schannel) are not here: Windows brings them.
const OUTSIDE: &[&str] = &[
    "alsa",
    "amf",
    "avisynth",
    "bzlib",
    "chromaprint",
    "cuda",
    "cuda-llvm",
    "cuda-nvcc",
    "cuda-sdk",
    "cuvid",
    "decklink",
    "ffnvcodec",
    "fontconfig",
    "frei0r",
    "gcrypt",
    "gmp",
    "gnutls",
    "iconv",
    "jni",
    "ladspa",
    "lcms2",
    "lv2",
    "lzma",
    "mbedtls",
    "mediacodec",
    "mmal",
    "nvdec",
    "nvenc",
    "omx",
    "openal",
    "opencl",
    "opengl",
    "openssl",
    "pocketsphinx",
    // winpthreads, when FFmpeg is built with MinGW. w32threads is Windows'.
    "pthreads",
    "rkmpp",
    "sdl2",
    "sndio",
    "vaapi",
    "vapoursynth",
    "vdpau",
    "vulkan",
    "vulkan-static",
    "whisper",
    "xlib",
    "zlib",
];

const SOURCE_LINES: &[&str] = &["Build: ", "Source: ", "Source SHA-256: ", "Recipe: "];
const BUILD_FFMPEG: &str = "powershell -ExecutionPolicy Bypass -File tools\\build-ffmpeg.ps1";
const ARCHIVES: &[&str] = &[".tar.xz", ".tar.gz", ".tar.bz2", ".zip"];

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Lgpl {
    V2_1,
    V3,
}

pub struct Ffmpeg {
    pub dlls: Vec<String>,
    pub license: Lgpl,
    license_text: Vec<u8>,
    notices: String,
    source: String,
    source_release: SourceRelease,
    configure: String,
}

#[derive(Debug, PartialEq)]
struct SourceRelease {
    name: String,
    address: String,
}

impl Ffmpeg {
    pub fn read(dir: &Path) -> Result<Ffmpeg, String> {
        let bin = dir.join("bin");
        let mut dlls: Vec<String> = fs::read_dir(&bin)
            .map_err(|err| {
                format!(
                    "could not list {}: {err}; build FFmpeg first with {BUILD_FFMPEG}",
                    bin.display()
                )
            })?
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.to_lowercase().ends_with(".dll"))
            .collect();
        dlls.sort();
        if dlls.is_empty() {
            return Err(format!(
                "{} has no DLLs; build FFmpeg first with {BUILD_FFMPEG}",
                bin.display()
            ));
        }

        let mut license = None;
        let mut common_lines: Option<BTreeSet<String>> = None;
        for dll in &dlls {
            let path = bin.join(dll);
            let bytes = fs::read(&path)
                .map_err(|err| format!("could not read {}: {err}", path.display()))?;
            let library = library_name(dll);
            let said = license_string(&bytes, &library).ok_or_else(|| {
                format!(
                    "{} has no \"lib{library} license: \" string; is it an FFmpeg DLL?",
                    path.display()
                )
            })?;
            let this = match said.as_str() {
                "LGPL version 2.1 or later" => Lgpl::V2_1,
                "LGPL version 3 or later" => Lgpl::V3,
                _ => {
                    return Err(format!(
                        "{dll} says its license is \"{said}\". Booth ships only an LGPL build of FFmpeg; build one with {BUILD_FFMPEG}"
                    ));
                }
            };
            if license.is_some_and(|l| l != this) {
                return Err(format!(
                    "the FFmpeg DLLs in {} do not all say the same license",
                    bin.display()
                ));
            }
            license = Some(this);

            let lines = configure_lines(&bytes);
            common_lines = Some(match common_lines {
                None => lines,
                Some(before) => before.intersection(&lines).cloned().collect(),
            });
        }
        let license = license.expect("at least one DLL was read");

        // Every FFmpeg library carries the configure line for its
        // *_configuration() call. Libraries built into avcodec can carry
        // their own, so FFmpeg's is the one line all the DLLs share.
        let mut common_lines = common_lines.unwrap_or_default().into_iter();
        let configure = match (common_lines.next(), common_lines.next()) {
            (Some(line), None) => line,
            (None, _) => {
                return Err(format!(
                    "the DLLs in {} share no FFmpeg configure line, so what was built into them cannot be checked; are they all from one FFmpeg build?",
                    bin.display()
                ));
            }
            (Some(_), Some(_)) => {
                return Err(format!(
                    "the DLLs in {} share more than one configure line, so the tool cannot tell which is FFmpeg's",
                    bin.display()
                ));
            }
        };
        check_configure(&configure)?;

        let license_path = dir.join("LICENSE.txt");
        let license_text = fs::read(&license_path)
            .map_err(|err| format!("could not read {}: {err}", license_path.display()))?;
        let flat = normalize(&String::from_utf8_lossy(&license_text))
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let expected = match license {
            Lgpl::V2_1 => "Version 2.1, February 1999",
            Lgpl::V3 => "Version 3, 29 June 2007",
        };
        if !(flat.contains("GNU LESSER GENERAL PUBLIC LICENSE") && flat.contains(expected)) {
            return Err(format!(
                "{} is not the LGPL {} text that the DLLs say they are under",
                license_path.display(),
                license.name()
            ));
        }

        let notices_path = dir.join("NOTICES.txt");
        let notices = fs::read(&notices_path).map_err(|err| {
            format!(
                "could not read {}: {err}. It holds the notices of the FFmpeg files under licenses of their own; build FFmpeg again with {BUILD_FFMPEG}",
                notices_path.display()
            )
        })?;
        let notices = normalize(&String::from_utf8_lossy(&notices));

        let source_path = dir.join("SOURCE.txt");
        let source = fs::read(&source_path)
            .map_err(|err| format!("could not read {}: {err}", source_path.display()))?;
        let source = normalize(&String::from_utf8_lossy(&source));
        let source_release = check_source(&source, &source_path, &dlls)?;
        Ok(Ffmpeg {
            dlls,
            license,
            license_text,
            notices,
            source,
            source_release,
            configure,
        })
    }

    // The ffmpeg folder of the release: the license as the build shipped
    // it, byte for byte, the GPL when the LGPL is version 3, the notices of
    // the files under other licenses, and SOURCE.txt.
    pub fn write_folder(&self, out: &Path) -> Result<(), String> {
        let dir = out.join("ffmpeg");
        fs::create_dir_all(&dir)
            .map_err(|err| format!("could not create {}: {err}", dir.display()))?;
        let license = dir.join("LICENSE.txt");
        fs::write(&license, &self.license_text)
            .map_err(|err| format!("could not write {}: {err}", license.display()))?;
        if self.license == Lgpl::V3 {
            let gpl = dir.join("GPL-3.0.txt");
            fs::write(&gpl, GPL_3)
                .map_err(|err| format!("could not write {}: {err}", gpl.display()))?;
        }
        write_crlf(&dir.join("NOTICES.txt"), &format!("{}\n", self.notices))?;
        write_crlf(&dir.join("SOURCE.txt"), &self.about())
    }

    fn about(&self) -> String {
        let mut out = wrap(
            &format!(
                "{} beside booth.exe are FFmpeg, built from its unmodified source as below. Booth loads them from its own folder when a viewer starts decoding; to use your own build of the same FFmpeg version, replace them with DLLs of the same names.",
                self.dlls.join(", ")
            ),
            "",
        );
        out.push('\n');
        // The address on a line of its own, so nothing next to it reads as
        // part of it.
        out.push_str(&wrap(
            &format!(
                "The exact source archive they were built from (Source) and the recipe that built them (Recipe) are in Booth's release {}:",
                self.source_release.name
            ),
            "",
        ));
        out.push_str(&self.source_release.address);
        out.push('\n');
        out.push_str(&wrap(
            "The source archive is also on ffmpeg.org at the Source address, and its SHA-256 tells whether a copy is the same file.",
            "",
        ));
        out.push('\n');
        out.push_str(&self.source);
        out.push('\n');
        // One line, so it can be pasted back into configure as it is.
        out.push_str("Configured with: ");
        out.push_str(&self.configure);
        out.push_str("\n\n");
        let texts = match self.license {
            Lgpl::V2_1 => "Its text is in ffmpeg\\LICENSE.txt.".to_string(),
            Lgpl::V3 => "Its text is in ffmpeg\\LICENSE.txt, and the GNU General Public License version 3 it is written on top of is in ffmpeg\\GPL-3.0.txt.".to_string(),
        };
        out.push_str(&wrap(
            &format!(
                "The DLLs say they are under the GNU Lesser General Public License {} or later. {texts} A few files compiled into them are under a permissive license of their own; their notices are in ffmpeg\\NOTICES.txt.",
                self.license.name()
            ),
            "",
        ));
        out
    }

    pub fn notice(&self) -> String {
        let mut out = self.about();
        out.push('\n');
        out.push_str(&normalize(&String::from_utf8_lossy(&self.license_text)));
        out.push_str("\n\n");
        out.push_str(&self.notices);
        out.push('\n');
        out
    }
}

impl Lgpl {
    pub fn name(self) -> &'static str {
        match self {
            Lgpl::V2_1 => "version 2.1",
            Lgpl::V3 => "version 3",
        }
    }
}

// avcodec-62.dll is libavcodec.
fn library_name(dll: &str) -> String {
    let lower = dll.to_lowercase();
    let stem = lower.strip_suffix(".dll").unwrap_or(&lower);
    stem.split('-').next().unwrap_or(stem).to_string()
}

// FFmpeg's libraries carry "lib<name> license: <license>" as a string for
// their *_license() calls, for example "libavutil license: LGPL version 3 or
// later". configure writes it from --enable-gpl, --enable-version3 and
// --enable-nonfree. Only the DLL's own library counts: avcodec also holds
// strings of the libraries built into it.
fn license_string(bytes: &[u8], library: &str) -> Option<String> {
    let marker = format!("lib{library} license: ");
    let marker = marker.as_bytes();
    let mut from = 0;
    while let Some(found) = find(&bytes[from..], marker) {
        let at = from + found;
        let value_start = at + marker.len();
        if at == 0 || bytes[at - 1] == 0 {
            let end = bytes[value_start..]
                .iter()
                .position(|&b| b == 0)
                .map_or(bytes.len(), |i| value_start + i);
            return String::from_utf8(bytes[value_start..end].to_vec()).ok();
        }
        from = value_start;
    }
    None
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

// Every string in the DLL that reads like a configure line: printable, and
// starting with an option.
fn configure_lines(bytes: &[u8]) -> BTreeSet<String> {
    bytes
        .split(|&b| b == 0)
        .filter(|s| {
            s.len() > 2 && s.starts_with(b"--") && s.iter().all(|&b| (0x20..0x7F).contains(&b))
        })
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect()
}

// Later options win, as they do in configure.
fn check_configure(line: &str) -> Result<(), String> {
    let mut autodetect = true;
    let mut outside = BTreeSet::new();
    for option in options(line) {
        if option == "--disable-autodetect" {
            autodetect = false;
        } else if option == "--enable-autodetect" {
            autodetect = true;
        } else if let Some(name) = option.strip_prefix("--enable-") {
            let name = name.replace('_', "-");
            if !name.contains('=') && (name.starts_with("lib") || OUTSIDE.contains(&name.as_str()))
            {
                outside.insert(name);
            }
        } else if let Some(name) = option.strip_prefix("--disable-") {
            outside.remove(&name.replace('_', "-"));
        } else if let Some(libraries) = option.strip_prefix("--extra-libs=") {
            for word in libraries.split_whitespace() {
                if let Some(library) = word.strip_prefix("-l") {
                    outside.insert(format!("{library} (--extra-libs)"));
                }
            }
        }
    }
    if !outside.is_empty() {
        return Err(format!(
            "the FFmpeg DLLs have outside libraries built in, whose licenses and notices this tool does not collect: {}. Booth ships only a build of FFmpeg without them; build one with {BUILD_FFMPEG}",
            outside.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }
    if autodetect {
        return Err(format!(
            "the FFmpeg DLLs were configured without --disable-autodetect, so configure may have built in outside libraries it found on the build machine (zlib, iconv, SDL2 and others) that its options do not name. Booth ships only a build of FFmpeg configured with --disable-autodetect; build one with {BUILD_FFMPEG}"
        ));
    }
    Ok(())
}

// configure writes each value that needs it in single quotes, with a quote
// inside written as '\''.
fn options(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut quoted = false;
    let mut escaped = false;
    for c in line.chars() {
        if escaped {
            word.push(c);
            escaped = false;
        } else if quoted {
            if c == '\'' {
                quoted = false;
            } else {
                word.push(c);
            }
        } else if c == '\'' {
            quoted = true;
        } else if c == '\\' {
            escaped = true;
        } else if c == ' ' {
            if !word.is_empty() {
                out.push(std::mem::take(&mut word));
            }
        } else {
            word.push(c);
        }
    }
    if !word.is_empty() {
        out.push(word);
    }
    out
}

// The LGPL asks for the exact source of what ships, for as long as it ships;
// a download page or a build tag that is later deleted is not that.
fn check_source(source: &str, path: &Path, dlls: &[String]) -> Result<SourceRelease, String> {
    let value = |prefix: &str| {
        source
            .lines()
            .find_map(|l| l.strip_prefix(prefix))
            .map(str::trim)
            .filter(|v| !v.is_empty())
    };
    for needed in SOURCE_LINES {
        if value(needed).is_none() {
            return Err(format!(
                "{} has no \"{needed}\" line. It must name the build (Build, and SHA-256 for each DLL), the exact source archive it was built from (Source, Source SHA-256) and the build recipe (Recipe); build FFmpeg again with {BUILD_FFMPEG}, which writes them all",
                path.display()
            ));
        }
    }
    let archive = value("Source: ").unwrap_or_default();
    if !(archive.starts_with("https://")
        && ARCHIVES
            .iter()
            .any(|ext| archive.to_lowercase().ends_with(ext)))
    {
        return Err(format!(
            "{} gives the source as \"{archive}\", which is not the address of a source archive; name the exact .tar.xz, .tar.gz, .tar.bz2 or .zip the DLLs were built from",
            path.display()
        ));
    }
    let hashes = std::iter::once("Source SHA-256: ".to_string())
        .chain(dlls.iter().map(|dll| format!("SHA-256 {dll}: ")));
    for hash in hashes {
        let Some(got) = value(&hash) else {
            return Err(format!(
                "{} has no \"{hash}\" line; it must give the SHA-256 of every DLL in the build",
                path.display()
            ));
        };
        if !(got.len() == 64 && got.bytes().all(|b| b.is_ascii_hexdigit())) {
            return Err(format!(
                "{} has \"{hash}{got}\", which is not a SHA-256 hash",
                path.display()
            ));
        }
    }
    let recipe = value("Recipe: ").unwrap_or_default();
    source_release(recipe).ok_or_else(|| {
        format!(
            "{} has \"Recipe: {recipe}\", which does not say which release holds the recipe and the source archive (\"in the release <name> at <https address ending in /tag/<name>>\"); build FFmpeg again with {BUILD_FFMPEG}, which writes it",
            path.display()
        )
    })
}

// "... in the release ffmpeg-8.1.3 at https://github.com/.../tag/ffmpeg-8.1.3
// with ...". The address has to end in the name, so the two cannot disagree.
fn source_release(recipe: &str) -> Option<SourceRelease> {
    let (_, rest) = recipe.split_once(" in the release ")?;
    let (name, rest) = rest.split_once(" at ")?;
    let address = rest.split_whitespace().next()?;
    let name_ok = !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-._".contains(&b));
    let address_ok = address.starts_with("https://")
        && address.ends_with(&format!("/tag/{name}"))
        && address.bytes().all(|b| b.is_ascii_graphic());
    (name_ok && address_ok).then(|| SourceRelease {
        name: name.to_string(),
        address: address.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Trimmed from the configure line of BtbN's LGPL autobuild of 8.1.3.
    const AUTOBUILD: &str = "--prefix=/ffbuild/prefix --pkg-config-flags=--static --arch=x86_64 --target-os=mingw32 --enable-version3 --disable-debug --enable-shared --disable-static --disable-w32threads --enable-pthreads --enable-iconv --enable-zlib --enable-libxml2 --enable-fontconfig --enable-libdav1d --disable-libxcb --enable-gmp --enable-lzma --enable-libopencore-amrnb --disable-libx264 --enable-libzvbi --extra-cflags=-DLIBTWOLAME_STATIC --extra-libs=-lgomp --extra-ldflags=-pthread --extra-version=20260926";
    // tools\ffmpeg\build.sh's, as the DLLs carry it.
    const SLIM: &str = "--toolchain=msvc --arch=x86_64 --prefix=/ffmpeg --enable-shared --disable-static --disable-autodetect --disable-everything --disable-programs --disable-doc --disable-network --disable-avdevice --disable-avformat --disable-avfilter --disable-swscale --disable-swresample --disable-faan --disable-debug --enable-w32threads --enable-d3d11va --enable-decoder='h264,hevc' --enable-hwaccel='h264_d3d11va,h264_d3d11va2,hevc_d3d11va,hevc_d3d11va2' --extra-cflags=-MT --extra-cflags=-Brepro --extra-ldflags=-Brepro";

    #[test]
    fn license_string_of_own_library() {
        let dll = b"MZ\0junk license: not this one\0libzvbi license: GPL\0libavutil license: LGPL version 3 or later\0more";
        assert_eq!(
            license_string(dll, "avutil").as_deref(),
            Some("LGPL version 3 or later")
        );
        let inside = b"\0xlibavcodec license: GPL\0libavcodec license: LGPL version 2.1 or later\0";
        assert_eq!(
            license_string(inside, "avcodec").as_deref(),
            Some("LGPL version 2.1 or later")
        );
        assert_eq!(license_string(dll, "avcodec"), None);
        assert_eq!(library_name("avcodec-62.dll"), "avcodec");
        assert_eq!(library_name("AVUTIL-60.DLL"), "avutil");
    }

    #[test]
    fn refuses_outside_libraries_and_autodetect() {
        let err = check_configure(AUTOBUILD).unwrap_err();
        for name in [
            "libdav1d",
            "libopencore-amrnb",
            "libzvbi",
            "gmp",
            "lzma",
            "fontconfig",
            "iconv",
            "zlib",
            "pthreads",
            "gomp (--extra-libs)",
        ] {
            assert!(err.contains(name), "{name}: {err}");
        }
        assert!(!err.contains("libxcb") && !err.contains("libx264"), "{err}");

        check_configure(SLIM).unwrap();
        let err = check_configure(&SLIM.replace(" --disable-autodetect", "")).unwrap_err();
        assert!(err.contains("--disable-autodetect"), "{err}");
        let err = check_configure(&format!("{SLIM} --enable-autodetect")).unwrap_err();
        assert!(err.contains("--disable-autodetect"), "{err}");
        let err = check_configure(&format!("{SLIM} --enable-libvpx")).unwrap_err();
        assert!(err.contains("libvpx"), "{err}");
        check_configure(&format!("{SLIM} --enable-libvpx --disable-libvpx")).unwrap();
        let err = check_configure(&format!("{SLIM} --enable-nvdec --enable-pthreads")).unwrap_err();
        assert!(err.contains("nvdec") && err.contains("pthreads"), "{err}");
    }

    #[test]
    fn options_as_configure_quotes() {
        assert_eq!(
            options("--a=b  --extra-cflags='-O2 -g' --c='it'\\''s'"),
            ["--a=b", "--extra-cflags=-O2 -g", "--c=it's"]
        );
        assert!(options(SLIM).contains(&"--enable-decoder=h264,hevc".to_string()));
    }

    fn dll(library: &str, license_line: &str, configure: &str, more: &str) -> Vec<u8> {
        format!("MZ\0lib{library} license: {license_line}\0{configure}\0{more}\0").into_bytes()
    }

    const SOURCE: &str = "\u{FEFF}FFmpeg 8.1.3, built for Booth\r\nBuild: MSVC 14.44.35207\r\nSHA-256 avcodec-62.dll: 923B45CEB3F460E658AC9379037D340468F2C8AFBD41DCD524C438987B4BC114\r\nSHA-256 avutil-60.dll: 0961B0202A2DFD30F73C12FAD0C9B7408E2F7E3822DB6D84D9C806446F24B984\r\nSource: https://ffmpeg.org/releases/ffmpeg-8.1.3.tar.xz\r\nSource SHA-256: 7138D28C96D9D3E3AF4EE3D8CAD72741F8FFB40DA90C1112235DEA3ECD3178A3\r\nRecipe: tools\\build-ffmpeg.ps1 and tools\\ffmpeg\\build.sh as ffmpeg-8.1.3-recipe.zip, in the release ffmpeg-8.1.3 at https://github.com/OWNER/booth/releases/tag/ffmpeg-8.1.3 with the exact source archive\r\n";
    const RELEASE_AT: &str =
        " in the release ffmpeg-8.1.3 at https://github.com/OWNER/booth/releases/tag/ffmpeg-8.1.3 ";
    const NOTICES: &str = "Files under a license of their own:\n\nlibavutil/adler32.c\n\n/*\n * This software is provided 'as-is'\n */\n";
    const LGPL21_HEAD: &str = "                  GNU LESSER GENERAL PUBLIC LICENSE\n                       Version 2.1, February 1999\n";
    const LGPL3_HEAD: &str = "                   GNU LESSER GENERAL PUBLIC LICENSE\n                       Version 3, 29 June 2007\n";

    struct Folder(std::path::PathBuf);

    impl Folder {
        fn new(name: &str, avutil: Vec<u8>, avcodec: Vec<u8>, license_file: &str) -> Folder {
            let dir = std::env::temp_dir().join(format!(
                "booth-release-ffmpeg-{name}-{}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(dir.join("bin")).unwrap();
            fs::write(dir.join("bin").join("avutil-60.dll"), avutil).unwrap();
            fs::write(dir.join("bin").join("avcodec-62.dll"), avcodec).unwrap();
            fs::write(dir.join("LICENSE.txt"), license_file).unwrap();
            fs::write(dir.join("NOTICES.txt"), NOTICES).unwrap();
            fs::write(dir.join("SOURCE.txt"), SOURCE).unwrap();
            Folder(dir)
        }

        fn with(name: &str, license: &str, text: &str, configure: &str) -> Folder {
            Folder::new(
                name,
                dll("avutil", license, configure, ""),
                // avcodec also carries the line of a library built into it.
                dll(
                    "avcodec",
                    license,
                    configure,
                    "--disable-shared --enable-static --target=x86_64-win64-gcc",
                ),
                text,
            )
        }

        fn lgpl21(name: &str, configure: &str) -> Folder {
            Folder::with(name, "LGPL version 2.1 or later", LGPL21_HEAD, configure)
        }
    }

    impl Drop for Folder {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn slim_build() {
        let folder = Folder::lgpl21("slim", SLIM);
        let ffmpeg = Ffmpeg::read(&folder.0).unwrap();
        assert_eq!(ffmpeg.license, Lgpl::V2_1);
        assert_eq!(ffmpeg.dlls, ["avcodec-62.dll", "avutil-60.dll"]);
        assert_eq!(ffmpeg.configure, SLIM);
        let out = folder.0.join("out");
        ffmpeg.write_folder(&out).unwrap();
        let dir = out.join("ffmpeg");
        assert_eq!(
            fs::read(dir.join("LICENSE.txt")).unwrap(),
            LGPL21_HEAD.as_bytes()
        );
        assert!(!dir.join("GPL-3.0.txt").exists());
        assert_eq!(
            fs::read_to_string(dir.join("NOTICES.txt")).unwrap(),
            NOTICES.replace('\n', "\r\n")
        );

        let source = fs::read_to_string(dir.join("SOURCE.txt")).unwrap();
        println!("{source}");
        let flat = source.split_whitespace().collect::<Vec<_>>().join(" ");
        for part in [
            "avcodec-62.dll, avutil-60.dll beside booth.exe are FFmpeg, built from its unmodified source",
            "The exact source archive they were built from (Source) and the recipe that built them (Recipe) are in Booth's release ffmpeg-8.1.3:",
            "The source archive is also on ffmpeg.org at the Source address",
            "SHA-256 avutil-60.dll: 0961B0202A2DFD30F73C12FAD0C9B7408E2F7E3822DB6D84D9C806446F24B984",
            "Source: https://ffmpeg.org/releases/ffmpeg-8.1.3.tar.xz",
            "Recipe: tools\\build-ffmpeg.ps1 and tools\\ffmpeg\\build.sh as ffmpeg-8.1.3-recipe.zip, in the release ffmpeg-8.1.3",
            "version 2.1 or later. Its text is in ffmpeg\\LICENSE.txt.",
            "their notices are in ffmpeg\\NOTICES.txt",
        ] {
            assert!(flat.contains(part), "{part}");
        }
        assert!(source.contains(
            ":\r\nhttps://github.com/OWNER/booth/releases/tag/ffmpeg-8.1.3\r\nThe source archive"
        ));
        assert!(source.contains(&format!("Configured with: {SLIM}\r\n")));
        assert!(!source.contains('\u{FEFF}') && !source.contains("GPL-3.0"));
        assert!(!flat.contains("attached"), "{source}");

        let notice = ffmpeg.notice();
        assert!(notice.contains("Version 2.1, February 1999"));
        assert!(notice.contains("libavutil/adler32.c\n\n/*\n * This software is provided 'as-is'"));
    }

    #[test]
    fn lgpl3_build_ships_gpl() {
        let folder = Folder::with("v3", "LGPL version 3 or later", LGPL3_HEAD, SLIM);
        let ffmpeg = Ffmpeg::read(&folder.0).unwrap();
        assert_eq!(ffmpeg.license, Lgpl::V3);
        let out = folder.0.join("out");
        ffmpeg.write_folder(&out).unwrap();
        assert!(
            fs::read_to_string(out.join("ffmpeg").join("GPL-3.0.txt"))
                .unwrap()
                .contains("GNU GENERAL PUBLIC LICENSE")
        );
        let source = fs::read_to_string(out.join("ffmpeg").join("SOURCE.txt")).unwrap();
        assert!(source.contains("version 3 or later") && source.contains("GPL-3.0.txt"));
        assert!(ffmpeg.notice().contains("Version 3, 29 June 2007"));
    }

    #[test]
    fn refuses_wrong_builds() {
        let folder = Folder::with("gpl", "GPL version 2 or later", LGPL21_HEAD, SLIM);
        let err = Ffmpeg::read(&folder.0).err().unwrap();
        assert!(err.contains("only an LGPL build"), "{err}");

        let folder = Folder::with("mismatch", "LGPL version 2.1 or later", LGPL3_HEAD, SLIM);
        let err = Ffmpeg::read(&folder.0).err().unwrap();
        assert!(err.contains("is not the LGPL version 2.1 text"), "{err}");

        let folder = Folder::new(
            "mixed",
            dll("avutil", "LGPL version 2.1 or later", SLIM, ""),
            dll("avcodec", "LGPL version 3 or later", SLIM, ""),
            LGPL21_HEAD,
        );
        let err = Ffmpeg::read(&folder.0).err().unwrap();
        assert!(err.contains("do not all say the same license"), "{err}");

        let err = Ffmpeg::read(&Folder::lgpl21("outside", AUTOBUILD).0)
            .err()
            .unwrap();
        assert!(err.contains("outside libraries built in"), "{err}");
    }

    #[test]
    fn one_shared_configure_line() {
        let folder = Folder::new(
            "no-line",
            dll("avutil", "LGPL version 2.1 or later", SLIM, ""),
            dll("avcodec", "LGPL version 2.1 or later", AUTOBUILD, ""),
            LGPL21_HEAD,
        );
        let err = Ffmpeg::read(&folder.0).err().unwrap();
        assert!(err.contains("share no FFmpeg configure line"), "{err}");

        let folder = Folder::new(
            "two-lines",
            dll("avutil", "LGPL version 2.1 or later", SLIM, "--x"),
            dll("avcodec", "LGPL version 2.1 or later", SLIM, "--x"),
            LGPL21_HEAD,
        );
        let err = Ffmpeg::read(&folder.0).err().unwrap();
        assert!(err.contains("more than one configure line"), "{err}");
    }

    #[test]
    fn source_txt() {
        let folder = Folder::lgpl21("source", SLIM);
        let path = folder.0.join("SOURCE.txt");
        let read = |text: String| {
            fs::write(&path, text).unwrap();
            Ffmpeg::read(&folder.0).err().unwrap_or_default()
        };

        let pointer = SOURCE.replace(
            "https://ffmpeg.org/releases/ffmpeg-8.1.3.tar.xz",
            "https://ffmpeg.org/download.html",
        );
        assert!(read(pointer).contains("not the address of a source archive"));
        let err = read(SOURCE.replace("Recipe: ", "Recipe "));
        assert!(err.contains("no \"Recipe: \" line"), "{err}");
        let err = read(SOURCE.replace("Source SHA-256: 7138", "Source SHA-256: 71"));
        assert!(err.contains("not a SHA-256 hash"), "{err}");
        let err = read(SOURCE.replace("avcodec-62.dll: 923B", "avcodec-62.dll: 92"));
        assert!(err.contains("\"SHA-256 avcodec-62.dll: 92"), "{err}");
        let err = read(SOURCE.replace("SHA-256 avutil-60.dll", "SHA-256 swresample-6.dll"));
        assert!(err.contains("no \"SHA-256 avutil-60.dll: \" line"), "{err}");
        assert_eq!(read(SOURCE.to_string()), "");

        // The wording from when the source went up with every release, and
        // release addresses that are not the named release's.
        for recipe in [
            " attached to each release ",
            " in the release ffmpeg-8.1.3 at http://github.com/OWNER/booth/releases/tag/ffmpeg-8.1.3 ",
            " in the release ffmpeg-8.1.3 at https://github.com/OWNER/booth/releases/tag/ffmpeg-8.1.2 ",
            " in the release ffmpeg-8.1.3 at https://github.com/OWNER/booth/releases ",
            " in the release ../x at https://github.com/OWNER/booth/releases/tag/../x ",
            " in the release  at https://github.com/OWNER/booth/releases/tag/ ",
        ] {
            let err = read(SOURCE.replace(RELEASE_AT, recipe));
            assert!(
                err.contains("does not say which release holds the recipe"),
                "{recipe}: {err}"
            );
        }

        fs::remove_file(folder.0.join("NOTICES.txt")).unwrap();
        let err = Ffmpeg::read(&folder.0).err().unwrap();
        assert!(
            err.contains("NOTICES.txt") && err.contains("build-ffmpeg.ps1"),
            "{err}"
        );
    }
}
