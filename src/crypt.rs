//! Encryption at rest: with a key, everything the database writes that holds data is encrypted
//! with ChaCha20-Poly1305 (authenticated, so tampering is found, not read as data):
//!   - log records, each bound to its segment and offset (a random nonce stored with each);
//!   - the manifest and history manifests (a random nonce);
//!   - pages, deterministically: a page's name is a keyed hash of its bytes (so names reveal
//!     nothing), and its nonce comes from that name, so equal pages still dedupe.
//!
//! Backups and cloud uploads copy these bytes, so they are encrypted too.
//!
//! The 32-byte key is never stored. The folder keeps `encryption`, a check value derived from the
//! key, so opening with a wrong key or none fails at once with a clear error.
//! ponytail: the key is chosen when a database is created; no rotation and no encrypting an
//! existing folder in place (copy its rows into a new encrypted database).

use crate::pages::Hash;
use ring::aead::{Aad, CHACHA20_POLY1305, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use ring::rand::{SecureRandom, SystemRandom};
use std::io;
use std::path::Path;

const MARKER: &str = "encryption";
const MARKER_MAGIC: &[u8] = b"SONOSKEY1";
/// Encrypted whole files (manifests) start with this, so either kind is recognised on sight.
pub(crate) const FILE_MAGIC: &[u8] = b"SONOSENC";
pub(crate) const TAG: usize = 16;

pub struct Crypt {
    aead: LessSafeKey,
    names: [u8; 32],
    check: [u8; 32],
}

fn bad(what: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("{what} doesn't decrypt: corrupt, tampered with, or another key's"),
    )
}

impl Crypt {
    pub fn new(key: &[u8; 32]) -> Crypt {
        let sub = |context: &str| blake3::derive_key(context, key);
        let aead = LessSafeKey::new(UnboundKey::new(&CHACHA20_POLY1305, &sub("sonos 2026-09 encryption")).unwrap());
        Crypt { aead, names: sub("sonos 2026-09 page names"), check: sub("sonos 2026-09 key check") }
    }

    /// A page's name: a keyed hash of its bytes.
    pub fn name(&self, bytes: &[u8]) -> Hash {
        blake3::keyed_hash(&self.names, bytes).into()
    }

    fn seal_with(&self, nonce: [u8; NONCE_LEN], aad: &[u8], data: &[u8]) -> Vec<u8> {
        let mut out = data.to_vec();
        self.aead.seal_in_place_append_tag(Nonce::assume_unique_for_key(nonce), Aad::from(aad), &mut out).unwrap();
        out
    }

    fn open_with(&self, nonce: [u8; NONCE_LEN], aad: &[u8], data: &[u8], what: &str) -> io::Result<Vec<u8>> {
        let mut buf = data.to_vec();
        let n = self
            .aead
            .open_in_place(Nonce::assume_unique_for_key(nonce), Aad::from(aad), &mut buf)
            .map_err(|_| bad(what))?
            .len();
        buf.truncate(n);
        Ok(buf)
    }

    /// A page's stored bytes; the nonce is its name's (a name is only ever for the same bytes).
    pub fn seal_page(&self, name: &Hash, data: &[u8]) -> Vec<u8> {
        self.seal_with(name[..NONCE_LEN].try_into().unwrap(), &[], data)
    }

    pub fn open_page(&self, name: &Hash, data: &[u8]) -> io::Result<Vec<u8>> {
        self.open_with(name[..NONCE_LEN].try_into().unwrap(), &[], data, "a page")
    }

    /// `data` sealed under a fresh random nonce, which goes first; `aad` must match to open it.
    pub fn seal(&self, aad: &[u8], data: &[u8]) -> Vec<u8> {
        let mut nonce = [0; NONCE_LEN];
        SystemRandom::new().fill(&mut nonce).expect("the system's random numbers");
        let mut out = nonce.to_vec();
        out.extend(self.seal_with(nonce, aad, data));
        out
    }

    pub fn open(&self, aad: &[u8], data: &[u8], what: &str) -> io::Result<Vec<u8>> {
        if data.len() < NONCE_LEN + TAG {
            return Err(bad(what));
        }
        self.open_with(data[..NONCE_LEN].try_into().unwrap(), aad, &data[NONCE_LEN..], what)
    }

