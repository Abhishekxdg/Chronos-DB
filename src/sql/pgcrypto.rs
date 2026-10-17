//! pgcrypto (Postgres 16's, version 1.3): digests and HMACs, crypt() and gen_salt(), random bytes, the raw
//! ciphers of encrypt() and decrypt(), and OpenPGP's symmetric encryption with its ASCII armor (`pgp`). Each
//! gives what Postgres gives for the same input (checked against it in tests/pgdiff.rs, and both ways for
//! what is random in tests/pgcrypto.rs). Like pg_trgm's, the functions are built in, in every database;
//! CREATE EXTENSION pgcrypto records the extension for pg_extension (see `EXTENSIONS`).

mod crypt;
mod pgp;

use super::funcs::Scalar;
use super::{Result, SqlError, Type, Value, err};
use aes::cipher::{BlockCipherDecrypt, BlockCipherEncrypt, KeyInit, array::Array};
use ring::rand::SecureRandom;

/// Where CREATE EXTENSION keeps an extension it records: rows `_sonos_extensions/<name>`, {"schema": ...,
/// "version": ...}. A row of the world, like a schema's, so it forks, merges, diffs and travels in time.
pub(super) const EXTENSIONS: &str = "_sonos_extensions";

/// The version CREATE EXTENSION pgcrypto installs, as Postgres 16 has it.
pub(super) const VERSION: &str = "1.3";

/// pgcrypto's oid in pg_extension (one of Chronos's own, below the oids of a world's objects).
pub(super) const EXTENSION_OID: i64 = 16310;

/// The key CREATE EXTENSION keeps extension `name` under.
pub(super) fn key(name: &str) -> String {
    format!("{EXTENSIONS}/{name}")
}

/// The extensions CREATE EXTENSION recorded in a world: name, schema, version.
pub(super) fn extensions(v: &super::View) -> Result<Vec<(String, String, String)>> {
    let mut out = vec![];
    for r in v.prefix(&format!("{EXTENSIONS}/"))? {
        let (k, raw) = r?;
        let j: serde_json::Value = serde_json::from_str(&raw).unwrap_or_default();
        let field = |f: &str| j[f].as_str().unwrap_or_default().to_string();
        out.push((k[EXTENSIONS.len() + 1..].to_string(), field("schema"), field("version")));
    }
    Ok(out)
}

/// The functions of pgcrypto, by name, and how many arguments each takes.
pub(super) fn lookup(name: &str) -> Option<(Scalar, std::ops::RangeInclusive<usize>)> {
    use Scalar::*;
    Some(match name {
        "digest" => (Digest, 2..=2),
        "hmac" => (Hmac, 3..=3),
        "crypt" => (Crypt, 2..=2),
        "gen_salt" => (GenSalt, 1..=2),
        "gen_random_bytes" => (GenRandomBytes, 1..=1),
        "encrypt" => (Encrypt, 3..=3),
        "decrypt" => (Decrypt, 3..=3),
        "encrypt_iv" => (EncryptIv, 4..=4),
        "decrypt_iv" => (DecryptIv, 4..=4),
        "pgp_sym_encrypt" => (PgpSymEncrypt, 2..=3),
        "pgp_sym_encrypt_bytea" => (PgpSymEncryptBytea, 2..=3),
        "pgp_sym_decrypt" => (PgpSymDecrypt, 2..=3),
        "pgp_sym_decrypt_bytea" => (PgpSymDecryptBytea, 2..=3),
        "pgp_key_id" => (PgpKeyId, 1..=1),
        "armor" => (Armor, 1..=3),
        "dearmor" => (Dearmor, 1..=1),
        _ => return None,
    })
}

/// Whether `f` is one of pgcrypto's.
pub(super) fn ours(f: Scalar) -> bool {
    use Scalar::*;
    matches!(
        f,
        Digest
            | Hmac
            | Crypt
            | GenSalt
            | GenRandomBytes
            | Encrypt
            | Decrypt
            | EncryptIv
            | DecryptIv
            | PgpSymEncrypt
            | PgpSymEncryptBytea
            | PgpSymDecrypt
            | PgpSymDecryptBytea
            | PgpKeyId
            | Armor
            | Dearmor
    )
}

