// Holds ffi.rs against nvEncodeAPI.h itself, through layout.c, which the
// build script compiles against the pinned header. A copying mistake in
// ffi.rs fails here instead of corrupting memory inside the driver.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{CStr, c_char};

use super::ffi::{self, GUID};

#[repr(C)]
struct Entry {
    name: *const c_char,
    value: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct GuidEntry {
    name: *const c_char,
    value: GUID,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct BitsEntry {
    name: *const c_char,
    offset: u32,
    mask: u32,
}

unsafe extern "C" {
    fn booth_nvenc_layout(count: *mut usize) -> *const Entry;
    fn booth_nvenc_guids(out: *mut GuidEntry, capacity: usize) -> usize;
    fn booth_nvenc_bitfields(out: *mut BitsEntry, capacity: usize) -> usize;
}

fn name(ptr: *const c_char) -> String {
    // SAFETY: every name in layout.c is a string literal.
    unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned()
}

fn header_layout() -> BTreeMap<String, u64> {
    let mut count = 0;
    // SAFETY: returns a pointer to a static array and writes its length.
    let entries = unsafe { booth_nvenc_layout(&mut count) };
    // SAFETY: `entries` points at `count` initialized entries that live for
    // the whole program.
    let entries = unsafe { std::slice::from_raw_parts(entries, count) };
    entries.iter().map(|e| (name(e.name), e.value)).collect()
}

fn header_guids() -> BTreeMap<String, GUID> {
    let mut out = [GuidEntry {
        name: std::ptr::null(),
        value: GUID::default(),
    }; 32];
    // SAFETY: `out` has room for `out.len()` entries; the function writes at
    // most that many and returns how many it has.
    let n = unsafe { booth_nvenc_guids(out.as_mut_ptr(), out.len()) };
    assert!(
        n <= out.len(),
        "layout.c has {n} GUIDs, more than the test's buffer"
    );
    out[..n].iter().map(|e| (name(e.name), e.value)).collect()
}

fn header_bits() -> BTreeMap<String, (u32, u32)> {
    let mut out = [BitsEntry {
        name: std::ptr::null(),
        offset: 0,
        mask: 0,
    }; 64];
    // SAFETY: as in header_guids.
    let n = unsafe { booth_nvenc_bitfields(out.as_mut_ptr(), out.len()) };
    assert!(
        n <= out.len(),
        "layout.c has {n} bitfield records, more than the test's buffer"
    );
    out[..n]
        .iter()
        .map(|e| (name(e.name), (e.offset, e.mask)))
        .collect()
}

#[derive(Default)]
struct Check {
    // Everything layout.c exports that no Rust declaration has asked about.
    unchecked: BTreeSet<String>,
    mismatches: Vec<String>,
    checked: usize,
}

impl Check {
    fn value(&mut self, what: &str, rust: u64, header: Option<u64>) {
        self.checked += 1;
        self.unchecked.remove(what);
        match header {
            None => self.fail(format!("{what}: layout.c does not export it")),
            Some(h) if h != rust => {
                self.fail(format!("{what}: Rust has {rust}, the header has {h}"))
            }
            Some(_) => {}
        }
    }

    fn fail(&mut self, message: String) {
        self.mismatches.push(message);
    }
}

#[test]
fn rust_types_match_the_header() {
    let layout = header_layout();
    let bits = header_bits();
    let guids = header_guids();
    let mut check = Check {
        unchecked: layout
            .keys()
            .chain(bits.keys())
            .chain(guids.keys())
            .cloned()
            .collect(),
        ..Check::default()
    };

    // GUID comes from the windows crate rather than ffi.rs.
    check.value(
        "size GUID",
        size_of::<GUID>() as u64,
        layout.get("size GUID").copied(),
    );
    check.value(
        "align GUID",
        align_of::<GUID>() as u64,
        layout.get("align GUID").copied(),
    );

    for ty in ffi::LAYOUT {
        let size = format!("size {}", ty.name);
        check.value(&size, ty.size as u64, layout.get(&size).copied());
        let align = format!("align {}", ty.name);
        check.value(&align, ty.align as u64, layout.get(&align).copied());

        for (field, offset) in ty.fields {
            let key = format!("{}.{field}", ty.name);
            if *field == "bitfields" {
                // A run of C bitfields has no offsetof. layout.c sets every
                // member of the run and reports which word the bits landed
                // in; together they must fill it, since Rust holds the run
                // as one u32.
                let header = bits.get(&key).copied();
                if let Some((_, mask)) = header
                    && mask != u32::MAX
                {
                    check.fail(format!(
                        "{key}: the header's bitfields cover {mask:#010x}, not the whole word"
                    ));
                }
                check.value(&key, *offset as u64, header.map(|(at, _)| u64::from(at)));
            } else {
                check.value(&key, *offset as u64, layout.get(&key).copied());
            }
        }
    }

    for field in ffi::BITFIELDS {
        let mask = (u32::MAX >> (32 - field.width)) << field.shift;
        match bits.get(field.name) {
            None => check.value(field.name, 0, None),
            Some(&(at, header_mask)) => {
                check.value(field.name, field.word as u64, Some(u64::from(at)));
                if header_mask != mask {
                    check.fail(format!(
                        "{}: Rust sets bits {mask:#010x}, the header's are {header_mask:#010x}",
                        field.name
                    ));
                }
            }
        }
    }

    for (constant, value) in ffi::CONSTS {
        check.value(constant, u64::from(*value), layout.get(*constant).copied());
    }

    for (guid_name, value) in ffi::GUIDS {
        check.checked += 1;
        check.unchecked.remove(*guid_name);
        match guids.get(*guid_name) {
            None => check.fail(format!("{guid_name}: layout.c does not export it")),
            Some(h) if h != value => check.fail(format!(
                "{guid_name}: Rust has {value:?}, the header has {h:?}"
            )),
            Some(_) => {}
        }
    }

    let extra: Vec<String> = check.unchecked.iter().cloned().collect();
    for name in extra {
        check.fail(format!(
            "{name}: layout.c exports it but ffi.rs does not declare it"
        ));
    }

    assert!(
        check.mismatches.is_empty(),
        "{} of {} layout checks failed, first: {}\nall:\n{}",
        check.mismatches.len(),
        check.checked,
        check.mismatches[0],
        check.mismatches.join("\n")
    );
    println!(
        "{} sizes, alignments, offsets, bitfields, constants and GUIDs match nvEncodeAPI.h",
        check.checked
    );
}

#[test]
fn setters_touch_only_their_own_bits() {
    let mut rc: ffi::NV_ENC_RC_PARAMS = ffi::zeroed();
    rc.set_zero_reorder_delay(1);
    assert_eq!(rc.bitfields, 1 << 9);
    rc.set_zero_reorder_delay(0);
    assert_eq!(rc.bitfields, 0);

    let mut h264: ffi::NV_ENC_CONFIG_H264 = ffi::zeroed();
    h264.bitfields = u32::MAX;
    h264.set_enable_intra_refresh(0);
    assert_eq!(h264.bitfields, !(1 << 10));

    // The one two-bit field: 4:2:0 is 1.
    let mut hevc: ffi::NV_ENC_CONFIG_HEVC = ffi::zeroed();
    hevc.bitfields = u32::MAX;
    hevc.set_chroma_format_idc(1);
    assert_eq!(hevc.bitfields, !(1 << 10));
}
