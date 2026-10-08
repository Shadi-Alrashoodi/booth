// THIRD-PARTY-LICENSES.txt: every crate compiled into booth.exe with its
// license and the text of it, then code copied into Booth's own crates, the
// fonts and FFmpeg. Everything comes from what is on disk already: cargo tree
// and cargo metadata, the registry sources cargo unpacked for the build,
// assets\fonts and third_party.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::expression;
use crate::ffmpeg::Ffmpeg;
use crate::fonts::{self, Fonts};
use crate::json::{self, Value};

// Which licenses a crate may be used under is cargo deny's call (deny.toml).
// This list only picks one when a crate offers a choice, in this order.
const PREFERRED: &[&str] = &[
    "MIT",
    "Apache-2.0",
    "BSD-3-Clause",
    "BSD-2-Clause",
    "ISC",
    "Zlib",
    "BSL-1.0",
    "Unicode-3.0",
    "CC0-1.0",
];

// For crates that ship no text of their license. Only licenses without a
// copyright line to fill in are here; for the others a missing text is a
// question for a person, not something to make up.
const STANDARD: &[(&str, &str)] = &[
    ("Apache-2.0", include_str!("../licenses/Apache-2.0.txt")),
    ("BSL-1.0", include_str!("../licenses/BSL-1.0.txt")),
];

// Code in Booth's own crates copied from another project, and the pinned file
// in third_party whose notice covers it. tools\fetch-third-party.ps1 checks
// that file against its hash.
struct Copied {
    into: &'static str,
    what: &'static str,
    notice_in: &'static str,
}

const COPIED: &[Copied] = &[Copied {
    into: r"crates\encode\src\nvenc\ffi.rs",
    what: "declarations copied from nvEncodeAPI.h in NVIDIA's nv-codec-headers n12.2.72.0",
    notice_in: r"third_party\nv-codec-headers\nvEncodeAPI.h",
}];

const LICENSE_NAMES: &[&str] = &[
    "LICENSE",
    "LICENCE",
    "COPYING",
    "COPYRIGHT",
    "NOTICE",
    "UNLICENSE",
];
// Folders whose code never goes into a build of the crate as a dependency.
const NOT_BUILT: &[&str] = &["tests", "benches", "examples", "target", ".git"];
// A module named license.rs is code, not a license.
const CODE: &[&str] = &[
    "rs", "c", "h", "cc", "cpp", "hpp", "py", "js", "toml", "json", "sh", "ps1",
];

const TARGET: &str = "x86_64-pc-windows-msvc";
pub const RULE: &str = "------------------------------------------------------------------------";
const DOUBLE_RULE: &str =
    "========================================================================";

pub struct Package {
    pub name: String,
    pub version: String,
    pub license: Option<String>,
    pub dir: PathBuf,
}

pub struct Crate {
    pub name: String,
    pub version: String,
    pub expression: String,
    pub used_under: Vec<String>,
    pub texts: Vec<Text>,
}

pub struct Text {
    pub label: String,
    // For a notice, the file in the crate it comes from.
    pub from: Option<String>,
    pub body: String,
    pub standard: bool,
}

pub struct CopiedCode {
    pub into: String,
    pub what: String,
    pub notice: String,
}

pub struct Collected {
    pub file: String,
    pub crate_count: usize,
    pub manual: Vec<String>,
}