/// The ones that give a new value each call (VOLATILE in Postgres): a column default of one is worked
/// out for each row.
pub(super) fn volatile(f: Scalar) -> bool {
    use Scalar::*;
    // (and uuid-ossp's time-based UUIDs)
    matches!(f, GenSalt | GenRandomBytes | PgpSymEncrypt | PgpSymEncryptBytea | UuidV1(_))
}

pub(super) fn result_type(f: Scalar) -> Type {
    use Scalar::*;
    match f {
        Crypt | GenSalt | PgpSymDecrypt | PgpKeyId | Armor => Type::Text,
        _ => Type::Bytea,
    }
}

/// The type of an argument where pgcrypto takes one.
#[derive(Clone, Copy, PartialEq)]
enum Arg {
    T,
    B,
    I,
    /// text[]
    A,
}

/// Each function's argument types, as pgcrypto declares them (several for the overloaded ones).
fn signatures(f: Scalar) -> &'static [&'static [Arg]] {
    use Arg::*;
    use Scalar::*;
    match f {
        Digest => &[&[T, T], &[B, T]],
        Hmac => &[&[T, T, T], &[B, B, T]],
        Crypt => &[&[T, T]],
        GenSalt => &[&[T], &[T, I]],
        GenRandomBytes => &[&[I]],
        Encrypt | Decrypt => &[&[B, B, T]],
        EncryptIv | DecryptIv => &[&[B, B, B, T]],
        PgpSymEncrypt => &[&[T, T], &[T, T, T]],
        PgpSymEncryptBytea | PgpSymDecrypt | PgpSymDecryptBytea => &[&[B, T], &[B, T, T]],
        PgpKeyId => &[&[B]],
        Armor => &[&[B], &[B, A, A]],
        Dearmor => &[&[T]],
        _ => &[],
    }
}

/// 42883 unless some signature of `f` takes arguments of these types (None: not known yet, a string
/// literal or a parameter, which takes the type it's given), with the conversions Postgres makes
/// without being asked: none between text and bytea, nor from a bigint to an integer.
pub(super) fn check_arguments(f: Scalar, name: &str, tys: &[Option<Type>]) -> Result<()> {
    let fits = |a: Arg, t: Type| match a {
        Arg::T => t == Type::Text,
        Arg::B => t == Type::Bytea,
        Arg::I => matches!(t, Type::SmallInt | Type::Int),
        Arg::A => t == Type::Array(25),
    };
    let ok = signatures(f)
        .iter()
        .any(|s| s.len() == tys.len() && s.iter().zip(tys).all(|(a, t)| t.is_none_or(|t| fits(*a, t))));
    if ok {
        return Ok(());
    }
    let shown: Vec<String> = tys.iter().map(|t| t.map_or("unknown".into(), |t| t.pg_name().into_owned())).collect();
    err("42883", format!("function {name}({}) does not exist", shown.join(", ")))
}

/// pgcrypto's own errors: 39000 (external routine invocation exception), with its words.
fn px(message: impl Into<String>) -> SqlError {
    SqlError { code: "39000", message: message.into(), info: None }
}

/// `n` bytes from the operating system's generator (never a seeded one: these are keys and salts).
fn random(n: usize) -> Result<Vec<u8>> {
    let mut b = vec![0; n];
    ring::rand::SystemRandom::new().fill(&mut b).map_err(|_| SqlError {
        code: "XX000",
        message: "could not generate a random number".into(),
        info: None,
    })?;
    Ok(b)
}

/// A digest's or cipher's name as pgcrypto looks it up: lowercase (ASCII), at most 63 bytes.
fn provider_name(v: &Value) -> String {
    let mut s = v.to_text().to_ascii_lowercase();
    let mut n = s.len().min(63);
    while !s.is_char_boundary(n) {
        n -= 1;
    }
    s.truncate(n);
    s
}

/// The bytes of a bytea argument: a bytea's, or a string read as one (a literal Postgres would read so).
fn bytes_of(v: &Value) -> Result<Vec<u8>> {
    super::bytes::of(v)
}

/// The bytes of a text argument: its UTF-8.
fn text_of(v: &Value) -> Vec<u8> {
    match v {
        Value::Text(s) => s.as_bytes().to_vec(),
        v => v.to_text().into_bytes(),
    }
}

