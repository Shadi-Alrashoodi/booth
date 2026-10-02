use std::io;
use std::ptr;
use std::slice;

use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::Cryptography::{
    CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
};
use zeroize::{Zeroize, Zeroizing};

// User scope (no CRYPTPROTECT_LOCAL_MACHINE): only this Windows account on
// this PC can decrypt. UI_FORBIDDEN because a prompt would hang a program
// started next to a fullscreen game.
const FLAGS: u32 = CRYPTPROTECT_UI_FORBIDDEN;

pub(crate) fn protect(plain: &[u8], description: &str) -> io::Result<Vec<u8>> {
    let input = input_blob(plain)?;
    let description: Vec<u16> = description.encode_utf16().chain(Some(0)).collect();
    let mut output = empty_blob();

    // SAFETY: `input` points at `plain`, which is borrowed for the whole call,
    // and DPAPI only reads pDataIn. `description` is a NUL-terminated UTF-16
    // string that lives until the end of this function. Entropy, reserved
    // and prompt are allowed to be null. `output` is a valid, writable
    // CRYPT_INTEGER_BLOB for DPAPI to fill in.
    let ok = unsafe {
        CryptProtectData(
            &input,
            description.as_ptr(),
            ptr::null(),
            ptr::null(),
            ptr::null(),
            FLAGS,
            &mut output,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }

    let output = LocalBuffer {
        blob: output,
        secret: false,
    };
    Ok(output.as_slice().to_vec())
}

pub(crate) fn unprotect(blob: &[u8]) -> io::Result<Zeroizing<Vec<u8>>> {
    let input = input_blob(blob)?;
    let mut output = empty_blob();

    // SAFETY: `input` points at `blob`, borrowed for the whole call, and
    // DPAPI only reads pDataIn. A null ppszDataDescr means DPAPI does not
    // allocate a description for us to free. Entropy, reserved and prompt
    // are allowed to be null. `output` is a valid, writable blob.
    let ok = unsafe {
        CryptUnprotectData(
            &input,
            ptr::null_mut(),
            ptr::null(),
            ptr::null(),
            ptr::null(),
            FLAGS,
            &mut output,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }

    let output = LocalBuffer {
        blob: output,
        secret: true,
    };
    Ok(Zeroizing::new(output.as_slice().to_vec()))
}

fn input_blob(data: &[u8]) -> io::Result<CRYPT_INTEGER_BLOB> {
    let len = u32::try_from(data.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} bytes is more than DPAPI can take", data.len()),
        )
    })?;
    // The field is *mut only because the C struct is shared with outputs;
    // DPAPI never writes through an input blob.
    Ok(CRYPT_INTEGER_BLOB {
        cbData: len,
        pbData: data.as_ptr().cast_mut(),
    })
}

fn empty_blob() -> CRYPT_INTEGER_BLOB {
    CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: ptr::null_mut(),
    }
}

/// An output buffer DPAPI allocated with LocalAlloc. Freed on drop, and wiped
/// first when it holds plaintext, so no path out of here leaks either.
struct LocalBuffer {
    blob: CRYPT_INTEGER_BLOB,
    secret: bool,
}

impl LocalBuffer {
    fn as_slice(&self) -> &[u8] {
        if self.blob.pbData.is_null() || self.blob.cbData == 0 {
            return &[];
        }
        // SAFETY: after a successful call DPAPI hands over a buffer of exactly
        // cbData bytes at pbData, and it stays valid and unaliased until
        // LocalFree in drop. The returned slice borrows self, so it cannot
        // outlive the buffer.
        unsafe { slice::from_raw_parts(self.blob.pbData, self.blob.cbData as usize) }
    }
}

impl Drop for LocalBuffer {
    fn drop(&mut self) {
        if self.blob.pbData.is_null() {
            return;
        }
        if self.secret && self.blob.cbData > 0 {
            // SAFETY: same buffer and length as in as_slice. We have &mut self,
            // so no shared slice of it is alive while we overwrite it.
            let plain =
                unsafe { slice::from_raw_parts_mut(self.blob.pbData, self.blob.cbData as usize) };
            plain.zeroize();
        }
        // SAFETY: DPAPI documents LocalFree as the way to release pbData. The
        // pointer came from DPAPI, is non-null, and is freed only here, once.
        unsafe { LocalFree(self.blob.pbData.cast()) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let plain = b"per-peer secret, 32 bytes long..";
        let blob = protect(plain, "Booth test").expect("protect");
        assert!(!blob.windows(plain.len()).any(|w| w == plain));
        let back = unprotect(&blob).expect("unprotect");
        assert_eq!(back.as_slice(), plain);
    }

    #[test]
    fn tampered_blob_is_rejected() {
        let mut blob = protect(b"secret", "Booth test").expect("protect");
        let last = blob.last_mut().expect("blob is not empty");
        *last ^= 0x01;
        assert!(unprotect(&blob).is_err());
    }

    #[test]
    fn garbage_is_rejected() {
        assert!(unprotect(&[0u8; 64]).is_err());
        assert!(unprotect(&[]).is_err());
    }
}
