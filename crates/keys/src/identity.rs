use std::fmt;
use std::io;
use std::path::Path;

use blake2::{Blake2s256, Digest};
use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::Zeroizing;

use crate::error::{FileKind, KeyError};
use crate::file::{self, NewFile};

pub const IDENTITY_FILE: &str = "identity.key";
const DESCRIPTION: &str = "Booth identity key";

/// This device's X25519 static key. The public half is who we are to every
/// host and friend; the secret never leaves this struct except as bytes
/// handed to the Noise handshake.
pub struct Identity {
    // StaticSecret wipes itself on drop.
    secret: StaticSecret,
    public: [u8; 32],
}

impl Identity {
    pub fn generate() -> Identity {
        let mut bytes = Zeroizing::new([0u8; 32]);
        // getrandom's Windows 10+ backend is ProcessPrng, which cannot fail,
        // so this is not a path a user can reach.
        getrandom::fill(bytes.as_mut_slice()).expect("the Windows random number generator failed");
        Identity::from_secret(StaticSecret::from(*bytes))
    }

    /// Loads `dir\identity.key`, or makes and saves a new identity if there
    /// is no such file. A file that is there but cannot be read or decrypted
    /// is an error and is left alone: quietly replacing it would lock this PC
    /// out of every host that knows its key.
    pub fn load_or_create(dir: &Path) -> Result<Identity, KeyError> {
        let path = dir.join(IDENTITY_FILE);
        // Two copies started together on a fresh profile both find no file.
        // Only one key can be put in place; the other copy drops its own and
        // loads that one, so both run on the key that is on disk. Going round
        // again needs someone to delete the file in between, hence the limit.
        for _ in 0..3 {
            match load(&path) {
                Err(KeyError::Read { source, .. }) if source.kind() == io::ErrorKind::NotFound => {}
                result => return result,
            }
            let identity = Identity::generate();
            let secret = identity.secret.as_bytes();
            match file::write_new(&path, secret, DESCRIPTION, FileKind::Identity)? {
                NewFile::Written => return Ok(identity),
                NewFile::AlreadyExists => {}
            }
        }
        load(&path)
    }

    pub fn public(&self) -> &[u8; 32] {
        &self.public
    }

    pub fn private_bytes(&self) -> Zeroizing<[u8; 32]> {
        Zeroizing::new(self.secret.to_bytes())
    }

    pub fn fingerprint(&self) -> String {
        fingerprint(&self.public)
    }

    fn from_secret(secret: StaticSecret) -> Identity {
        let public = PublicKey::from(&secret).to_bytes();
        Identity { secret, public }
    }
}

fn load(path: &Path) -> Result<Identity, KeyError> {
    let plain = file::read(path, FileKind::Identity)?;
    let bytes = <&[u8; 32]>::try_from(plain.as_slice()).map_err(|_| KeyError::WrongKeyLength {
        path: path.to_path_buf(),
        len: plain.len(),
    })?;
    Ok(Identity::from_secret(StaticSecret::from(*bytes)))
}

impl fmt::Debug for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Identity")
            .field("fingerprint", &self.fingerprint())
            .finish_non_exhaustive()
    }
}

/// First 48 bits of BLAKE2s-256 over the public key, as "a7f3 9c21 0d4e".
/// 48 bits puts a key made to match a chosen fingerprint about 2^48 tries
/// away, out of reach, while staying short enough to read aloud.
pub fn fingerprint(public: &[u8; 32]) -> String {
    let digest: [u8; 32] = Blake2s256::digest(public).into();
    let [a, b, c, d, e, f, ..] = digest;
    format!("{a:02x}{b:02x} {c:02x}{d:02x} {e:02x}{f:02x}")
}