/// A bytea argument where text is also taken: its bytes as a bytea, else as text.
fn either(v: &Value) -> Vec<u8> {
    match v {
        Value::Bytea(b) => b.clone(),
        v => text_of(v),
    }
}

fn int_of(v: &Value) -> Result<i64> {
    match super::coerce(v.clone(), Type::Int)? {
        Value::Int(n) => Ok(n),
        _ => err("22P02", "an integer was expected"),
    }
}

/// Text from decrypted bytes, which must be UTF-8 (as Postgres checks text it makes); the error shows the bad
/// sequence as Postgres's report_invalid_encoding does: as many bytes as its first says it has, up to the end.
fn utf8(b: Vec<u8>) -> Result<String> {
    String::from_utf8(b).map_err(|e| {
        let bad = &e.as_bytes()[e.utf8_error().valid_up_to()..];
        let n = match bad.first() {
            Some(c) if c & 0xe0 == 0xc0 => 2,
            Some(c) if c & 0xf0 == 0xe0 => 3,
            Some(c) if c & 0xf8 == 0xf0 => 4,
            _ => 1,
        };
        let shown: Vec<String> = bad.iter().take(n).map(|c| format!("0x{c:02x}")).collect();
        SqlError {
            code: "22021",
            message: format!("invalid byte sequence for encoding \"UTF8\": {}", shown.join(" ")),
            info: None,
        }
    })
}

/// A pgcrypto function called with `a` (none of them NULL): None for any other function.
pub(super) fn call(f: Scalar, a: &[Value]) -> Result<Option<Value>> {
    use Scalar::*;
    use Value::{Bytea, Text};
    Ok(Some(match f {
        Digest => {
            let name = provider_name(&a[1]);
            let Some(h) = Hash::named(name.trim_end_matches(' '))? else {
                return err("22023", format!("Cannot use \"{name}\": No such hash algorithm"));
            };
            Bytea(h.digest(&either(&a[0])))
        }
        Hmac => {
            let name = provider_name(&a[2]);
            let Some(h) = Hash::named(name.trim_end_matches(' '))? else {
                return err("22023", format!("Cannot use \"{name}\": No such hash algorithm"));
            };
            // hmac(bytea, bytea, text) when either is a bytea, else hmac(text, text, text)
            let (data, key) = match (&a[0], &a[1]) {
                (Bytea(_), _) | (_, Bytea(_)) => (bytes_of(&a[0])?, bytes_of(&a[1])?),
                _ => (text_of(&a[0]), text_of(&a[1])),
            };
            Bytea(h.hmac(&key, &data))
        }
        Crypt => Text(crypt::crypt(&text_of(&a[0]), &text_of(&a[1]))?),
        GenSalt => {
            let rounds = a.get(1).map(int_of).transpose()?.unwrap_or(0);
            let r: [u8; 16] = random(16)?.try_into().unwrap_or([0; 16]);
            match crypt::gen_salt(&a[0].to_text(), rounds, &r) {
                Some(s) => Text(s?),
                None => return err("22023", "gen_salt: Unknown salt algorithm"),
            }
        }
        GenRandomBytes => {
            let n = int_of(&a[0])?;
            if !(1..=1024).contains(&n) {
                return Err(px("Length not in range"));
            }
            Bytea(random(n as usize)?)
        }
        Encrypt | Decrypt | EncryptIv | DecryptIv => {
            let iv = if matches!(f, EncryptIv | DecryptIv) { Some(bytes_of(&a[2])?) } else { None };
            let combo = Combo::named(&provider_name(&a[a.len() - 1]))?;
            let (data, key) = (bytes_of(&a[0])?, bytes_of(&a[1])?);
            let name = super::func_name(&super::Func::Scalar(f));
            Bytea(
                match f {
                    Encrypt | EncryptIv => combo.encrypt(&data, &key, iv.as_deref()),
                    _ => combo.decrypt(&data, &key, iv.as_deref()),
                }
                .map_err(|e| px(format!("{name} error: {e}")))?,
            )
        }
        PgpSymEncrypt | PgpSymEncryptBytea => {
            let opts = pgp::Options::parse(&a.get(2).map(Value::to_text).unwrap_or_default())?;
            let text = f == PgpSymEncrypt;
            let data = if text { text_of(&a[0]) } else { bytes_of(&a[0])? };
            Bytea(pgp::encrypt(&data, &text_of(&a[1]), text, &opts)?)
        }
        PgpSymDecrypt | PgpSymDecryptBytea => {
            let opts = pgp::Options::parse(&a.get(2).map(Value::to_text).unwrap_or_default())?;
            let out = pgp::decrypt(&bytes_of(&a[0])?, &text_of(&a[1]), f == PgpSymDecrypt, &opts)?;
            if f == PgpSymDecrypt { Text(utf8(out)?) } else { Bytea(out) }
        }
        PgpKeyId => Text(pgp::key_id(&bytes_of(&a[0])?)?),
        Armor => {
            let headers = match a.len() {
                3 => headers(&a[1], &a[2])?,
                _ => vec![],
            };
            Text(pgp::armor(&bytes_of(&a[0])?, &headers))
        }
        Dearmor => Bytea(pgp::dearmor(&text_of(&a[0]))?),
        _ => return Ok(None),
    }))
}

