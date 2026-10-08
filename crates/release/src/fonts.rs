// The fonts booth.exe embeds. IBM Plex comes from assets\fonts with the OFL
// next to it. The Phosphor icons come from inside the egui-phosphor crate,
// whose subset feature keeps only the tables that draw the icons and drops
// the name table, where the font keeps its copyright notice and license. So
// the notice is read here from the full font files the crate ships.

use std::fs;
use std::path::Path;

use crate::licenses::{Package, RULE, decode, normalize, wrap};

// The MIT text with the copyright line of its SPDX template, which the
// font's own notice takes the place of.
const MIT: &str = include_str!("../licenses/MIT.txt");
const MIT_COPYRIGHT: &str = "Copyright (c) <year> <copyright holders>";

// Name IDs in a font's name table.
const COPYRIGHT: u16 = 0;
const LICENSE: u16 = 13;
const LICENSE_URL: u16 = 14;

struct FromCrate {
    krate: &'static str,
    // Every font file in it must give the same notice: which of them goes
    // into booth.exe is up to the icons crates\app asks for.
    folder: &'static str,
    what: &'static str,
}

const FROM_CRATES: &[FromCrate] = &[FromCrate {
    krate: "egui-phosphor",
    folder: "res",
    what: "the Phosphor icon font, cut down to the icons Booth draws",
}];

pub struct Fonts {
    pub files: Vec<String>,
    pub license: String,
    pub from_crates: Vec<CrateFont>,
}

pub struct CrateFont {
    // "egui-phosphor 0.14.0"
    pub from: String,
    pub what: String,
    pub copyright: String,
    pub license_url: Option<String>,
}

pub fn read(dir: &Path, packages: &[Package]) -> Result<Fonts, String> {
    let files = font_files(dir)?;
    let ofl = dir.join("OFL.txt");
    let bytes = fs::read(&ofl).map_err(|err| format!("could not read {}: {err}", ofl.display()))?;
    let from_crates = FROM_CRATES
        .iter()
        .map(|font| read_crate_font(font, packages))
        .collect::<Result<_, _>>()?;
    Ok(Fonts {
        files,
        license: normalize(&decode(&bytes)),
        from_crates,
    })
}

fn font_files(dir: &Path) -> Result<Vec<String>, String> {
    let mut files: Vec<String> = fs::read_dir(dir)
        .map_err(|err| format!("could not list {}: {err}", dir.display()))?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| {
            let lower = n.to_lowercase();
            lower.ends_with(".ttf") || lower.ends_with(".otf")
        })
        .collect();
    files.sort();
    if files.is_empty() {
        return Err(format!("{} has no fonts", dir.display()));
    }
    Ok(files)
}

fn read_crate_font(font: &FromCrate, packages: &[Package]) -> Result<CrateFont, String> {
    let package = packages
        .iter()
        .find(|p| p.name == font.krate)
        .ok_or_else(|| {
            format!(
                "{} is no longer compiled into booth.exe; take it out of the list of fonts from crates in crates\\release\\src\\fonts.rs",
                font.krate
            )
        })?;
    let dir = package.dir.join(font.folder);
    let mut notices = Vec::new();
    for file in font_files(&dir)? {
        let path = dir.join(&file);
        let bytes =
            fs::read(&path).map_err(|err| format!("could not read {}: {err}", path.display()))?;
        let copyright = font_name(&bytes, COPYRIGHT).ok_or_else(|| {
            format!(
                "{} has no copyright notice in its name table; read the font's license and add an answer to the release tool",
                path.display()
            )
        })?;
        let license = font_name(&bytes, LICENSE).unwrap_or_default();
        if license != "MIT" {
            return Err(format!(
                "{} gives its license as \"{license}\", and the release tool writes out only MIT for a font from a crate; read the font's license and add an answer to the release tool",
                path.display()
            ));
        }
        notices.push((file, copyright, font_name(&bytes, LICENSE_URL)));
    }
    let mut notices = notices.into_iter();
    let (first, copyright, license_url) = notices
        .next()
        .ok_or_else(|| format!("{} has no fonts", dir.display()))?;
    if let Some((other, ..)) = notices.find(|(_, c, u)| (c, u) != (&copyright, &license_url)) {
        return Err(format!(
            "{first} and {other} in {} give different copyright notices or licenses; read them and add an answer to the release tool",
            dir.display()
        ));
    }
    Ok(CrateFont {
        from: format!("{} {}", package.name, package.version),
        what: font.what.to_string(),
        copyright,
        license_url,
    })
}