// Run from the repository, so cargo picks the toolchain in
// rust-toolchain.toml and the settings in .cargo\config.toml.
fn cargo(root: &Path, args: &[&str]) -> Result<String, String> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = Command::new(&cargo)
        .args(args)
        .args(["--locked", "--offline", "--manifest-path"])
        .arg(root.join("Cargo.toml"))
        .current_dir(root)
        .output()
        .map_err(|err| format!("could not run {}: {err}", cargo.to_string_lossy()))?;
    if !output.status.success() {
        return Err(format!(
            "cargo {} failed ({}): {}",
            args[0],
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    String::from_utf8(output.stdout)
        .map_err(|_| format!("cargo {} printed something that is not UTF-8", args[0]))
}

pub fn cargo_metadata(root: &Path) -> Result<String, String> {
    cargo(
        root,
        &[
            "metadata",
            "--format-version",
            "1",
            "--filter-platform",
            TARGET,
        ],
    )
}

// The crates compiled into booth.exe: app's normal dependencies, all the way
// down, with features as `cargo build -p app` settles them. cargo metadata
// alone cannot say this, because it unifies features across the whole
// workspace and its tests and so lists crates booth.exe never contains.
// Build dependencies only run on the build PC and leave no code in the exe.
// Proc macros only run there too, but the code they write is compiled in,
// so they stay in the list.
pub fn cargo_tree(root: &Path) -> Result<String, String> {
    cargo(
        root,
        &[
            "tree", "-p", "app", "-e", "normal", "--target", TARGET, "--prefix", "none",
            "--format", "{p}",
        ],
    )
}

// Joins the tree's names and versions with metadata's license and folder
// for each crate. Workspace members are Booth's own code and are left out.
pub fn packages_in_exe(
    metadata: &Value,
    tree: &str,
    root_package: &str,
) -> Result<(String, Vec<Package>), String> {
    let members: HashSet<&str> = metadata
        .get("workspace_members")
        .map(Value::as_array)
        .unwrap_or(&[])
        .iter()
        .filter_map(Value::as_str)
        .collect();
    let packages = metadata.get("packages").map(Value::as_array).unwrap_or(&[]);
    let root = packages
        .iter()
        .find(|p| {
            p.str_field("id").is_some_and(|id| members.contains(id))
                && p.str_field("name") == Some(root_package)
        })
        .ok_or_else(|| format!("cargo metadata has no workspace package named {root_package}"))?;
    let booth_version = root.str_field("version").unwrap_or_default().to_string();

    let mut in_tree = BTreeMap::new();
    for line in tree.lines() {
        let mut words = line.split_whitespace();
        let (Some(name), Some(version)) = (words.next(), words.next()) else {
            continue;
        };
        let Some(version) = version.strip_prefix('v') else {
            return Err(format!(
                "cargo tree printed a line this tool cannot read: {line}"
            ));
        };
        in_tree.insert((name, version), false);
    }

    let mut out = Vec::new();
    for package in packages {
        let (Some(id), Some(name), Some(version)) = (
            package.str_field("id"),
            package.str_field("name"),
            package.str_field("version"),
        ) else {
            continue;
        };
        let Some(found) = in_tree.get_mut(&(name, version)) else {
            continue;
        };
        *found = true;
        if members.contains(id) {
            continue;
        }
        let manifest = package
            .str_field("manifest_path")
            .ok_or_else(|| format!("{name} {version} has no manifest_path in cargo metadata"))?;
        out.push(Package {
            name: name.to_string(),
            version: version.to_string(),
            license: package.str_field("license").map(str::to_string),
            dir: Path::new(manifest)
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_default(),
        });
    }
    if let Some(((name, version), _)) = in_tree.iter().find(|(_, found)| !**found) {
        return Err(format!(
            "cargo tree lists {name} {version}, which cargo metadata does not know"
        ));
    }
    out.sort_by(|a, b| (&a.name, &a.version).cmp(&(&b.name, &b.version)));
    Ok((booth_version, out))
}

// Returns the crate, and a sentence when a person should look at what the
// tool decided.
pub fn license_crate(package: &Package) -> Result<(Crate, Option<String>), String> {
    let label = format!("{} {}", package.name, package.version);
    let Some(expression) = package.license.clone() else {
        return Err(format!(
            "{label} has no license expression in its Cargo.toml; read its license and add an answer to the release tool"
        ));
    };
    let offered = expression::alternatives(&expression).map_err(|err| format!("{label}: {err}"))?;
    // Only a crate under exactly one license can have an unrecognised license
    // file taken as that license's text. With a choice, the file might be
    // the text of the half Booth does not use.
    let only_one_license = offered.len() == 1 && offered[0].len() == 1;
    let named: Vec<&str> = offered.iter().flatten().map(|l| family(l)).collect();
    let files = license_files(&package.dir, &named, only_one_license)?;
    let alternatives: Vec<&Vec<String>> = offered
        .iter()
        .filter(|set| set.iter().all(|l| PREFERRED.contains(&l.as_str())))
        .collect();
    if alternatives.is_empty() {
        return Err(format!(
            "{label} is under \"{expression}\", which offers no license the release tool knows"
        ));
    }

    let is = |f: &LicenseFile, kind: Kind, license: &str| {
        f.kind == kind && f.licenses.contains(&license)
    };
    let shipped = |license: &str| {
        files
            .iter()
            .any(|f| is(f, Kind::Text, license) || is(f, Kind::Mixed, license))
    };
    let own_words = files.iter().any(|f| f.kind == Kind::OwnWords);
    let rank = |license: &String| {
        PREFERRED
            .iter()
            .position(|p| p == license)
            .unwrap_or(PREFERRED.len())
    };
    let plain = |license: &str| files.iter().any(|f| is(f, Kind::Text, license));
    // Fewest texts the crate does not ship first, so a standard text is used
    // only when nothing better exists; then fewest texts found only among
    // other licenses; then the preferred licenses.
    let chosen: Vec<String> = alternatives
        .into_iter()
        .filter(|set| set.iter().all(|l| shipped(l) || own_words || standard_text(l).is_some()))
        .min_by_key(|set| {
            (
                set.iter().filter(|l| !shipped(l)).count(),
                set.iter().filter(|l| !plain(l)).count(),
                set.iter().map(rank).sum::<usize>(),
            )
        })
        .ok_or_else(|| {
            format!(
                "{label} is under \"{expression}\" and ships no text of it that the release tool recognises; read its license files in {} and add an answer to the release tool",
                package.dir.display()
            )
        })?
        .clone();

    let mut texts: Vec<Text> = Vec::new();
    let mut manual = None;
    let mut used_files = HashSet::new();
    for license in &chosen {
        let mut own: Vec<usize> = (0..files.len())
            .filter(|&i| is(&files[i], Kind::Text, license))
            .collect();
        if own.is_empty() {
            own = (0..files.len())
                .filter(|&i| is(&files[i], Kind::Mixed, license))
                .collect();
        }
        if !own.is_empty() {
            for i in own {
                if used_files.insert(i) {
                    texts.push(Text {
                        label: license.clone(),
                        from: None,
                        body: files[i].text.clone(),
                        standard: false,
                    });
                }
            }
        } else if own_words {
            for (i, file) in files
                .iter()
                .enumerate()
                .filter(|(_, f)| f.kind == Kind::OwnWords)
            {
                if used_files.insert(i) {
                    texts.push(Text {
                        label: license.clone(),
                        from: None,
                        body: file.text.clone(),
                        standard: false,
                    });
                }
            }
            manual = Some(format!(
                "{label}: its license file is not a text the tool recognises as {license}; it is included as the crate ships it"
            ));
        } else if let Some(text) = standard_text(license) {
            texts.push(Text {
                label: license.clone(),
                from: None,
                body: normalize(text),
                standard: true,
            });
            manual = Some(format!(
                "{label}: ships no {license} text; the standard text is used"
            ));
        }
    }
    for (i, file) in files.iter().enumerate() {
        let notice =
            file.kind == Kind::Notice || (file.kind == Kind::Mixed && !used_files.contains(&i));
        // A crate that bundles a library often ships the library's license
        // at its top as well; the same text is printed once.
        if !notice || texts.iter().any(|t| t.body == file.text) {
            continue;
        }
        texts.push(Text {
            label: "notice".to_string(),
            from: Some(file.path.clone()),
            body: file.text.clone(),
            standard: false,
        });
    }

    let used_under = if offered.len() > 1 {
        chosen
    } else {
        Vec::new()
    };
    Ok((
        Crate {
            name: package.name.clone(),
            version: package.version.clone(),
            expression,
            used_under,
            texts,
        },
        manual,
    ))
}

fn standard_text(license: &str) -> Option<&'static str> {
    STANDARD
        .iter()
        .find(|(id, _)| *id == license)
        .map(|(_, text)| *text)
}

// "GPL-2.0-only", "GPL-2.0-or-later" and "GPL-2.0+" share one text, and an
// exception is added to a license's text, not written into it.
fn family(license: &str) -> &str {
    let base = license.split(" WITH ").next().unwrap_or(license);
    base.strip_suffix("-only")
        .or_else(|| base.strip_suffix("-or-later"))
        .or_else(|| base.strip_suffix('+'))
        .unwrap_or(base)
}

struct LicenseFile {
    // Where it is in the crate: LICENSE-MIT, src/spin/LICENSE.
    path: String,
    text: String,
    licenses: Vec<&'static str>,
    kind: Kind,
}

#[derive(PartialEq, Clone, Copy)]
enum Kind {
    // The text of licenses the crate is offered under.
    Text,
    // The text of licenses the crate is offered under and of others too, as
    // in crossbeam-channel's LICENSE-THIRD-PARTY and libm's LICENSE.txt:
    // used as a license's text only when the crate ships no plainer one, and
    // otherwise kept as a notice.
    Mixed,
    // Kept whatever license is used: NOTICE and COPYRIGHT files, the
    // licenses of code the crate bundles from elsewhere, and in a crate with
    // a choice of licenses, any license file the tool does not recognise.
    Notice,
    // In a crate under one license, a license file the tool does not
    // recognise, taken as that license in the crate's own words.
    OwnWords,
}

fn find_license_files(dir: &Path, found: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries =
        fs::read_dir(dir).map_err(|err| format!("could not list {}: {err}", dir.display()))?;
    // The REUSE layout: LICENSES\MIT.txt and so on.
    let reuse = dir
        .file_name()
        .is_some_and(|n| n.eq_ignore_ascii_case("LICENSES"));
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let name = entry.file_name().to_string_lossy().to_uppercase();
        let path = entry.path();
        if kind.is_dir() {
            if !NOT_BUILT.iter().any(|n| n.eq_ignore_ascii_case(&name)) {
                find_license_files(&path, found)?;
            }
        } else if kind.is_file() {
            let code = path
                .extension()
                .is_some_and(|e| CODE.iter().any(|c| e.eq_ignore_ascii_case(c)));
            if !code && (reuse || LICENSE_NAMES.iter().any(|p| name.starts_with(p))) {
                found.push(path);
            }
        }
    }
    Ok(())
}