/// armor's headers: two arrays of text, of one dimension and as many items, no NULLs, no newlines, and
/// ASCII only; a key holds no `": "`.
fn headers(keys: &Value, values: &Value) -> Result<Vec<(String, String)>> {
    let dims = |v: &Value| -> Result<(usize, Vec<Value>)> {
        let t = v.to_text();
        let inner = t.trim().strip_prefix('{').map(str::trim_start);
        if inner.is_some_and(|i| i.starts_with('{')) {
            return Ok((2, vec![]));
        }
        let items = super::array_values(v.clone())?.unwrap_or_default();
        Ok((usize::from(!items.is_empty()), items))
    };
    let ((kd, keys), (vd, values)) = (dims(keys)?, dims(values)?);
    if kd > 1 || kd != vd {
        return err("2202E", "wrong number of array subscripts");
    }
    if keys.len() != values.len() {
        return err("2202E", "mismatched array dimensions");
    }
    let mut out = vec![];
    for (k, v) in keys.iter().zip(&values) {
        let what = |v: &Value, kind: &str| -> Result<String> {
            if *v == Value::Null {
                return err("22004", format!("null value not allowed for header {kind}"));
            }
            let s = v.to_text();
            if !s.is_ascii() {
                return err("22023", format!("header {kind} must not contain non-ASCII characters"));
            }
            if kind == "key" && s.contains(": ") {
                return err("22023", "header key must not contain \": \"");
            }
            if s.contains('\n') {
                return err("22023", format!("header {kind} must not contain newlines"));
            }
            Ok(s)
        };
        out.push((what(k, "key")?, what(v, "value")?));
    }
    Ok(out)
}

// ---------------------------------------------------------------- digests

/// The digests pgcrypto's functions take here (OpenSSL's names for them, any case; pgcrypto asks OpenSSL).
#[derive(Clone, Copy, PartialEq, Debug)]
enum Hash {
    Md5,
    Sha1,
    Sha224,
    Sha256,
    Sha384,
    Sha512,
    Ripemd160,
}