impl CrateFont {
    fn license_text(&self) -> String {
        normalize(&MIT.replacen(MIT_COPYRIGHT, &self.copyright, 1))
    }
}

impl Fonts {
    pub fn notice(&self) -> String {
        let mut out = wrap(
            &format!(
                "booth.exe embeds these font files: {}. They are under the SIL Open Font License 1.1:",
                self.files.join(", ")
            ),
            "",
        );
        out.push('\n');
        out.push_str(&self.license);
        out.push_str("\n\n");
        for font in &self.from_crates {
            let pointing = match &font.license_url {
                Some(url) => format!(", pointing to {url} for its text"),
                None => String::new(),
            };
            out.push_str(RULE);
            out.push_str("\n\n");
            out.push_str(&wrap(
                &format!(
                    "booth.exe also embeds {}, from {}. The cut-down font leaves out the part that holds its copyright notice and license. The full font in that crate gives the notice as \"{}\" and the license as MIT{pointing}. The MIT license follows with that notice in it:",
                    font.what, font.from, font.copyright
                ),
                "",
            ));
            out.push('\n');
            out.push_str(&font.license_text());
            out.push_str("\n\n");
        }
        out
    }
}

fn be16(data: &[u8], at: usize) -> Option<u16> {
    let bytes = data.get(at..at.checked_add(2)?)?;
    Some(u16::from_be_bytes([bytes[0], bytes[1]]))
}