// `offered` holds every license the crate's expression names, by family.
fn license_files(
    dir: &Path,
    offered: &[&str],
    only_one_license: bool,
) -> Result<Vec<LicenseFile>, String> {
    let mut paths = Vec::new();
    find_license_files(dir, &mut paths)?;
    paths.sort();

    let mut files = Vec::new();
    for path in paths {
        let bytes =
            fs::read(&path).map_err(|err| format!("could not read {}: {err}", path.display()))?;
        let text = normalize(&decode(&bytes));
        if text.is_empty() {
            continue;
        }
        let inside = path.strip_prefix(dir).unwrap_or(&path);
        let folder = inside.parent().unwrap_or(Path::new("")).as_os_str();
        let reuse = folder.eq_ignore_ascii_case("LICENSES");
        let name = inside
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_uppercase();
        let mut licenses = identify(&text);
        if licenses.is_empty() && reuse {
            let stem = path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            if let Some(known) = PREFERRED.iter().find(|p| p.eq_ignore_ascii_case(&stem)) {
                licenses.push(known);
            }
        }
        let kind = if !(folder.is_empty() || reuse) {
            // Deeper in the crate, a license is for code it bundles from
            // another project, such as tracing-core's copy of spin.
            Kind::Notice
        } else if !licenses.is_empty() {
            let ours = licenses.iter().filter(|l| offered.contains(l)).count();
            if ours == licenses.len() {
                Kind::Text
            } else if ours > 0 {
                Kind::Mixed
            } else {
                Kind::Notice
            }
        } else if only_one_license && !(name.starts_with("NOTICE") || name.starts_with("COPYRIGHT"))
        {
            Kind::OwnWords
        } else {
            Kind::Notice
        };
        files.push(LicenseFile {
            path: inside.to_string_lossy().replace('\\', "/"),
            text,
            licenses,
            kind,
        });
    }
    Ok(files)
}

