use std::hint::black_box;

use snow::Error;
use snow::params::{CipherChoice, DHChoice, HashChoice};
use snow::resolvers::{CryptoResolver, DefaultResolver};
use snow::types::{Cipher, Dh, Hash, Random};

// snow 0.10 frees its private keys, cipher keys and hash state without clearing them. Every
// primitive it builds for us comes from here, wrapped so the secret inside is overwritten through
// snow's own setters just before it is freed. The black_box stops the compiler from dropping
// those stores as dead writes to memory about to be freed; that is best effort, not the
// guarantee zeroize gives for memory we own. The chaining key and the psk sit inline in snow's
// HandshakeState, out of reach of this. noise.rs clears the psk; nothing clears the chaining key.
pub(crate) struct WipingResolver;

impl CryptoResolver for WipingResolver {
    fn resolve_rng(&self) -> Option<Box<dyn Random>> {
        DefaultResolver.resolve_rng()
    }

    fn resolve_dh(&self, choice: &DHChoice) -> Option<Box<dyn Dh>> {
        let inner = DefaultResolver.resolve_dh(choice)?;
        Some(Box::new(WipedDh {
            inner,
            keyed: false,
        }))
    }

    fn resolve_hash(&self, choice: &HashChoice) -> Option<Box<dyn Hash>> {
        let inner = DefaultResolver.resolve_hash(choice)?;
        Some(Box::new(WipedHash(inner)))
    }

    fn resolve_cipher(&self, choice: &CipherChoice) -> Option<Box<dyn Cipher>> {
        let inner = DefaultResolver.resolve_cipher(choice)?;
        Some(Box::new(WipedCipher(inner)))
    }
}

struct WipedDh {
    inner: Box<dyn Dh>,
    // Clearing a key costs a scalar multiplication in snow, so a slot never given one is skipped.
    // That is the common case for a host throwing away an initiation it could not read.
    keyed: bool,
}

impl WipedDh {
    fn wipe(&mut self) {
        if self.keyed {
            self.inner.set(&[0; 32]);
            black_box(&self.inner);
        }
    }
}

impl Drop for WipedDh {
    fn drop(&mut self) {
        self.wipe();
    }
}

impl Dh for WipedDh {
    fn name(&self) -> &'static str {
        self.inner.name()
    }

    fn pub_len(&self) -> usize {
        self.inner.pub_len()
    }

    fn priv_len(&self) -> usize {
        self.inner.priv_len()
    }

    fn set(&mut self, privkey: &[u8]) {
        self.keyed = true;
        self.inner.set(privkey);
    }

    fn generate(&mut self, rng: &mut dyn Random) -> Result<(), Error> {
        self.keyed = true;
        self.inner.generate(rng)
    }

    fn pubkey(&self) -> &[u8] {
        self.inner.pubkey()
    }

    fn privkey(&self) -> &[u8] {
        self.inner.privkey()
    }

    fn dh(&self, pubkey: &[u8], out: &mut [u8]) -> Result<(), Error> {
        self.inner.dh(pubkey, out)
    }

    fn dh_len(&self) -> usize {
        self.inner.dh_len()
    }
}

struct WipedCipher(Box<dyn Cipher>);

impl WipedCipher {
    fn wipe(&mut self) {
        self.0.set(&[0; 32]);
        black_box(&self.0);
    }
}

impl Drop for WipedCipher {
    fn drop(&mut self) {
        self.wipe();
    }
}

impl Cipher for WipedCipher {
    fn name(&self) -> &'static str {
        self.0.name()
    }

    fn set(&mut self, key: &[u8; 32]) {
        self.0.set(key);
    }

    fn encrypt(&self, nonce: u64, authtext: &[u8], plaintext: &[u8], out: &mut [u8]) -> usize {
        self.0.encrypt(nonce, authtext, plaintext, out)
    }

    fn decrypt(
        &self,
        nonce: u64,
        authtext: &[u8],
        ciphertext: &[u8],
        out: &mut [u8],
    ) -> Result<usize, Error> {
        self.0.decrypt(nonce, authtext, ciphertext, out)
    }

    fn rekey(&mut self) {
        self.0.rekey();
    }
}