fn be32(data: &[u8], at: usize) -> Option<u32> {
    let bytes = data.get(at..at.checked_add(4)?)?;
    Some(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

// One entry of a TrueType or OpenType font's name table, the Windows English
// one when there is one, as Windows shows it. A font cut short or with its
// offsets out of range gives None rather than a guess.
fn font_name(font: &[u8], id: u16) -> Option<String> {
    let tables = usize::from(be16(font, 4)?);
    let record = (0..tables)
        .map(|i| 12 + 16 * i)
        .find(|&r| font.get(r..r + 4) == Some(b"name".as_slice()))?;
    let start = usize::try_from(be32(font, record + 8)?).ok()?;
    let length = usize::try_from(be32(font, record + 12)?).ok()?;
    let table = font.get(start..start.checked_add(length)?)?;

    let count = usize::from(be16(table, 2)?);
    let storage = usize::from(be16(table, 4)?);
    let mut best: Option<(u8, String)> = None;
    for i in 0..count {
        let field = |n: usize| be16(table, 6 + 12 * i + 2 * n);
        let (platform, encoding, language) = (field(0)?, field(1)?, field(2)?);
        if field(3)? != id {
            continue;
        }
        let rank = match (platform, encoding, language) {
            (3, 1 | 10, 0x0409) => 0,
            (3, 1 | 10, _) => 1,
            (0, _, _) => 2,
            (1, 0, 0) => 3,
            _ => continue,
        };
        if best.as_ref().is_some_and(|(b, _)| *b <= rank) {
            continue;
        }
        let at = storage + usize::from(field(5)?);
        let Some(bytes) = table.get(at..at + usize::from(field(4)?)) else {
            continue;
        };
        // Mac Roman, the Mac entries' encoding, matches ASCII and nothing
        // past it.
        let text = if platform == 1 {
            bytes
                .is_ascii()
                .then(|| String::from_utf8_lossy(bytes).into_owned())
        } else {
            let (pairs, odd) = bytes.as_chunks::<2>();
            let units: Vec<u16> = pairs.iter().map(|&p| u16::from_be_bytes(p)).collect();
            String::from_utf16(&units).ok().filter(|_| odd.is_empty())
        };
        if let Some(text) = text.map(|t| t.trim().to_string()).filter(|t| !t.is_empty()) {
            best = Some((rank, text));
        }
    }
    best.map(|(_, text)| text)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    const BOOTH_MIT: &str = include_str!("../../../LICENSE-MIT");
    const URL: &str = "https://raw.githubusercontent.com/phosphor-icons/homepage/master/LICENSE";

    // (platform, encoding, language, name id, text), the way Phosphor.ttf
    // carries them: a Mac entry and a Windows one for each.
    fn phosphor_names(copyright: &str, license: &str) -> Vec<(u16, u16, u16, u16, String)> {
        let mut names = Vec::new();
        for (platform, encoding, language) in [(1, 0, 0), (3, 1, 0x0409)] {
            for (id, text) in [
                (1, "Phosphor"),
                (COPYRIGHT, copyright),
                (LICENSE, license),
                (LICENSE_URL, URL),
            ] {
                names.push((platform, encoding, language, id, text.to_string()));
            }
        }
        names
    }

    // A font with a head table ahead of its name table, so the name table
    // has to be found among the others.
    fn font(names: &[(u16, u16, u16, u16, String)]) -> Vec<u8> {
        let mut storage = Vec::new();
        let mut records = Vec::new();
        for (platform, encoding, language, id, text) in names {
            let bytes: Vec<u8> = if *platform == 1 {
                text.as_bytes().to_vec()
            } else {
                text.encode_utf16().flat_map(u16::to_be_bytes).collect()
            };
            for field in [
                *platform,
                *encoding,
                *language,
                *id,
                bytes.len() as u16,
                storage.len() as u16,
            ] {
                records.extend(field.to_be_bytes());
            }
            storage.extend(bytes);
        }
        let mut name = Vec::new();
        name.extend(0u16.to_be_bytes());
        name.extend((names.len() as u16).to_be_bytes());
        name.extend((6 + records.len() as u16).to_be_bytes());
        name.extend(records);
        name.extend(storage);

        let head = [0u8; 54];
        let head_at = 12 + 16 * 2;
        let name_at = head_at + head.len();
        let mut out = Vec::new();
        out.extend(0x0001_0000u32.to_be_bytes());
        out.extend(2u16.to_be_bytes());
        out.extend([0u8; 6]);
        for (tag, at, length) in [
            (b"head", head_at, head.len()),
            (b"name", name_at, name.len()),
        ] {
            out.extend(tag);
            out.extend(0u32.to_be_bytes());
            out.extend((at as u32).to_be_bytes());
            out.extend((length as u32).to_be_bytes());
        }
        out.extend(head);
        out.extend(name);
        out
    }

    struct Folder(PathBuf);

    impl Folder {
        fn new(name: &str) -> Folder {
            let dir = std::env::temp_dir()
                .join(format!("booth-release-fonts-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Folder(dir)
        }

        fn with(self, file: &str, bytes: &[u8]) -> Folder {
            let path = self.0.join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, bytes).unwrap();
            self
        }

        fn package(&self) -> Package {
            Package {
                name: "egui-phosphor".into(),
                version: "0.14.0".into(),
                license: Some("MIT OR Apache-2.0".into()),
                dir: self.0.clone(),
            }
        }
    }

    impl Drop for Folder {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn reads_the_name_table() {
        let both = font(&phosphor_names("Phosphor Icons", "MIT"));
        assert_eq!(
            font_name(&both, COPYRIGHT).as_deref(),
            Some("Phosphor Icons")
        );
        assert_eq!(font_name(&both, LICENSE).as_deref(), Some("MIT"));
        assert_eq!(font_name(&both, LICENSE_URL).as_deref(), Some(URL));
        assert_eq!(font_name(&both, 9), None);

        // The Windows entry wins over the Mac one, and the Mac one is used
        // when it is all there is.
        let mut names = phosphor_names("Mac notice", "MIT");
        names[5].4 = "Windows notice".to_string();
        assert_eq!(
            font_name(&font(&names), COPYRIGHT).as_deref(),
            Some("Windows notice")
        );
        names.truncate(4);
        assert_eq!(
            font_name(&font(&names), COPYRIGHT).as_deref(),
            Some("Mac notice")
        );
        names[1].4 = "\u{A9} Phosphor".to_string();
        assert_eq!(font_name(&font(&names), COPYRIGHT), None);
    }

    #[test]
    fn broken_fonts_give_nothing() {
        let whole = font(&phosphor_names("Phosphor Icons", "MIT"));
        for cut in [0, 5, 20, 60, 100, whole.len() - 1] {
            assert_eq!(font_name(&whole[..cut], COPYRIGHT), None, "cut at {cut}");
        }
        let mut wild = whole.clone();
        let name_record = 12 + 16;
        wild[name_record + 8..name_record + 12].copy_from_slice(&u32::MAX.to_be_bytes());
        assert_eq!(font_name(&wild, COPYRIGHT), None);
        assert_eq!(font_name(b"not a font at all", COPYRIGHT), None);
    }

    #[test]
    fn mit_template_is_the_mit_text() {
        assert_eq!(MIT.matches(MIT_COPYRIGHT).count(), 1);
        let booth_line = BOOTH_MIT
            .lines()
            .find(|l| l.starts_with("Copyright"))
            .unwrap();
        assert_eq!(
            normalize(&MIT.replacen(MIT_COPYRIGHT, booth_line, 1)),
            normalize(BOOTH_MIT)
        );
    }

    #[test]
    fn phosphor_notice() {
        let phosphor = font(&phosphor_names("Phosphor Icons", "MIT"));
        let folder = Folder::new("phosphor")
            .with("res/Phosphor.ttf", &phosphor)
            .with("res/Phosphor-Bold.ttf", &phosphor)
            .with("res/README.md", b"not a font");
        let font = read_crate_font(&FROM_CRATES[0], &[folder.package()]).unwrap();
        assert_eq!(font.from, "egui-phosphor 0.14.0");
        assert_eq!(font.copyright, "Phosphor Icons");
        assert_eq!(font.license_url.as_deref(), Some(URL));
        assert!(
            font.license_text()
                .starts_with("MIT License\n\nPhosphor Icons\n\nPermission is hereby granted")
        );

        let plex = Folder::new("plex")
            .with("IBMPlexSans-Regular.ttf", b"plex")
            .with("OFL.txt", b"\r\nSIL OPEN FONT LICENSE Version 1.1\r\n");
        let fonts = read(&plex.0, &[folder.package()]).unwrap();
        assert_eq!(fonts.files, ["IBMPlexSans-Regular.ttf"]);
        let notice = fonts.notice();
        let ofl = notice.find("SIL OPEN FONT LICENSE").unwrap();
        let mit = notice.find("MIT License\n\nPhosphor Icons\n\n").unwrap();
        assert!(ofl < mit, "{notice}");
        assert!(notice.contains("from egui-phosphor 0.14.0."), "{notice}");
        assert!(notice.contains(URL), "{notice}");
        assert!(
            notice.ends_with("OTHER DEALINGS IN THE\nSOFTWARE.\n\n"),
            "{notice}"
        );
    }

    #[test]
    fn phosphor_notice_refusals() {
        let phosphor = font(&phosphor_names("Phosphor Icons", "MIT"));
        let folder = Folder::new("refusals")
            .with("res/Phosphor.ttf", &phosphor)
            .with(
                "res/Phosphor-Fill.ttf",
                &font(&phosphor_names("Someone Else", "MIT")),
            );
        let err = read_crate_font(&FROM_CRATES[0], &[folder.package()])
            .err()
            .unwrap();
        assert!(
            err.contains("Phosphor-Fill.ttf and Phosphor.ttf") && err.contains("different"),
            "{err}"
        );

        let folder = folder.with(
            "res/Phosphor-Fill.ttf",
            &font(&phosphor_names("Phosphor Icons", "OFL-1.1")),
        );
        let err = read_crate_font(&FROM_CRATES[0], &[folder.package()])
            .err()
            .unwrap();
        assert!(err.contains("\"OFL-1.1\""), "{err}");

        let mut names = phosphor_names("Phosphor Icons", "MIT");
        names.retain(|n| n.3 != COPYRIGHT);
        let folder = folder.with("res/Phosphor-Fill.ttf", &font(&names));
        let err = read_crate_font(&FROM_CRATES[0], &[folder.package()])
            .err()
            .unwrap();
        assert!(err.contains("no copyright notice"), "{err}");

        fs::remove_dir_all(folder.0.join("res")).unwrap();
        let err = read_crate_font(&FROM_CRATES[0], &[folder.package()])
            .err()
            .unwrap();
        assert!(err.contains("could not list"), "{err}");

        let err = read_crate_font(&FROM_CRATES[0], &[]).err().unwrap();
        assert!(err.contains("no longer compiled into booth.exe"), "{err}");
    }
}