// Recognises a license by phrases from its full text, so a LICENSE file that
// only says "MIT or Apache-2.0, at your option" is not taken for either text.
fn identify(text: &str) -> Vec<&'static str> {
    let flat = flatten(text);
    let has = |phrase: &str| flat.contains(phrase);
    let mut found = Vec::new();
    if has("apache license") && has("terms and conditions for use, reproduction, and distribution")
    {
        found.push("Apache-2.0");
    }
    // Some copies add "(including the next paragraph)" before "shall be
    // included"; the condition is the same.
    if has("permission is hereby granted, free of charge, to any person obtaining a copy")
        && has("the above copyright notice and this permission notice")
    {
        found.push("MIT");
    }
    if has("redistribution and use in source and binary forms")
        && !has("all advertising materials mentioning")
    {
        if has("neither the name") || has("may be used to endorse or promote products") {
            found.push("BSD-3-Clause");
        } else {
            found.push("BSD-2-Clause");
        }
    }
    if (has(
        "permission to use, copy, modify, and/or distribute this software for any purpose with or without fee is hereby granted, provided that the above copyright notice and this permission notice appear in all copies",
    ) || has(
        "permission to use, copy, modify, and distribute this software for any purpose with or without fee is hereby granted, provided that the above copyright notice and this permission notice appear in all copies",
    )) && !found.contains(&"MIT")
    {
        found.push("ISC");
    }
    if has("altered source versions must be plainly marked as such") {
        found.push("Zlib");
    }
    if has("boost software license") {
        found.push("BSL-1.0");
    }
    if has("unicode license v3") {
        found.push("Unicode-3.0");
    }
    if has("cc0 1.0 universal") {
        found.push("CC0-1.0");
    }
    // Never used, but told apart from notices, so the text of the half of a
    // choice Booth does not take is left out rather than kept as a notice.
    if has(
        "permission to use, copy, modify, and/or distribute this software for any purpose with or without fee is hereby granted.",
    ) {
        found.push("0BSD");
    }
    if has("this is free and unencumbered software released into the public domain") {
        found.push("Unlicense");
    }
    for (title, id) in [
        ("gnu general public license version 2, june 1991", "GPL-2.0"),
        (
            "gnu general public license version 3, 29 june 2007",
            "GPL-3.0",
        ),
        (
            "gnu lesser general public license version 2.1, february 1999",
            "LGPL-2.1",
        ),
        (
            "gnu lesser general public license version 3, 29 june 2007",
            "LGPL-3.0",
        ),
    ] {
        if has(title) {
            found.push(id);
        }
    }
    found
}

fn flatten(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for word in text.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        for c in word.chars() {
            let plain = match c {
                '\u{2018}' | '\u{2019}' => '\'',
                '\u{201C}' | '\u{201D}' => '"',
                _ => c,
            };
            out.extend(plain.to_lowercase());
        }
    }
    out
}

pub fn decode(bytes: &[u8]) -> String {
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_string(),
        // Older license files are sometimes Latin-1, for a copyright sign or
        // an accented name.
        Err(_) => bytes.iter().map(|&b| b as char).collect(),
    }
}

// Line ends and trailing spaces differ between copies of the same license;
// without them identical texts group together.
pub fn normalize(text: &str) -> String {
    let text = text
        .strip_prefix('\u{FEFF}')
        .unwrap_or(text)
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    let lines: Vec<&str> = text.lines().map(str::trim_end).collect();
    let first = lines
        .iter()
        .position(|l| !l.is_empty())
        .unwrap_or(lines.len());
    let last = lines
        .iter()
        .rposition(|l| !l.is_empty())
        .map_or(first, |i| i + 1);
    lines[first..last].join("\n")
}

fn read_copied(root: &Path) -> Result<Vec<CopiedCode>, String> {
    let mut out = Vec::new();
    for copied in COPIED {
        let into = root.join(copied.into);
        if !into.is_file() {
            return Err(format!(
                "{} is gone; take it out of the list of copied code in crates\\release\\src\\licenses.rs",
                into.display()
            ));
        }
        let path = root.join(copied.notice_in);
        let bytes = fs::read(&path).map_err(|err| {
            format!(
                "could not read {}: {err}; run powershell -ExecutionPolicy Bypass -File tools\\fetch-third-party.ps1 first",
                path.display()
            )
        })?;
        let notice = leading_comment(&decode(&bytes))
            .filter(|n| !identify(n).is_empty())
            .ok_or_else(|| {
                format!(
                    "{} does not start with a license notice the release tool recognises",
                    path.display()
                )
            })?;
        out.push(CopiedCode {
            into: copied.into.to_string(),
            what: copied.what.to_string(),
            notice,
        });
    }
    Ok(out)
}

