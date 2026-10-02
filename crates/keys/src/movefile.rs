use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows_sys::Win32::Storage::FileSystem::{
    MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum IfExists {
    Replace,
    // Fails with io::ErrorKind::AlreadyExists, and the check and the rename
    // are one step, so nothing can slip in between.
    Fail,
}

pub(crate) fn move_file(from: &Path, to: &Path, if_exists: IfExists) -> io::Result<()> {
    let from = wide(from)?;
    let to = wide(to)?;
    // Without WRITE_THROUGH the rename can still sit in the NTFS log cache when
    // we return, and a power cut a few seconds later brings back the old file.
    let flags = match if_exists {
        IfExists::Replace => MOVEFILE_WRITE_THROUGH | MOVEFILE_REPLACE_EXISTING,
        IfExists::Fail => MOVEFILE_WRITE_THROUGH,
    };
    // SAFETY: both pointers point at NUL-terminated UTF-16 strings with no
    // NUL inside them, owned by this function until it returns. MoveFileExW
    // only reads them and keeps no pointer after the call.
    let ok = unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), flags) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn wide(path: &Path) -> io::Result<Vec<u16>> {
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    if wide.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("the path {} contains a NUL character", path.display()),
        ));
    }
    wide.push(0);
    Ok(wide)
}