impl Hash {
    /// The digest a lowercase name means (OpenSSL's names and aliases for it, and its OID; OpenSSL lets spaces
    /// follow it, so the caller drops them), None for one OpenSSL
    /// doesn't know; one it knows that isn't here is 0A000, and one its legacy provider has is the
    /// error Postgres on OpenSSL 3 gives.
    fn named(name: &str) -> Result<Option<Hash>> {
        use Hash::*;
        Ok(Some(match name {
            "md5" | "ssl3-md5" | "rsa-md5" | "md5withrsaencryption" | "1.2.840.113549.2.5" => Md5,
            "sha1" | "sha-1" | "ssl3-sha1" | "rsa-sha1" | "rsa-sha1-2" | "sha1withrsaencryption" | "1.3.14.3.2.26" => {
                Sha1
            }
            "sha224" | "sha-224" | "sha2-224" | "rsa-sha224" | "sha224withrsaencryption" | "2.16.840.1.101.3.4.2.4" => {
                Sha224
            }
            "sha256" | "sha-256" | "sha2-256" | "rsa-sha256" | "sha256withrsaencryption" | "2.16.840.1.101.3.4.2.1" => {
                Sha256
            }
            "sha384" | "sha-384" | "sha2-384" | "rsa-sha384" | "sha384withrsaencryption" | "2.16.840.1.101.3.4.2.2" => {
                Sha384
            }
            "sha512" | "sha-512" | "sha2-512" | "rsa-sha512" | "sha512withrsaencryption" | "2.16.840.1.101.3.4.2.3" => {
                Sha512
            }
            "ripemd160" | "ripemd" | "ripemd-160" | "rmd160" | "rsa-ripemd160" | "ripemd160withrsa"
            | "1.3.36.3.2.1" => Ripemd160,
            "md4" | "rsa-md4" | "md4withrsaencryption" | "mdc2" | "rsa-mdc2" | "mdc2withrsa" | "whirlpool" => {
                return err("22023", format!("Cannot use \"{name}\": Cipher cannot be initialized"));
            }
            n if n.starts_with("sha3-")
                || n.starts_with("rsa-sha3-")
                || n.starts_with("shake")
                || n.starts_with("keccak")
                || n.starts_with("blake2")
                || n.starts_with("sha512-")
                || n.starts_with("sha-512/")
                || n.starts_with("sha2-512/")
                || n.starts_with("rsa-sha512/")
                || n.starts_with("id-rsassa-pkcs1-v1_5-with-sha3-")
                || matches!(
                    n,
                    "sm3" | "rsa-sm3" | "sm3withrsaencryption" | "md5-sha1" | "sha256-192" | "sha-256/192"
                ) =>
            {
                return err(
                    "0A000",
                    format!(
                        "digest \"{name}\" isn't supported: md5, sha1, sha224, sha256, sha384, sha512 and ripemd160 are"
                    ),
                );
            }
            _ => return Ok(None),
        }))
    }

    fn digest(self, data: &[u8]) -> Vec<u8> {
        let mut m = Md::new(self);
        m.update(data);
        m.finish()
    }

    fn hmac(self, key: &[u8], data: &[u8]) -> Vec<u8> {
        use hmac::digest::{Digest, block_api::BlockSizeUser};
        use hmac::{KeyInit, Mac, SimpleHmac};
        fn mac<D: Digest + BlockSizeUser + Clone>(key: &[u8], data: &[u8]) -> Vec<u8> {
            let mut m = <SimpleHmac<D> as KeyInit>::new_from_slice(key).unwrap_or_else(|_| unreachable!());
            m.update(data);
            m.finalize().into_bytes().to_vec()
        }
        match self {
            Hash::Md5 => mac::<md5::Md5>(key, data),
            Hash::Sha1 => mac::<sha1::Sha1>(key, data),
            Hash::Sha224 => mac::<sha2::Sha224>(key, data),
            Hash::Sha256 => mac::<sha2::Sha256>(key, data),
            Hash::Sha384 => mac::<sha2::Sha384>(key, data),
            Hash::Sha512 => mac::<sha2::Sha512>(key, data),
            Hash::Ripemd160 => mac::<ripemd::Ripemd160>(key, data),
        }
    }
}

/// A digest being computed.
#[derive(Clone)]
enum Md {
    Md5(md5::Md5),
    Sha1(sha1::Sha1),
    Sha224(sha2::Sha224),
    Sha256(sha2::Sha256),
    Sha384(sha2::Sha384),
    Sha512(sha2::Sha512),
    Ripemd160(ripemd::Ripemd160),
}

impl Md {
    fn new(h: Hash) -> Md {
        use md5::Digest;
        match h {
            Hash::Md5 => Md::Md5(md5::Md5::new()),
            Hash::Sha1 => Md::Sha1(sha1::Sha1::new()),
            Hash::Sha224 => Md::Sha224(sha2::Sha224::new()),
            Hash::Sha256 => Md::Sha256(sha2::Sha256::new()),
            Hash::Sha384 => Md::Sha384(sha2::Sha384::new()),
            Hash::Sha512 => Md::Sha512(sha2::Sha512::new()),
            Hash::Ripemd160 => Md::Ripemd160(ripemd::Ripemd160::new()),
        }
    }

