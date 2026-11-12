mod common;

use std::fs;
use std::io;
use std::process;

use common::TempDir;
use keys::{KeyError, read_protected, write_protected};

const LEFT_OVER: &[u8] = b"left over from a crash";

// The only test in this program, so its first save takes the first temporary
// name this process can make.
#[test]
fn leftover_temp_files_kept() {
    let dir = TempDir::new("leftover");
    let id = process::id();
    let tmp = |file: &str, n: u32| dir.path().join(format!("{file}.{id}-{n}.tmp"));

    let read_only = tmp("hosts.bin", 0);
    fs::write(&read_only, LEFT_OVER).expect("write leftover");
    let mut permissions = fs::metadata(&read_only).expect("metadata").permissions();
    permissions.set_readonly(true);
    fs::set_permissions(&read_only, permissions).expect("set read-only");
    fs::write(tmp("hosts.bin", 1), LEFT_OVER).expect("write leftover");

    let path = dir.path().join("hosts.bin");
    write_protected(&path, b"fresh", "Booth test").expect("write");
    assert_eq!(read_protected(&path).expect("read").as_slice(), b"fresh");

    // A long run of taken names stops the save instead.
    for n in 3..11 {
        fs::write(tmp("devices.bin", n), LEFT_OVER).expect("write leftover");
    }
    let err = write_protected(&dir.path().join("devices.bin"), b"x", "Booth test")
        .expect_err("every name taken");
    assert!(
        matches!(&err, KeyError::Write { path, source, .. }
            if source.kind() == io::ErrorKind::AlreadyExists && *path == tmp("devices.bin", 10)),
        "{err:?}"
    );
    assert_eq!(
        err.without_folders(),
        "could not save devices.bin: 8 temporary names in a row are taken"
    );

    let names = dir.file_names();
    assert_eq!(names.len(), 11, "{names:?}");
    for name in names.iter().filter(|name| name.ends_with(".tmp")) {
        let bytes = fs::read(dir.path().join(name)).expect("read leftover");
        assert_eq!(bytes, LEFT_OVER, "{name}");
    }
}