    /// A whole file (a manifest): FILE_MAGIC, then sealed.
    pub fn seal_file(&self, data: &[u8]) -> Vec<u8> {
        let mut out = FILE_MAGIC.to_vec();
        out.extend(self.seal(FILE_MAGIC, data));
        out
    }
}

/// A new random key, as 64 hex digits (for `chronos keygen`).
pub fn new_key() -> String {
    let mut k = [0u8; 32];
    SystemRandom::new().fill(&mut k).expect("the system's random numbers");
    k.iter().map(|b| format!("{b:02x}")).collect()
}

/// A key from 64 hex digits.
pub fn parse_key(hex: &str) -> io::Result<[u8; 32]> {
    let hex = hex.trim();
    let bytes: Option<Vec<u8>> =
        (0..hex.len()).step_by(2).map(|i| hex.get(i..i + 2).and_then(|b| u8::from_str_radix(b, 16).ok())).collect();
    bytes
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| io::Error::other("a key is 64 hex digits (32 bytes); make one with: chronos keygen"))
}

/// A whole file's bytes as stored: decrypted if encrypted. Refuses a mismatch either way.
pub(crate) fn open_file(c: Option<&Crypt>, bytes: Vec<u8>, what: &str) -> io::Result<Vec<u8>> {
    match (c, bytes.strip_prefix(FILE_MAGIC)) {
        (Some(c), Some(sealed)) => c.open(FILE_MAGIC, sealed, what),
        (None, None) => Ok(bytes),
        (Some(_), None) => Err(io::Error::other(format!("{what} isn't encrypted: open this database without a key"))),
        (None, Some(_)) => {
            Err(io::Error::other(format!("{what} is encrypted: open this database with its key (CHRONOS_KEY)")))
        }
    }
}

/// Checks `key` against the folder's `encryption` file, writing one for a new database. A folder
/// that was created without a key can't be opened with one, nor the other way round.
pub(crate) fn setup(dir: &Path, key: Option<&[u8; 32]>, fresh: bool) -> io::Result<Option<Crypt>> {
    let path = dir.join(MARKER);
    let stored = match std::fs::read(&path) {
        Ok(b) => Some(b),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    let Some(key) = key else {
        return match stored {
            Some(_) => {
                Err(io::Error::other(format!("{} is encrypted: open it with its key (CHRONOS_KEY)", dir.display())))
            }
            None => Ok(None),
        };
    };
    let c = Crypt::new(key);
    let mut want = MARKER_MAGIC.to_vec();
    want.extend(c.check);
    match stored {
        Some(b) if b == want => Ok(Some(c)),
        Some(_) => Err(io::Error::other(format!("{}: wrong key", dir.display()))),
        None if !fresh => Err(io::Error::other(format!(
            "{} was created without encryption; a key can only be given to a new database",
            dir.display()
        ))),
        None => {
            let tmp = dir.join("encryption.tmp");
            std::fs::write(&tmp, &want)?;
            std::fs::File::open(&tmp)?.sync_all()?;
            std::fs::rename(&tmp, &path)?;
            std::fs::File::open(dir)?.sync_all()?;
            Ok(Some(c))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_refuses_tampering() {
        let (a, b) = (Crypt::new(&[1; 32]), Crypt::new(&[2; 32]));
        let sealed = a.seal(b"here", b"secret rows");
        assert!(!sealed.windows(6).any(|w| w == b"secret"));
        assert_eq!(a.open(b"here", &sealed, "x").unwrap(), b"secret rows");
        assert!(a.open(b"there", &sealed, "x").is_err(), "bound to its place");
        assert!(b.open(b"here", &sealed, "x").is_err(), "another key");
        let mut t = sealed.clone();
        t[20] ^= 1;
        assert!(a.open(b"here", &t, "x").is_err(), "tampered");
        let name = a.name(b"page");
        assert_ne!(name, b.name(b"page"));
        assert_eq!(a.open_page(&name, &a.seal_page(&name, b"page")).unwrap(), b"page");
        assert_eq!(parse_key(&new_key()).unwrap().len(), 32);
        assert!(parse_key("zz").is_err());
    }
}