    fn update(&mut self, b: &[u8]) {
        use md5::Digest;
        match self {
            Md::Md5(h) => h.update(b),
            Md::Sha1(h) => h.update(b),
            Md::Sha224(h) => h.update(b),
            Md::Sha256(h) => h.update(b),
            Md::Sha384(h) => h.update(b),
            Md::Sha512(h) => h.update(b),
            Md::Ripemd160(h) => h.update(b),
        }
    }

    fn finish(self) -> Vec<u8> {
        use md5::Digest;
        match self {
            Md::Md5(h) => h.finalize().to_vec(),
            Md::Sha1(h) => h.finalize().to_vec(),
            Md::Sha224(h) => h.finalize().to_vec(),
            Md::Sha256(h) => h.finalize().to_vec(),
            Md::Sha384(h) => h.finalize().to_vec(),
            Md::Sha512(h) => h.finalize().to_vec(),
            Md::Ripemd160(h) => h.finalize().to_vec(),
        }
    }
}

// ---------------------------------------------------------------- block ciphers

/// A block cipher with its key set.
enum Block {
    Aes128(aes::Aes128),
    Aes192(aes::Aes192),
    Aes256(aes::Aes256),
    Bf(Box<blowfish::Blowfish>),
    Cast5(cast5::Cast5),
    Des(des::Des),
    Des3(des::TdesEde3),
}

impl Block {
    /// AES of the key's length: 16, 24 or 32 bytes.
    fn aes(key: &[u8]) -> Block {
        match key.len() {
            16 => Block::Aes128(aes::Aes128::new_from_slice(key).unwrap_or_else(|_| unreachable!())),
            24 => Block::Aes192(aes::Aes192::new_from_slice(key).unwrap_or_else(|_| unreachable!())),
            _ => Block::Aes256(aes::Aes256::new_from_slice(key).unwrap_or_else(|_| unreachable!())),
        }
    }

    /// Blowfish of any key: its schedule goes round the key's bytes, so a key shorter than the four bytes
    /// the crate takes is the same as itself repeated; no key at all is one zero byte, as OpenSSL reads it.
    fn bf(key: &[u8]) -> Block {
        let key: Vec<u8> = match key.len() {
            0 => vec![0; 4],
            n if n < 4 => key.iter().copied().cycle().take(n * 4usize.div_ceil(n)).collect(),
            _ => key[..key.len().min(56)].to_vec(),
        };
        Block::Bf(Box::new(blowfish::Blowfish::new_from_slice(&key).unwrap_or_else(|_| unreachable!())))
    }

    /// CAST5 of a key up to 16 bytes, zeros after a short one (as OpenSSL fills it; up to 10 bytes is the
    /// 12-round kind).
    fn cast5(key: &[u8]) -> Block {
        let mut k = key[..key.len().min(16)].to_vec();
        if k.len() < 5 {
            k.resize(5, 0);
        }
        Block::Cast5(cast5::Cast5::new_from_slice(&k).unwrap_or_else(|_| unreachable!()))
    }

    fn des(key: &[u8]) -> Block {
        let mut k = [0u8; 8];
        k[..key.len().min(8)].copy_from_slice(&key[..key.len().min(8)]);
        Block::Des(des::Des::new(&Array::from(k)))
    }

    fn des3(key: &[u8]) -> Block {
        let mut k = [0u8; 24];
        k[..key.len().min(24)].copy_from_slice(&key[..key.len().min(24)]);
        Block::Des3(des::TdesEde3::new(&Array::from(k)))
    }

    fn size(&self) -> usize {
        match self {
            Block::Aes128(_) | Block::Aes192(_) | Block::Aes256(_) => 16,
            _ => 8,
        }
    }

    /// Encrypts one block, `b` (the cipher's size).
    fn encrypt(&self, b: &mut [u8]) {
        fn one<C: BlockCipherEncrypt>(c: &C, b: &mut [u8]) {
            c.encrypt_block(b.try_into().unwrap_or_else(|_| unreachable!("a whole block")));
        }
        match self {
            Block::Aes128(c) => one(c, b),
            Block::Aes192(c) => one(c, b),
            Block::Aes256(c) => one(c, b),
            Block::Bf(c) => one(&**c, b),
            Block::Cast5(c) => one(c, b),
            Block::Des(c) => one(c, b),
            Block::Des3(c) => one(c, b),
        }
    }