// The /* */ comment a C header starts with, without its margin of stars.
fn leading_comment(text: &str) -> Option<String> {
    let body = text.trim_start().strip_prefix("/*")?;
    let body = &body[..body.find("*/")?];
    let lines: Vec<&str> = body
        .lines()
        .map(|line| {
            let line = line.trim_start();
            let line = line.strip_prefix('*').unwrap_or(line);
            line.strip_prefix(' ').unwrap_or(line)
        })
        .collect();
    Some(normalize(&lines.join("\n"))).filter(|t| !t.is_empty())
}

pub fn collect(root: &Path, metadata: &str, tree: &str) -> Result<(Collected, Ffmpeg), String> {
    let metadata = json::parse(metadata)?;
    let (version, packages) = packages_in_exe(&metadata, tree, "app")?;
    let mut crates = Vec::new();
    let mut manual = Vec::new();
    for package in &packages {
        let (krate, note) = license_crate(package)?;
        crates.push(krate);
        manual.extend(note);
    }
    let copied = read_copied(root)?;
    let fonts = fonts::read(&root.join("assets").join("fonts"), &packages)?;
    let ffmpeg = Ffmpeg::read(&root.join("third_party").join("ffmpeg"))?;
    let file = render(&version, &crates, &copied, &fonts, &ffmpeg);
    Ok((
        Collected {
            file,
            crate_count: crates.len(),
            manual,
        },
        ffmpeg,
    ))
}

