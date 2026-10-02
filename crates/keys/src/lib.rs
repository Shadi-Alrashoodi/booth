//! This device's identity key, kept on disk under Windows DPAPI, and the
//! short fingerprint people compare to check a key.

// DPAPI and MoveFileExW are C APIs, so this crate cannot forbid unsafe
// outright. Denying it everywhere except these two small FFI modules keeps
// every unsafe block in dpapi.rs or movefile.rs, where it is easy to find.
#![deny(unsafe_code)]
#![deny(clippy::undocumented_unsafe_blocks)]

#[allow(unsafe_code)]
mod dpapi;
mod error;
mod file;
mod identity;
#[allow(unsafe_code)]
mod movefile;
mod profile;

pub use error::{FileKind, KeyError};
pub use file::{put_aside, read_protected, write_protected};
pub use identity::{IDENTITY_FILE, Identity, fingerprint};
pub use profile::data_dir;