    /// Decrypts one block, `b` (the cipher's size).
    fn decrypt(&self, b: &mut [u8]) {
        fn one<C: BlockCipherDecrypt>(c: &C, b: &mut [u8]) {
            c.decrypt_block(b.try_into().unwrap_or_else(|_| unreachable!("a whole block")));
        }
        match self {
            Block::Aes128(c) => one(c, b),
            Block::Aes192(c) => one(c, b),
            Block::Aes256(c) => one(c, b),
            Block::Bf(c) => one(&**c, b),
            Block::Cast5(c) => one(c, b),
            Block::Des(c) => one(c, b),
            Block::Des3(c) => one(c, b),
        }
    }
}

// ---------------------------------------------------------------- encrypt and decrypt

#[derive(Clone, Copy, PartialEq)]
enum Algo {
    Aes,
    Bf,
    Des,
    Des3,
    Cast5,
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Cbc,
    Ecb,
    Cfb,
}

/// encrypt's `type`: a cipher, its mode, and whether it pads (PKCS#7).
struct Combo {
    algo: Algo,
    mode: Mode,
    pad: bool,
}

impl Combo {
    /// `cipher[-mode][/pad:pkcs|/pad:none]` as pgcrypto reads it (lowercase: see `provider_name`), its options
    /// first, then the cipher.
    fn named(kind: &str) -> Result<Combo> {
        let cannot = |why: &str| err("22023", format!("Cannot use \"{kind}\": {why}"));
        let mut parts = kind.split('/');
        let name = parts.next().unwrap_or_default();
        let mut pad = None;
        for opt in parts.filter(|p| !p.is_empty()) {
            match opt.split_once(':') {
                Some(("pad", p)) => pad = Some(p),
                Some(_) => return cannot("Unknown option"),
                None => return cannot("Badly formatted type"),
            }
        }
        let resolved = match name {
            "bf" | "blowfish" | "blowfish-cbc" => "bf-cbc",
            "blowfish-ecb" => "bf-ecb",
            "blowfish-cfb" => "bf-cfb",
            "des" => "des-cbc",
            "3des" | "3des-cbc" => "des3-cbc",
            "3des-ecb" => "des3-ecb",
            "cast5" => "cast5-cbc",
            "aes" | "rijndael" | "rijndael-cbc" => "aes-cbc",
            "rijndael-ecb" => "aes-ecb",
            n => n,
        };
        let (algo, mode) = match resolved {
            "bf-cbc" => (Algo::Bf, Mode::Cbc),
            "bf-ecb" => (Algo::Bf, Mode::Ecb),
            "bf-cfb" => (Algo::Bf, Mode::Cfb),
            "des-ecb" => (Algo::Des, Mode::Ecb),
            "des-cbc" => (Algo::Des, Mode::Cbc),
            "des3-ecb" => (Algo::Des3, Mode::Ecb),
            "des3-cbc" => (Algo::Des3, Mode::Cbc),
            "cast5-ecb" => (Algo::Cast5, Mode::Ecb),
            "cast5-cbc" => (Algo::Cast5, Mode::Cbc),
            "aes-ecb" => (Algo::Aes, Mode::Ecb),
            "aes-cbc" => (Algo::Aes, Mode::Cbc),
            _ => return cannot("No such cipher algorithm"),
        };
        let pad = match pad {
            None | Some("pkcs") => true,
            Some("none") => false,
            Some(_) => return cannot("No such cipher algorithm"),
        };
        Ok(Combo { algo, mode, pad })
    }

    /// The cipher with `key` as pgcrypto sets it: cut to the cipher's longest, AES's length chosen by it
    /// with zeros after it.
    fn block(&self, key: &[u8]) -> Block {
        match self.algo {
            Algo::Aes => {
                let k = &key[..key.len().min(32)];
                let size = match k.len() {
                    0..=16 => 16,
                    17..=24 => 24,
                    _ => 32,
                };
                let mut full = k.to_vec();
                full.resize(size, 0);
                Block::aes(&full)
            }
            Algo::Bf => Block::bf(key),
            Algo::Des => Block::des(key),
            Algo::Des3 => Block::des3(key),
            Algo::Cast5 => Block::cast5(key),
        }
    }