// HKDF leaves the last block it hashed, derived from the chaining key, in the hasher's buffer.
struct WipedHash(Box<dyn Hash>);

impl WipedHash {
    fn wipe(&mut self) {
        self.0.reset();
        black_box(&self.0);
    }
}

impl Drop for WipedHash {
    fn drop(&mut self) {
        self.wipe();
    }
}

impl Hash for WipedHash {
    fn name(&self) -> &'static str {
        self.0.name()
    }

    fn block_len(&self) -> usize {
        self.0.block_len()
    }

    fn hash_len(&self) -> usize {
        self.0.hash_len()
    }

    fn reset(&mut self) {
        self.0.reset();
    }

    fn input(&mut self, data: &[u8]) {
        self.0.input(data);
    }

    fn result(&mut self, out: &mut [u8]) {
        self.0.result(out);
    }

    fn hmac(&mut self, key: &[u8], data: &[u8], out: &mut [u8]) {
        self.0.hmac(key, data, out);
    }

    fn hkdf(
        &mut self,
        chaining_key: &[u8],
        input_key_material: &[u8],
        outputs: usize,
        out1: &mut [u8],
        out2: &mut [u8],
        out3: &mut [u8],
    ) {
        self.0
            .hkdf(chaining_key, input_key_material, outputs, out1, out2, out3);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{NOISE_PATTERN, PROLOGUE};
    use snow::Builder;
    use snow::resolvers::BoxedCryptoResolver;

    fn default_dh() -> Box<dyn Dh> {
        DefaultResolver
            .resolve_dh(&DHChoice::Curve25519)
            .expect("curve25519")
    }

    fn default_cipher() -> Box<dyn Cipher> {
        DefaultResolver
            .resolve_cipher(&CipherChoice::ChaChaPoly)
            .expect("chachapoly")
    }

    fn default_hash() -> Box<dyn Hash> {
        DefaultResolver
            .resolve_hash(&HashChoice::Blake2s)
            .expect("blake2s")
    }

    #[test]
    fn wipe_dh() {
        let mut dh = WipedDh {
            inner: default_dh(),
            keyed: false,
        };
        dh.set(&[7; 32]);
        assert_eq!(dh.privkey(), &[7; 32]);
        dh.wipe();
        assert_eq!(dh.privkey(), &[0; 32]);
    }

    #[test]
    fn wipe_cipher() {
        let mut wiped = WipedCipher(default_cipher());
        wiped.set(&[7; 32]);
        wiped.wipe();
        let mut zero_key = default_cipher();
        zero_key.set(&[0; 32]);

        let (mut a, mut b) = ([0u8; 20], [0u8; 20]);
        wiped.encrypt(3, b"ad", b"four", &mut a);
        zero_key.encrypt(3, b"ad", b"four", &mut b);
        assert_eq!(a, b);
    }

    #[test]
    fn wipe_hash() {
        let mut wiped = WipedHash(default_hash());
        wiped.input(b"left over from a key derivation");
        wiped.wipe();
        let (mut a, mut b) = ([0u8; 32], [0u8; 32]);
        wiped.result(&mut a);
        default_hash().result(&mut b);
        assert_eq!(a, b);
    }

    fn message_one(resolver: BoxedCryptoResolver) -> Vec<u8> {
        let mut noise = Builder::with_resolver(NOISE_PATTERN.parse().expect("pattern"), resolver)
            .local_private_key(&[1; 32])
            .and_then(|b| b.remote_public_key(&[9; 32]))
            .and_then(|b| b.prologue(PROLOGUE))
            .and_then(|b| b.psk(2, &[3; 32]))
            .map(|b| b.fixed_ephemeral_key_for_testing_only(&[5; 32]))
            .and_then(|b| b.build_initiator())
            .expect("initiator");
        let mut message = vec![0u8; 200];
        let len = noise
            .write_message(b"payload", &mut message)
            .expect("write");
        message.truncate(len);
        message
    }

    #[test]
    fn wrapped_primitives_give_the_same_bytes() {
        assert_eq!(
            message_one(Box::new(WipingResolver)),
            message_one(Box::new(DefaultResolver))
        );
    }
}