pub fn render(
    version: &str,
    crates: &[Crate],
    copied: &[CopiedCode],
    fonts: &Fonts,
    ffmpeg: &Ffmpeg,
) -> String {
    let mut out = String::new();
    out.push_str(&format!("Third-party software in Booth {version}\n\n"));
    out.push_str(&wrap(
        "Booth's own code is under the MIT license or the Apache License 2.0, at your option (LICENSE-MIT and LICENSE-APACHE). booth.exe also contains the Rust crates in part 1 and the code in part 2, compiled in, and the fonts in part 3. It loads the FFmpeg DLLs in part 4 from its own folder. The licenses and notices of each part follow.",
        "",
    ));
    out.push('\n');
    out.push_str(&wrap(
        "Where a crate offers a choice of licenses, Booth uses it under the one named after \"used under\". Where a crate ships no text of its license, the standard text is given and marked as such. A notice is printed with the file in the crate it comes from. Crates that only run while building (cc, cmake and the like) put no code in booth.exe and are not listed; proc macros are listed, because the code they write is compiled in. The Rust standard library, also compiled in, is under the MIT license or the Apache License 2.0, like Booth.",
        "",
    ));
    out.push_str(&format!(
        "\n1. Rust crates ({})\n2. Code copied into Booth\n3. Fonts\n4. FFmpeg\n\n",
        crates.len()
    ));

    heading(&mut out, "1. Rust crates");
    let width = crates
        .iter()
        .map(|c| c.name.len() + c.version.len() + 1)
        .max()
        .unwrap_or(0)
        + 2;
    for krate in crates {
        let name = format!("{} {}", krate.name, krate.version);
        let mut line = format!("{name:width$}{}", krate.expression);
        if !krate.used_under.is_empty() {
            line.push_str(&format!(", used under {}", krate.used_under.join(" AND ")));
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out.push('\n');

    // Many crates ship the very same text; each distinct text is printed
    // once, with every crate that ships it.
    let mut groups: Vec<(&Text, Vec<String>)> = Vec::new();
    for krate in crates {
        for text in &krate.texts {
            let who = match &text.from {
                Some(file) => format!("{} {} ({file})", krate.name, krate.version),
                None => format!("{} {}", krate.name, krate.version),
            };
            match groups.iter_mut().find(|(t, _)| {
                t.label == text.label && t.body == text.body && t.standard == text.standard
            }) {
                Some((_, list)) => list.push(who),
                None => groups.push((text, vec![who])),
            }
        }
    }
    for (text, who) in &groups {
        let head = if text.standard {
            format!(
                "{}, the standard text, for crates that ship none: {}",
                text.label,
                who.join(", ")
            )
        } else if text.label == "notice" {
            format!("Notice shipped with {}", who.join(", "))
        } else {
            format!("{}, as shipped with {}", text.label, who.join(", "))
        };
        out.push_str(RULE);
        out.push('\n');
        out.push_str(&wrap(&head, ""));
        out.push_str(RULE);
        out.push_str("\n\n");
        out.push_str(&text.body);
        out.push_str("\n\n");
    }

    heading(&mut out, "2. Code copied into Booth");
    for code in copied {
        out.push_str(&wrap(
            &format!(
                "{} holds {}. It is compiled into booth.exe under this notice:",
                code.into, code.what
            ),
            "",
        ));
        out.push('\n');
        out.push_str(&code.notice);
        out.push_str("\n\n");
    }

    heading(&mut out, "3. Fonts");
    out.push_str(&fonts.notice());

    heading(&mut out, "4. FFmpeg");
    out.push_str(&ffmpeg.notice());
    out
}

fn heading(out: &mut String, title: &str) {
    out.push_str(DOUBLE_RULE);
    out.push('\n');
    out.push_str(title);
    out.push('\n');
    out.push_str(DOUBLE_RULE);
    out.push_str("\n\n");
}

pub fn wrap(text: &str, indent: &str) -> String {
    let mut out = String::new();
    let mut line = String::from(indent);
    for word in text.split_whitespace() {
        if line.len() > indent.len() && line.len() + 1 + word.len() > RULE.len() {
            out.push_str(&line);
            out.push('\n');
            line = String::from(indent);
        }
        if line.len() > indent.len() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if line.len() > indent.len() {
        out.push_str(&line);
        out.push('\n');
    }
    out
}

pub fn write_crlf(path: &Path, text: &str) -> Result<(), String> {
    let text = text.replace("\r\n", "\n").replace('\n', "\r\n");
    fs::write(path, text).map_err(|err| format!("could not write {}: {err}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIT_TEXT: &str = include_str!("../../../LICENSE-MIT");

    struct Folder(PathBuf);

    impl Folder {
        fn new(name: &str) -> Folder {
            let dir =
                std::env::temp_dir().join(format!("booth-release-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Folder(dir)
        }

        fn with(self, file: &str, text: &str) -> Folder {
            let path = self.0.join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
            self
        }

        fn package(&self, license: &str) -> Package {
            Package {
                name: "sample".into(),
                version: "1.0.0".into(),
                license: Some(license.into()),
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
    fn recognises_the_texts_in_booths_tree() {
        assert_eq!(identify(STANDARD[0].1), ["Apache-2.0"]);
        assert_eq!(identify(STANDARD[1].1), ["BSL-1.0"]);
        assert_eq!(identify(MIT_TEXT), ["MIT"]);
        let bsd3 = "Redistribution and use in source and binary forms, with or without modification, are permitted. Neither the name of the copyright holder nor the names of its contributors may be used to endorse or promote products";
        assert_eq!(identify(bsd3), ["BSD-3-Clause"]);
        assert_eq!(
            identify("Redistribution and use in source and binary\n   forms, with or without"),
            ["BSD-2-Clause"]
        );
        let isc = "Permission to use, copy, modify, and/or distribute this software for any\npurpose with or without fee is hereby granted, provided that the above\ncopyright notice and this permission notice appear in all copies.";
        assert_eq!(identify(isc), ["ISC"]);
        assert_eq!(
            identify("2. Altered source versions must be plainly marked as such, and"),
            ["Zlib"]
        );
        assert_eq!(
            identify("UNICODE LICENSE V3\n\nCOPYRIGHT AND PERMISSION NOTICE"),
            ["Unicode-3.0"]
        );
    }

    #[test]
    fn pointer_is_not_a_text() {
        let pointer =
            "Licensed under either of Apache License, Version 2.0 or MIT license at your option.";
        assert!(identify(pointer).is_empty());
    }

    #[test]
    fn choice_prefers_shipped_text() {
        let folder = Folder::new("ships-mit").with("LICENSE-MIT", MIT_TEXT);
        let (krate, manual) = license_crate(&folder.package("Apache-2.0 OR MIT")).unwrap();
        assert_eq!(krate.used_under, ["MIT"]);
        assert_eq!(krate.texts.len(), 1);
        assert!(!krate.texts[0].standard);
        assert!(manual.is_none());
    }

    #[test]
    fn no_text_gets_standard_apache() {
        let folder = Folder::new("ships-none");
        let (krate, manual) = license_crate(&folder.package("MIT OR Apache-2.0")).unwrap();
        assert_eq!(krate.used_under, ["Apache-2.0"]);
        assert!(krate.texts[0].standard);
        assert!(manual.unwrap().contains("standard text"));

        let err = license_crate(&folder.package("MIT")).err().unwrap();
        assert!(
            err.contains("sample 1.0.0") && err.contains("add an answer"),
            "{err}"
        );
    }

    // The GPL text is recognised as the other half of the choice, so it is
    // neither used nor kept as a notice.
    #[test]
    fn gpl_alternatives_are_never_picked() {
        let folder = Folder::new("gpl-choice")
            .with("LICENSE-APACHE", STANDARD[0].1)
            .with(
                "LICENSE-GPLv2",
                "                    GNU GENERAL PUBLIC LICENSE\n                       Version 2, June 1991\n",
            );
        let (krate, _) = license_crate(&folder.package("Apache-2.0 OR GPL-2.0-only")).unwrap();
        assert_eq!(krate.used_under, ["Apache-2.0"]);
        assert_eq!(krate.texts.len(), 1);

        let err = license_crate(&folder.package("GPL-3.0-only"))
            .err()
            .unwrap();
        assert!(err.contains("offers no license"), "{err}");
    }

    #[test]
    fn and_takes_every_text_and_keeps_notices() {
        let folder = Folder::new("and")
            .with("LICENSE-MIT", MIT_TEXT)
            .with(
                "LICENSE-UNICODE",
                "UNICODE LICENSE V3\n\nCOPYRIGHT AND PERMISSION NOTICE",
            )
            .with(
                "NOTICE",
                "This product includes software from the sample project.",
            );
        let (krate, _) =
            license_crate(&folder.package("(MIT OR Apache-2.0) AND Unicode-3.0")).unwrap();
        assert_eq!(krate.used_under, ["MIT", "Unicode-3.0"]);
        let labels: Vec<&str> = krate.texts.iter().map(|t| t.label.as_str()).collect();
        assert_eq!(labels, ["MIT", "Unicode-3.0", "notice"]);
    }

    #[test]
    fn single_license_own_words() {
        let folder = Folder::new("own-words").with(
            "LICENSE",
            "Copyright 2020 Someone. Old style permission text.",
        );
        let (krate, manual) = license_crate(&folder.package("MIT")).unwrap();
        assert!(krate.used_under.is_empty());
        assert_eq!(
            krate.texts[0].body,
            "Copyright 2020 Someone. Old style permission text."
        );
        assert!(manual.unwrap().contains("not a text the tool recognises"));
    }

    const BSD3_TEXT: &str = "Copyright 2009 The Go Authors.\n\nRedistribution and use in source and binary forms, with or without modification, are permitted. Neither the name of Google Inc. nor the names of its contributors may be used to endorse or promote products";

    #[test]
    fn bundled_code_notices() {
        let spin = MIT_TEXT
            .lines()
            .map(|line| {
                if line.starts_with("Copyright") {
                    "Copyright (c) 2014 Mathijs van de Nes"
                } else {
                    line
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        let folder = Folder::new("bundled")
            .with("LICENSE-MIT", MIT_TEXT)
            .with("LICENSE-APACHE", STANDARD[0].1)
            .with("LICENSE-THIRD-PARTY", &format!("{MIT_TEXT}\n\n{BSD3_TEXT}"))
            .with(
                "LICENSE-ODD",
                "Parts of this crate are also under terms of their own.",
            )
            .with("src/spin/LICENSE", &spin)
            .with("vendor/lib/COPYING", MIT_TEXT)
            .with("tests/data/LICENSE", "fixture, never built")
            .with("src/license.rs", "pub fn license() {}");
        let texts = |krate: &Crate| -> Vec<(String, Option<String>)> {
            krate
                .texts
                .iter()
                .map(|t| (t.label.clone(), t.from.clone()))
                .collect()
        };
        let notice = |file: &str| ("notice".to_string(), Some(file.to_string()));

        // vendor/lib/COPYING is the MIT text already printed, so it is not
        // printed twice.
        let (krate, _) = license_crate(&folder.package("MIT OR Apache-2.0")).unwrap();
        assert_eq!(krate.used_under, ["MIT"]);
        assert_eq!(
            texts(&krate),
            [
                ("MIT".to_string(), None),
                notice("LICENSE-ODD"),
                notice("LICENSE-THIRD-PARTY"),
                notice("src/spin/LICENSE"),
            ]
        );

        fs::remove_file(folder.0.join("LICENSE-MIT")).unwrap();
        let (krate, _) = license_crate(&folder.package("MIT OR Apache-2.0")).unwrap();
        assert_eq!(krate.used_under, ["Apache-2.0"]);
        assert_eq!(
            texts(&krate),
            [
                ("Apache-2.0".to_string(), None),
                notice("LICENSE-ODD"),
                notice("LICENSE-THIRD-PARTY"),
                notice("src/spin/LICENSE"),
                notice("vendor/lib/COPYING"),
            ]
        );
    }

    // libm's LICENSE.txt holds the MIT text and the Apache text, and it is
    // the only MIT text libm ships.
    #[test]
    fn mixed_file_as_text() {
        let folder = Folder::new("mixed").with(
            "LICENSE.txt",
            &format!(
                "{MIT_TEXT}\n\nContributions also under:\n\n{}",
                STANDARD[0].1
            ),
        );
        let (krate, manual) = license_crate(&folder.package("MIT")).unwrap();
        assert_eq!(krate.texts.len(), 1);
        assert_eq!(krate.texts[0].label, "MIT");
        assert!(krate.texts[0].from.is_none() && manual.is_none());
    }

    #[test]
    fn license_families() {
        assert_eq!(family("GPL-2.0-only"), "GPL-2.0");
        assert_eq!(family("GPL-2.0+"), "GPL-2.0");
        assert_eq!(family("Apache-2.0 WITH LLVM-exception"), "Apache-2.0");
        assert_eq!(family("MIT"), "MIT");
        assert_eq!(
            identify("This is free and unencumbered software released into the public domain."),
            ["Unlicense"]
        );
        assert_eq!(
            identify(
                "Permission to use, copy, modify, and/or distribute this software for any\npurpose with or without fee is hereby granted.\n\nTHE SOFTWARE IS PROVIDED"
            ),
            ["0BSD"]
        );
    }

    const HEADER: &str = "/*\n * This copyright notice applies to this header file only:\n *\n * Copyright (c) 2010-2024 NVIDIA Corporation\n *\n * Permission is hereby granted, free of charge, to any person obtaining a copy of this software.\n *\n * The above copyright notice and this permission notice shall be\n * included in all copies or substantial portions of the Software.\n */\n\n/**\n * \\file nvEncodeAPI.h\n */\n";

    #[test]
    fn copied_code_notice() {
        assert_eq!(
            leading_comment(HEADER).unwrap(),
            "This copyright notice applies to this header file only:\n\nCopyright (c) 2010-2024 NVIDIA Corporation\n\nPermission is hereby granted, free of charge, to any person obtaining a copy of this software.\n\nThe above copyright notice and this permission notice shall be\nincluded in all copies or substantial portions of the Software."
        );
        assert_eq!(leading_comment("#include <stdlib.h>\n/* late */"), None);

        let root = Folder::new("copied")
            .with(COPIED[0].into, "// the copy")
            .with(COPIED[0].notice_in, HEADER);
        let copied = read_copied(&root.0).unwrap();
        assert_eq!(copied[0].into, r"crates\encode\src\nvenc\ffi.rs");
        assert!(
            copied[0]
                .notice
                .starts_with("This copyright notice applies")
        );

        fs::write(root.0.join(COPIED[0].notice_in), "#pragma once\n").unwrap();
        let err = read_copied(&root.0).err().unwrap();
        assert!(
            err.contains("does not start with a license notice"),
            "{err}"
        );
        fs::remove_file(root.0.join(COPIED[0].notice_in)).unwrap();
        let err = read_copied(&root.0).err().unwrap();
        assert!(err.contains("fetch-third-party.ps1"), "{err}");
        fs::remove_file(root.0.join(COPIED[0].into)).unwrap();
        let err = read_copied(&root.0).err().unwrap();
        assert!(err.contains("is gone"), "{err}");
    }

    #[test]
    fn normalize_and_decode() {
        assert_eq!(normalize("\u{FEFF}\r\n\r\nA  \r\nB\rC\n\n\n"), "A\nB\nC");
        assert_eq!(decode(b"\xA9 2019 Jos\xE9"), "\u{A9} 2019 Jos\u{E9}");
    }

    #[test]
    fn wraps_at_the_rule_width() {
        let text = "word ".repeat(40);
        for line in wrap(&text, "  ").lines() {
            assert!(line.len() <= RULE.len() && line.starts_with("  "));
        }
    }

    // As cargo tree prints it with --prefix none and --format {p}: repeats
    // marked (*), proc macros and path crates marked in brackets.
    const TREE: &str = "app v0.1.0 (C:/p/crates/app)
net v0.1.0 (C:/p/crates/net)
lib v2.0.0
deep v1.1.0 (proc-macro)
lib v2.0.0 (*)
patched v0.3.0 (C:/p/vendor/patched)
";

    fn metadata(extra: &str) -> Value {
        json::parse(&format!(
            r#"{{
              "workspace_members": ["app-id", "net-id"],
              "packages": [
                {{"id": "app-id", "name": "app", "version": "0.1.0", "license": "MIT OR Apache-2.0", "manifest_path": "C:\\p\\crates\\app\\Cargo.toml"}},
                {{"id": "net-id", "name": "net", "version": "0.1.0", "license": "MIT OR Apache-2.0", "manifest_path": "C:\\p\\crates\\net\\Cargo.toml"}},
                {{"id": "lib-id", "name": "lib", "version": "2.0.0", "license": "MIT", "manifest_path": "C:\\r\\lib-2.0.0\\Cargo.toml"}},
                {{"id": "lib-old", "name": "lib", "version": "1.0.0", "license": "MIT", "manifest_path": "C:\\r\\lib-1.0.0\\Cargo.toml"}},
                {{"id": "deep-id", "name": "deep", "version": "1.1.0", "license": "Zlib", "manifest_path": "C:\\r\\deep-1.1.0\\Cargo.toml"}},
                {{"id": "cc-id", "name": "cc", "version": "1.4.7", "license": "MIT OR Apache-2.0", "manifest_path": "C:\\r\\cc-1.4.7\\Cargo.toml"}}
                {extra}
              ]
            }}"#
        ))
        .unwrap()
    }

    #[test]
    fn crates_in_the_tree() {
        let patched = r#", {"id": "patched-id", "name": "patched", "version": "0.3.0", "license": "MIT OR Apache-2.0", "manifest_path": "C:\\p\\vendor\\patched\\Cargo.toml"}"#;
        let (version, packages) = packages_in_exe(&metadata(patched), TREE, "app").unwrap();
        assert_eq!(version, "0.1.0");
        let names: Vec<String> = packages
            .iter()
            .map(|p| format!("{} {}", p.name, p.version))
            .collect();
        assert_eq!(names, ["deep 1.1.0", "lib 2.0.0", "patched 0.3.0"]);
        assert_eq!(packages[1].dir, Path::new(r"C:\r\lib-2.0.0"));
        assert_eq!(packages[2].dir, Path::new(r"C:\p\vendor\patched"));
    }

    #[test]
    fn unexplained_tree() {
        let err = packages_in_exe(&metadata(""), TREE, "app").err().unwrap();
        assert!(err.contains("patched 0.3.0"), "{err}");
        let err = packages_in_exe(&metadata(""), "lib v2.0.0\n", "booth")
            .err()
            .unwrap();
        assert!(err.contains("no workspace package named booth"), "{err}");
        let err = packages_in_exe(&metadata(""), "lib 2.0.0\n", "app")
            .err()
            .unwrap();
        assert!(err.contains("cannot read"), "{err}");
    }
}