    /// The IV: zeros, with as much of `iv` as fits over them.
    fn iv(bs: usize, iv: Option<&[u8]>) -> Vec<u8> {
        let mut out = vec![0; bs];
        if let Some(iv) = iv {
            let n = iv.len().min(bs);
            out[..n].copy_from_slice(&iv[..n]);
        }
        out
    }

    fn encrypt(&self, data: &[u8], key: &[u8], iv: Option<&[u8]>) -> std::result::Result<Vec<u8>, &'static str> {
        let c = self.block(key);
        let bs = c.size();
        let mut prev = Self::iv(bs, iv);
        if self.mode == Mode::Cfb {
            return Ok(cfb64(&c, &prev, data, true));
        }
        let mut buf = data.to_vec();
        if self.pad {
            let n = bs - buf.len() % bs;
            buf.extend(std::iter::repeat_n(n as u8, n));
        } else if !buf.len().is_multiple_of(bs) {
            return Err("Encryption failed");
        }
        for block in buf.chunks_mut(bs) {
            if self.mode == Mode::Cbc {
                block.iter_mut().zip(&prev).for_each(|(b, p)| *b ^= p);
            }
            c.encrypt(block);
            if self.mode == Mode::Cbc {
                prev.copy_from_slice(block);
            }
        }
        Ok(buf)
    }

    fn decrypt(&self, data: &[u8], key: &[u8], iv: Option<&[u8]>) -> std::result::Result<Vec<u8>, &'static str> {
        let c = self.block(key);
        let bs = c.size();
        let mut prev = Self::iv(bs, iv);
        if self.mode == Mode::Cfb {
            return Ok(cfb64(&c, &prev, data, false));
        }
        if !data.len().is_multiple_of(bs) || (self.pad && data.is_empty()) {
            return Err("Decryption failed");
        }
        let mut buf = data.to_vec();
        for block in buf.chunks_mut(bs) {
            let this = block.to_vec();
            c.decrypt(block);
            if self.mode == Mode::Cbc {
                block.iter_mut().zip(&prev).for_each(|(b, p)| *b ^= p);
                prev = this;
            }
        }
        if self.pad {
            let n = usize::from(buf[buf.len() - 1]);
            if n == 0 || n > bs || !buf[buf.len() - n..].iter().all(|&b| usize::from(b) == n) {
                return Err("Decryption failed");
            }
            buf.truncate(buf.len() - n);
        }
        Ok(buf)
    }
}

/// CFB over whole blocks (OpenSSL's cfb64), a stream: no padding, any length.
fn cfb64(c: &Block, iv: &[u8], data: &[u8], encrypt: bool) -> Vec<u8> {
    let bs = c.size();
    let mut reg = iv.to_vec();
    let mut out = Vec::with_capacity(data.len());
    for chunk in data.chunks(bs) {
        let mut ks = reg.clone();
        c.encrypt(&mut ks);
        for (i, &x) in chunk.iter().enumerate() {
            let y = x ^ ks[i];
            out.push(y);
            reg[i] = if encrypt { y } else { x };
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FIPS 197's AES vectors and RFC 2144's CAST5, through the block type.
    #[test]
    fn block_ciphers() {
        let hex = |s: &str| super::super::bytes::from_hex(s).unwrap();
        let mut b = hex("00112233445566778899aabbccddeeff");
        Block::aes(&hex("000102030405060708090a0b0c0d0e0f")).encrypt(&mut b);
        assert_eq!(b, hex("69c4e0d86a7b0430d8cdb78070b4c55a"));
        let mut b = hex("0123456789abcdef");
        Block::cast5(&hex("0123456712345678234567893456789a")).encrypt(&mut b);
        assert_eq!(b, hex("238b4fe5847e44b2"));
        Block::cast5(&hex("0123456712345678234567893456789a")).decrypt(&mut b);
        assert_eq!(b, hex("0123456789abcdef"));
        // a short Blowfish key is itself repeated
        let (mut x, mut y) = (hex("0011223344556677"), hex("0011223344556677"));
        Block::bf(b"ab").encrypt(&mut x);
        Block::bf(b"abab").encrypt(&mut y);
        assert_eq!(x, y);
    }
}
