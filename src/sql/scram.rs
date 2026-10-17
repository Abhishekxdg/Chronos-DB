//! Passwords as Postgres keeps them (`pg_authid.rolpassword`): never the password itself, but a
//! SCRAM-SHA-256 verifier (RFC 5802, 7677), `SCRAM-SHA-256$<iterations>:<salt>$<StoredKey>:<ServerKey>`
//! in base64, as Postgres 16 writes it: `scram_iterations` (4096) iterations of PBKDF2-HMAC-SHA-256
//! over a random 16-byte salt, of the password after SASLprep (`saslprep`). A client logging in
//! proves it knows the password without sending it; the verifier is what the server checks that
//! proof against (`Login`, Postgres's server side of the exchange). A password given already
//! hashed (a verifier, or `md5` and 32 hex digits) is kept as given; a role whose password is an
//! md5 hash logs in with Postgres's MD5 challenge instead. An agent's token has a verifier too
//! (see `chronos::agents`). A login that can't succeed is checked against a mock verifier whose
//! salt is made from the name and a server secret (`mock_salt`), as Postgres's is.

use super::{SqlError, Violation};
use ring::{digest, hmac, rand::SecureRandom};
use std::borrow::Cow;

/// Postgres's `scram_iterations` default.
pub const ITERATIONS: u32 = 4096;
const SALT_LEN: usize = 16;

/// A SCRAM-SHA-256 verifier's parts.
#[derive(Clone, Debug, PartialEq)]
pub struct Verifier {
    pub iterations: u32,
    pub salt: Vec<u8>,
    pub stored_key: Vec<u8>,
    pub server_key: Vec<u8>,
}

/// What a role's password is kept as: as given if it is hashed already, else a SCRAM-SHA-256
/// verifier with `iterations` (`scram_iterations`), or with `password_encryption` set to md5,
/// Postgres's md5 hash (`md5` and the hex md5 of the password followed by the role's name).
pub(crate) fn kept(password: &str, role: &str, encryption: &str, iterations: u32) -> Result<String, SqlError> {
    if parse(password).is_some() || is_md5(password) {
        return Ok(password.to_string());
    }
    if encryption.eq_ignore_ascii_case("md5") {
        return Ok(md5_hash(password, role));
    }
    fresh(password, iterations)
}

/// A new verifier of `password`: a random salt, `iterations` iterations. Inside a statement it stops
/// (57014) when the statement is canceled or past its statement_timeout, as Postgres checks for
/// interrupts while it hashes: `scram_iterations` may be set to two billion.
pub fn fresh(password: &str, iterations: u32) -> Result<String, SqlError> {
    let mut salt = [0u8; SALT_LEN];
    ring::rand::SystemRandom::new()
        .fill(&mut salt)
        .map_err(|_| refusal("XX000", "could not generate a random salt", None))?;
    made(password, &salt, iterations, &mut super::stopped)
}

/// The salt of the mock verifier a login as `name` that can't succeed is checked against, as
/// Postgres's scram_mock_salt makes it: the SHA-256 of the name and a server secret, cut to 16
/// bytes. The same name always gets the same salt, and a client can't tell it from a real one.
pub fn mock_salt(name: &str, secret: &[u8]) -> Vec<u8> {
    let mut h = digest::Context::new(&digest::SHA256);
    h.update(name.as_bytes());
    h.update(secret);
    h.finish().as_ref()[..SALT_LEN].to_vec()
}

/// A password as SCRAM hashes it: after SASLprep (RFC 4013) as Postgres's pg_saslprep runs it, or
/// as given where SASLprep prohibits it (libpq does the same, so both ends agree). ASCII is as given,
/// control characters too. Otherwise: spaces outside ASCII become spaces and characters "commonly
/// mapped to nothing" go; then the result is NFKC-normalized, unless it is empty, has a prohibited
/// or (in Unicode 3.2) unassigned character, or mixes right-to-left characters with left-to-right
/// ones or doesn't start and end with one (checked before NFKC, as Postgres checks it).
pub fn saslprep(password: &str) -> Cow<'_, str> {
    use stringprep::tables as t;
    if password.is_ascii() {
        return Cow::Borrowed(password);
    }
    let mapped: Vec<char> = password
        .chars()
        .map(|c| if t::non_ascii_space_character(c) { ' ' } else { c })
        .filter(|&c| !t::commonly_mapped_to_nothing(c))
        .collect();
    let prohibited = |&c: &char| {
        t::non_ascii_space_character(c)
            || t::ascii_control_character(c)
            || t::non_ascii_control_character(c)
            || t::private_use(c)
            || t::non_character_code_point(c)
            || t::inappropriate_for_plain_text(c)
            || t::inappropriate_for_canonical_representation(c)
            || t::change_display_properties_or_deprecated(c)
            || t::tagging_character(c)
            || t::unassigned_code_point(c)
    };
    let rtl = |c: Option<&char>| c.is_some_and(|&c| t::bidi_r_or_al(c));
    let bidi_broken = mapped.iter().any(|c| rtl(Some(c)))
        && (mapped.iter().any(|&c| t::bidi_l(c)) || !rtl(mapped.first()) || !rtl(mapped.last()));
    if mapped.is_empty() || mapped.iter().any(prohibited) || bidi_broken {
        return Cow::Borrowed(password);
    }
    let mapped: String = mapped.into_iter().collect();
    Cow::Owned(icu_normalizer::ComposingNormalizerBorrowed::new_nfkc().normalize(&mapped).into_owned())
}

/// Whether `s` is a password hashed by Postgres's md5 method: `md5` and 32 lowercase hex digits
/// (with an uppercase one, Postgres takes it for a password given in the clear).
pub fn is_md5(s: &str) -> bool {
    s.len() == 35 && s.starts_with("md5") && s[3..].bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Postgres's md5 hash of a role's password: `md5` and the hex md5 of the password and the name.
pub fn md5_hash(password: &str, role: &str) -> String {
    format!("md5{}", super::funcs::md5(format!("{password}{role}").as_bytes()))
}

/// What a client answers the MD5 challenge with: `md5` and the hex md5 of the role's md5 hash
/// (without its `md5`) and the server's 4-byte salt.
pub fn md5_salted(hash: &str, salt: &[u8]) -> String {
    let hex = hash.strip_prefix("md5").unwrap_or(hash);
    format!("md5{}", super::funcs::md5(&[hex.as_bytes(), salt].concat()))
}

/// The verifier of `password` with this salt and iteration count.
pub fn verifier(password: &str, salt: &[u8], iterations: u32) -> String {
    made(password, salt, iterations, &mut || Ok(())).expect("nothing stops it")
}

/// `verifier`, asking `stop` every 4096 iterations whether to give up.
fn made(
    password: &str,
    salt: &[u8],
    iterations: u32,
    stop: &mut dyn FnMut() -> Result<(), SqlError>,
) -> Result<String, SqlError> {
    let salted = pbkdf2(saslprep(password).as_bytes(), salt, iterations, stop)?;
    let client_key = hmac256(&salted, b"Client Key");
    let stored_key = digest::digest(&digest::SHA256, &client_key);
    let server_key = hmac256(&salted, b"Server Key");
    Ok(format!("SCRAM-SHA-256${iterations}:{}${}:{}", b64(salt), b64(stored_key.as_ref()), b64(&server_key)))
}

/// SaltedPassword: PBKDF2 with HMAC-SHA-256, of the password after SASLprep.
pub fn salted_password(password: &str, salt: &[u8], iterations: u32) -> Vec<u8> {
    pbkdf2(saslprep(password).as_bytes(), salt, iterations, &mut || Ok(())).expect("nothing stops it")
}

/// PBKDF2-HMAC-SHA-256 for one block (SCRAM's 32 bytes), as RFC 8018 has it: U1 is the HMAC of the
/// salt and block number 1, each next U the HMAC of the last, and the result all of them XORed.
/// `stop` is asked every 4096 iterations.
fn pbkdf2(
    password: &[u8],
    salt: &[u8],
    iterations: u32,
    stop: &mut dyn FnMut() -> Result<(), SqlError>,
) -> Result<Vec<u8>, SqlError> {
    let key = hmac::Key::new(hmac::HMAC_SHA256, password);
    let mut u = [0u8; digest::SHA256_OUTPUT_LEN];
    u.copy_from_slice(hmac::sign(&key, &[salt, &1u32.to_be_bytes()].concat()).as_ref());
    let mut out = u;
    for i in 2..=iterations.max(1) {
        if i.is_multiple_of(4096) {
            stop()?;
        }
        let next = hmac::sign(&key, &u);
        u.copy_from_slice(next.as_ref());
        out.iter_mut().zip(&u).for_each(|(o, u)| *o ^= u);
    }
    Ok(out.to_vec())
}

/// Whether `password`, given in the clear, is the one kept for `role` (a SCRAM-SHA-256 verifier or an
/// md5 hash), as Postgres's plain_crypt_verify checks a password sent in the clear: a verifier is
/// made again from its salt and iteration count and both its keys compared; an md5 hash is made
/// again from the password and the role's name. Compared in a time that doesn't tell how much matched.
/// A kept password of neither kind matches nothing.
pub fn plain_matches(kept: &str, password: &str, role: &str) -> bool {
    if is_md5(kept) {
        return same(md5_hash(password, role).as_bytes(), kept.as_bytes());
    }
    let Some(v) = parse(kept) else { return false };
    let salted = salted_password(password, &v.salt, v.iterations);
    let stored_key = digest::digest(&digest::SHA256, &hmac256(&salted, b"Client Key"));
    let server_key = hmac256(&salted, b"Server Key");
    // (both compared, whatever the first gives)
    same(stored_key.as_ref(), &v.stored_key) & same(&server_key, &v.server_key)
}

pub(crate) fn hmac256(key: &[u8], data: &[u8]) -> Vec<u8> {
    hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, key), data).as_ref().to_vec()
}

/// A verifier's parts, if `s` is one.
pub fn parse(s: &str) -> Option<Verifier> {
    let rest = s.strip_prefix("SCRAM-SHA-256$")?;
    let (head, keys) = rest.split_once('$')?;
    let (iterations, salt) = head.split_once(':')?;
    let (stored, server) = keys.split_once(':')?;
    let v = Verifier {
        iterations: iterations.parse().ok().filter(|n| *n > 0)?,
        salt: unb64(salt)?,
        stored_key: unb64(stored)?,
        server_key: unb64(server)?,
    };
    (v.stored_key.len() == digest::SHA256_OUTPUT_LEN && v.server_key.len() == digest::SHA256_OUTPUT_LEN).then_some(v)
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub(crate) fn b64(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = u32::from(c[0]) << 16 | u32::from(*c.get(1).unwrap_or(&0)) << 8 | u32::from(*c.get(2).unwrap_or(&0));
        for i in 0..4 {
            out.push(if i <= c.len() { B64[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
        }
    }
    out
}

/// Strict base64: whole groups of four, `=` only as padding at the end.
pub(crate) fn unb64(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(4) {
        return None;
    }
    let body = s.trim_end_matches('=');
    if s.len() - body.len() > 2 {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    for c in body.as_bytes().chunks(4) {
        let mut n = 0u32;
        for (i, b) in c.iter().enumerate() {
            let v = B64.iter().position(|x| x == b)? as u32;
            n |= v << (18 - 6 * i);
        }
        out.extend(&n.to_be_bytes()[1..c.len()]);
    }
    Some(out)
}

// ---------------------------------------------------------------- logging in: the server's side

/// The SASL mechanisms a login is offered (AuthenticationSASL's list, its terminator included):
/// SCRAM-SHA-256-PLUS first when the connection can bind to its TLS certificate, as Postgres lists them.
pub fn mechanisms(plus: bool) -> Vec<u8> {
    let mut b = vec![];
    if plus {
        b.extend(b"SCRAM-SHA-256-PLUS\0");
    }
    b.extend(b"SCRAM-SHA-256\0\0");
    b
}

/// Postgres's server nonce: 18 random bytes, in base64.
const NONCE_LEN: usize = 18;

/// One SCRAM-SHA-256 login, the server's side, as Postgres runs it (RFC 5802, 7677): the client's
/// first message names its nonce; the server answers with its own nonce after it, the verifier's
/// salt and iteration count; the client's final message proves it knows the password (its
/// ClientProof, checked against StoredKey), and the server proves it knows the verifier back
/// (ServerSignature). With SCRAM-SHA-256-PLUS the client also binds the exchange to the TLS
/// certificate it saw (tls-server-end-point), so a server in the middle can't relay it.
/// Every refusal is Postgres's: 08P01 for a malformed message, 28P01 for a wrong password.
pub struct Login {
    /// what the proof may match: verifiers with one salt and iteration count (the first that matches wins)
    secrets: Vec<Verifier>,
    /// refused however it ends (Postgres's mock authentication: a role that can't log in with a
    /// password looks like a wrong password, and takes as long)
    doomed: bool,
    /// the certificate hash the client must bind to, when it chose SCRAM-SHA-256-PLUS
    binding: Option<Vec<u8>>,
    /// SCRAM-SHA-256-PLUS was offered: a client that says it could bind but thinks the server can't is refused
    offered_plus: bool,
    server_nonce: String,
    /// the GS2 channel-binding flag the client sent (`n`, `y` or `p`)
    flag: u8,
    client_first_bare: Vec<u8>,
    client_nonce: Vec<u8>,
    server_first: Vec<u8>,
}

fn malformed(detail: impl Into<String>) -> SqlError {
    refusal("08P01", "malformed SCRAM message", Some(detail.into()))
}

fn refusal(code: &'static str, message: impl Into<String>, detail: Option<String>) -> SqlError {
    let info = detail.map(|d| Box::new(Violation { detail: Some(d), ..Violation::default() }));
    SqlError { code, message: message.into(), info }
}

/// A byte as Postgres's messages show it: quoted if printable, else in hex.
fn shown(c: Option<u8>) -> String {
    match c.unwrap_or(0) {
        c @ 0x21..=0x7e => format!("'{}'", c as char),
        c => format!("0x{c:02x}"),
    }
}

/// Reads a SCRAM message's `name=value` attributes, as Postgres's read_attr_value and read_any_attr do.
struct Attrs<'a>(&'a [u8], usize);

impl<'a> Attrs<'a> {
    fn peek(&self) -> Option<u8> {
        self.0.get(self.1).copied()
    }
    fn value(&mut self, attr: u8) -> Result<&'a [u8], SqlError> {
        if self.peek() != Some(b'=') {
            return Err(malformed(format!("Expected character \"=\" for attribute \"{}\".", attr as char)));
        }
        let start = self.1 + 1;
        let end = self.0[start..].iter().position(|&b| b == b',').map_or(self.0.len(), |n| start + n);
        self.1 = (end + 1).min(self.0.len());
        Ok(&self.0[start..end])
    }
    /// The value of attribute `attr`, which must come next.
    fn attr(&mut self, attr: u8) -> Result<&'a [u8], SqlError> {
        if self.peek() != Some(attr) {
            return Err(malformed(format!(
                "Expected attribute \"{}\" but found \"{}\".",
                attr as char,
                shown(self.peek())
            )));
        }
        self.1 += 1;
        self.value(attr)
    }
    /// Whatever attribute comes next: (its letter, its value).
    fn any(&mut self) -> Result<(u8, &'a [u8]), SqlError> {
        match self.peek() {
            None => Err(malformed("Attribute expected, but found end of string.")),
            Some(a) if a.is_ascii_alphabetic() => {
                self.1 += 1;
                Ok((a, self.value(a)?))
            }
            c => Err(malformed(format!("Attribute expected, but found invalid character \"{}\".", shown(c)))),
        }
    }
}

/// A SASL message must be text: not empty, no NUL inside (Postgres reads it as a C string).
fn text(msg: &[u8]) -> Result<&[u8], SqlError> {
    if msg.is_empty() {
        return Err(malformed("The message is empty."));
    }
    if msg.contains(&0) {
        return Err(malformed("Message length does not match input length."));
    }
    Ok(msg)
}

/// Whether two byte strings are equal, in a time that doesn't tell how much of them matched.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && std::hint::black_box(a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y))) == 0
}

impl Login {
    /// Starts a login with the mechanism the client chose. `secrets`: the verifiers its proof may
    /// match, all with one salt and iteration count (never empty); `doomed`: refused however it
    /// ends; `binding`: the certificate's tls-server-end-point hash, when SCRAM-SHA-256-PLUS was offered.
    pub fn start(
        mechanism: &[u8],
        secrets: Vec<Verifier>,
        doomed: bool,
        binding: Option<&[u8]>,
    ) -> Result<Login, SqlError> {
        let mut raw = [0u8; NONCE_LEN];
        ring::rand::SystemRandom::new()
            .fill(&mut raw)
            .map_err(|_| refusal("XX000", "could not generate random nonce", None))?;
        Login::with_nonce(mechanism, secrets, doomed, binding, b64(&raw))
    }

    fn with_nonce(
        mechanism: &[u8],
        secrets: Vec<Verifier>,
        doomed: bool,
        binding: Option<&[u8]>,
        server_nonce: String,
    ) -> Result<Login, SqlError> {
        let plus = match mechanism {
            b"SCRAM-SHA-256-PLUS" if binding.is_some() => true,
            b"SCRAM-SHA-256" => false,
            _ => return Err(refusal("08P01", "client selected an invalid SASL authentication mechanism", None)),
        };
        if secrets.is_empty() {
            return Err(refusal("XX000", "no SCRAM secret to check the login against", None));
        }
        Ok(Login {
            secrets,
            doomed,
            binding: binding.filter(|_| plus).map(<[u8]>::to_vec),
            offered_plus: binding.is_some(),
            server_nonce,
            flag: 0,
            client_first_bare: vec![],
            client_nonce: vec![],
            server_first: vec![],
        })
    }

    /// Puts `secrets` in place of those the login started with, before the final message is
    /// checked: verifiers made only once the client has sent its proof, with the salt and iteration
    /// count the server-first-message gave (those of the secrets it started with). Nothing if empty.
    pub fn secrets(&mut self, secrets: Vec<Verifier>) {
        let (salt, iterations) = (&self.secrets[0].salt, self.secrets[0].iterations);
        debug_assert!(secrets.iter().all(|v| &v.salt == salt && v.iterations == iterations));
        if !secrets.is_empty() {
            self.secrets = secrets;
        }
    }

    /// The client-first-message, answered with the server-first-message.
    pub fn first(&mut self, msg: &[u8]) -> Result<Vec<u8>, SqlError> {
        let mut a = Attrs(text(msg)?, 0);
        self.flag = a.peek().unwrap_or(0);
        let unbound = || {
            malformed(
                "The client selected SCRAM-SHA-256-PLUS, but the SCRAM message does not include channel binding data.",
            )
        };
        match self.flag {
            b'n' | b'y' => {
                if self.binding.is_some() {
                    return Err(unbound());
                }
                if self.flag == b'y' && self.offered_plus {
                    return Err(refusal(
                        "28000",
                        "SCRAM channel binding negotiation error",
                        Some("The client supports SCRAM channel binding but thinks the server does not.  However, this server does support channel binding.".into()),
                    ));
                }
                a.1 += 1;
                if a.peek() != Some(b',') {
                    return Err(malformed(format!("Comma expected, but found character \"{}\".", shown(a.peek()))));
                }
                a.1 += 1;
            }
            b'p' => {
                if self.binding.is_none() {
                    return Err(malformed(
                        "The client selected SCRAM-SHA-256 without channel binding, but the SCRAM message includes channel binding data.",
                    ));
                }
                let kind = a.attr(b'p')?;
                if kind != b"tls-server-end-point" {
                    // as Postgres shows it: 30 bytes at most, `?` for what isn't printable
                    let kind: String = kind
                        .iter()
                        .take(30)
                        .map(|&c| if (0x21..=0x7e).contains(&c) { c as char } else { '?' })
                        .collect();
                    return Err(refusal("08P01", format!("unsupported SCRAM channel-binding type \"{kind}\""), None));
                }
            }
            c => return Err(malformed(format!("Unexpected channel-binding flag \"{}\".", shown(Some(c))))),
        }
        if a.peek() == Some(b'a') {
            return Err(refusal("0A000", "client uses authorization identity, but it is not supported", None));
        }
        if a.peek() != Some(b',') {
            return Err(malformed(format!("Unexpected attribute \"{}\" in client-first-message.", shown(a.peek()))));
        }
        a.1 += 1;
        self.client_first_bare = a.0[a.1..].to_vec();
        if a.peek() == Some(b'm') {
            return Err(refusal("0A000", "client requires an unsupported SCRAM extension", None));
        }
        a.attr(b'n')?; // the user name: Postgres takes it from the startup message, and so does this
        let nonce = a.attr(b'r')?;
        // printable: %x21-2B / %x2D-7E
        if !nonce.iter().all(|&c| (0x21..=0x7e).contains(&c) && c != b',') {
            return Err(refusal("08P01", "non-printable characters in SCRAM nonce", None));
        }
        self.client_nonce = nonce.to_vec();
        while a.peek().is_some() {
            a.any()?; // extensions: none is supported, all are ignored
        }
        let v = &self.secrets[0];
        let mut m = b"r=".to_vec();
        m.extend(&self.client_nonce);
        m.extend(format!("{},s={},i={}", self.server_nonce, b64(&v.salt), v.iterations).bytes());
        self.server_first = m.clone();
        Ok(m)
    }

    /// The client-final-message: Ok((which secret its proof matched, the server-final-message)),
    /// or 28P01 for `user` if it proves no password the login takes.
    pub fn last(&mut self, msg: &[u8], user: &str) -> Result<(usize, Vec<u8>), SqlError> {
        let mut a = Attrs(text(msg)?, 0);
        let binding = a.attr(b'c')?;
        match &self.binding {
            Some(hash) => {
                let mut want = b"p=tls-server-end-point,,".to_vec();
                want.extend(hash);
                if !same(binding, b64(&want).as_bytes()) {
                    return Err(refusal("28000", "SCRAM channel binding check failed", None));
                }
            }
            None => {
                if !((binding == b"biws" && self.flag == b'n') || (binding == b"eSws" && self.flag == b'y')) {
                    return Err(refusal(
                        "08P01",
                        "unexpected SCRAM channel-binding attribute in client-final-message",
                        None,
                    ));
                }
            }
        }
        let nonce = a.attr(b'r')?;
        // extensions until the proof
        let (at, proof) = loop {
            let at = a.1;
            if let (b'p', v) = a.any()? {
                break (at, v);
            }
        };
        let proof = std::str::from_utf8(proof).ok().and_then(unb64).filter(|p| p.len() == digest::SHA256_OUTPUT_LEN);
        let Some(proof) = proof else { return Err(malformed("Malformed proof in client-final-message.")) };
        if a.peek().is_some() {
            return Err(malformed("Garbage found at the end of client-final-message."));
        }
        let want = [self.client_nonce.as_slice(), self.server_nonce.as_bytes()].concat();
        if nonce != want {
            return Err(refusal("08P01", "invalid SCRAM response", Some("Nonce does not match.".into())));
        }
        let mut auth = self.client_first_bare.clone();
        auth.push(b',');
        auth.extend(&self.server_first);
        auth.push(b',');
        auth.extend(&a.0[..at - 1]); // client-final-message-without-proof
        // every secret is checked, so the time taken doesn't tell which matched
        let mut matched = None;
        for (i, v) in self.secrets.iter().enumerate() {
            let signature = hmac256(&v.stored_key, &auth);
            let client_key: Vec<u8> = proof.iter().zip(&signature).map(|(p, s)| p ^ s).collect();
            let ok = same(digest::digest(&digest::SHA256, &client_key).as_ref(), &v.stored_key);
            if ok && matched.is_none() {
                matched = Some(i);
            }
        }
        match matched {
            Some(i) if !self.doomed => {
                let signature = hmac256(&self.secrets[i].server_key, &auth);
                Ok((i, format!("v={}", b64(&signature)).into_bytes()))
            }
            _ => Err(refusal("28P01", format!("password authentication failed for user \"{user}\""), None)),
        }
    }
}

/// A certificate's tls-server-end-point channel binding data (RFC 5929), as Postgres makes it:
/// its DER hashed with the hash its signature uses, SHA-256 for MD5 and SHA-1. None when that is
/// none of SHA-224, 256, 384 or 512 (EdDSA, which has none) or the certificate can't be read.
pub fn end_point(cert: &[u8]) -> Option<Vec<u8>> {
    // Certificate ::= SEQUENCE { tbsCertificate, signatureAlgorithm AlgorithmIdentifier, signature }
    let mut c = Der(Der(cert).take(0x30)?);
    c.take(0x30)?;
    let mut alg = Der(c.take(0x30)?);
    let oid = alg.take(0x06)?;
    const RSA: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01]; // 1.2.840.113549.1.1
    const ECDSA: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04]; // 1.2.840.10045.4
    const DSA_SHA1: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x38, 0x04, 0x03]; // 1.2.840.10040.4.3
    let hash = match oid {
        [rsa @ .., n] if rsa == RSA => match n {
            4 | 5 | 11 => Hash::Ring(&digest::SHA256), // md5, sha1, sha256 WithRSAEncryption
            12 => Hash::Ring(&digest::SHA384),
            13 => Hash::Ring(&digest::SHA512),
            14 => Hash::Sha224,
            10 => pss_hash(&mut alg)?, // RSASSA-PSS: the hash its parameters name
            _ => return None,
        },
        [ec @ .., 1] if ec == ECDSA => Hash::Ring(&digest::SHA256), // ecdsa-with-SHA1
        [ec @ .., 3, n] if ec == ECDSA => match n {
            1 => Hash::Sha224,
            2 => Hash::Ring(&digest::SHA256),
            3 => Hash::Ring(&digest::SHA384),
            4 => Hash::Ring(&digest::SHA512),
            _ => return None,
        },
        o if o == DSA_SHA1 => Hash::Ring(&digest::SHA256),
        _ => return None,
    };
    Some(match hash {
        Hash::Ring(a) => digest::digest(a, cert).as_ref().to_vec(),
        Hash::Sha224 => super::bytes::sha224(cert),
    })
}

/// A certificate's hash: one of `ring`'s, or SHA-224 (which `ring` hasn't).
enum Hash {
    Ring(&'static digest::Algorithm),
    Sha224,
}

/// The hash RSASSA-PSS parameters name (`hashAlgorithm [0]`, SHA-1 if absent: then SHA-256).
fn pss_hash(params: &mut Der) -> Option<Hash> {
    let mut p = Der(params.take(0x30)?);
    if p.0.first() != Some(&0xa0) {
        return Some(Hash::Ring(&digest::SHA256));
    }
    let oid = Der(Der(p.take(0xa0)?).take(0x30)?).take(0x06)?;
    const SHA2: &[u8] = &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02]; // 2.16.840.1.101.3.4.2
    match oid {
        [0x2b, 0x0e, 0x03, 0x02, 0x1a] => Some(Hash::Ring(&digest::SHA256)), // sha1
        [s @ .., n] if s == SHA2 => match n {
            1 => Some(Hash::Ring(&digest::SHA256)),
            2 => Some(Hash::Ring(&digest::SHA384)),
            3 => Some(Hash::Ring(&digest::SHA512)),
            4 => Some(Hash::Sha224),
            _ => None,
        },
        _ => None,
    }
}

/// Reads DER: one tag-length-value after another.
struct Der<'a>(&'a [u8]);

impl<'a> Der<'a> {
    /// The contents of the next value, which must have tag `tag`.
    fn take(&mut self, tag: u8) -> Option<&'a [u8]> {
        let (&t, rest) = self.0.split_first()?;
        let (&l, rest) = rest.split_first()?;
        if t != tag {
            return None;
        }
        let (len, rest) = match l {
            0..=0x7f => (usize::from(l), rest),
            0x81..=0x84 => {
                let n = usize::from(l & 0x7f);
                let digits = rest.get(..n)?;
                (digits.iter().fold(0usize, |a, &d| a << 8 | usize::from(d)), &rest[n..])
            }
            _ => return None,
        };
        let value = rest.get(..len)?;
        self.0 = &rest[len..];
        Some(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_verifier_is_what_postgres_makes() {
        // `create role r password 'secret'` on Postgres 16 gave this, with the salt it drew
        let pg = "SCRAM-SHA-256$4096:wumW+WjYyKCelfYxZ9y7Og==$UBs4bxfcqZ/NiIcwyl2y0nf8n/x5JbJi6L5+WDQPj48=:OSfxkKH8ld4XKQYnAp48qJIvYUi0kSqoIE8+BKbnr90=";
        let v = parse(pg).unwrap();
        assert_eq!(verifier("secret", &v.salt, v.iterations), pg);
        // a new one: Postgres's shape, its own salt, never the password
        let mine = kept("secret", "r", "scram-sha-256", ITERATIONS).unwrap();
        let p = parse(&mine).unwrap();
        assert_eq!((p.iterations, p.salt.len()), (4096, 16));
        assert_eq!(verifier("secret", &p.salt, 4096), mine);
        assert_ne!(kept("secret", "r", "scram-sha-256", ITERATIONS).unwrap(), mine, "a fresh salt each time");
        assert!(!mine.contains("secret"));
        // md5: Postgres's `md5(password || role)`
        assert_eq!(kept("secret", "r1_vec", "md5", ITERATIONS).unwrap(), "md5ec182998978d1e15ba1af54abc81abe0");
        // hashed already: kept as given
        assert_eq!(kept(pg, "r", "md5", ITERATIONS).unwrap(), pg);
        assert_eq!(
            kept("md5ec182998978d1e15ba1af54abc81abe0", "x", "scram-sha-256", ITERATIONS).unwrap(),
            "md5ec182998978d1e15ba1af54abc81abe0"
        );
        assert!(parse("SCRAM-SHA-256$4096:abc$def:ghi").is_none());
    }

    #[test]
    fn a_password_in_the_clear_is_checked_as_postgres_checks_it() {
        // `create role r password 'secret'` on Postgres 16, and its md5 of 'secret' for r1_vec (see above)
        let pg = "SCRAM-SHA-256$4096:wumW+WjYyKCelfYxZ9y7Og==$UBs4bxfcqZ/NiIcwyl2y0nf8n/x5JbJi6L5+WDQPj48=:OSfxkKH8ld4XKQYnAp48qJIvYUi0kSqoIE8+BKbnr90=";
        assert!(plain_matches(pg, "secret", "r"));
        assert!(plain_matches(pg, "secret", "another name: a verifier doesn't hold it"));
        for wrong in ["Secret", "secret ", "", "secre"] {
            assert!(!plain_matches(pg, wrong, "r"), "{wrong:?}");
        }
        let md5 = "md5ec182998978d1e15ba1af54abc81abe0";
        assert!(plain_matches(md5, "secret", "r1_vec"));
        assert!(!plain_matches(md5, "secret", "r1_ve"), "md5 hashes the name too");
        assert!(!plain_matches(md5, "Secret", "r1_vec"));
        // the password as SASLprep makes it, as a SCRAM login sends it
        let salt = [7u8; 16];
        assert!(plain_matches(&verifier("IX", &salt, 4096), "\u{2168}", "r"));
        // a verifier whose server key is wrong (its stored key right) is no match
        let v = parse(pg).unwrap();
        let bad = format!("SCRAM-SHA-256$4096:{}${}:{}", b64(&v.salt), b64(&v.stored_key), b64(&[0u8; 32]));
        assert!(!plain_matches(&bad, "secret", "r"));
        assert!(!plain_matches("secret", "secret", "r"), "a kept password that is neither hash");
    }

    #[test]
    fn saslprep_as_postgres_runs_it() {
        // RFC 4013's examples, as Postgres has them (a prohibited one is the password as typed)
        for (given, prepared) in [
            ("I\u{AD}X", "IX"),
            ("user", "user"),
            ("USER", "USER"),
            ("\u{AA}", "a"),
            ("\u{2168}", "IX"),
            ("\u{7}", "\u{7}"),
            ("\u{627}1", "\u{627}1"),
        ] {
            assert_eq!(saslprep(given), prepared, "{given:?}");
        }
        for (given, prepared) in [
            ("a\u{A0}b\u{200B}c", "a b c"), // spaces outside ASCII (the zero-width one too) are spaces
            ("\u{FF50}\u{FF41}ss", "pass"),
            ("e\u{301}t\u{E9}", "\u{E9}t\u{E9}"),
            ("\u{627}1\u{628}", "\u{627}1\u{628}"),
            ("\u{FF21}\u{627}", "\u{FF21}\u{627}"), // left to right with right to left
            ("\u{AD}", "\u{AD}"),                   // nothing left
            ("\u{2150}", "\u{2150}"),               // unassigned in Unicode 3.2
            ("a\u{340}", "a\u{340}"),               // deprecated, checked before NFKC
            ("\u{FF41}\u{E000}", "\u{FF41}\u{E000}"), // private use
        ] {
            assert_eq!(saslprep(given), prepared, "{given:?}");
        }
        // a verifier is of the prepared password
        let salt = [7u8; 16];
        assert_eq!(verifier("\u{2168}", &salt, 4096), verifier("IX", &salt, 4096));
        assert_ne!(verifier("\u{627}1", &salt, 4096), verifier("\u{627}1 ", &salt, 4096));
    }

    #[test]
    fn pbkdf2_as_ring_makes_it_and_stops_when_asked() {
        for n in [1u32, 2, 4095, 4096, 4097, 10_000] {
            let mut want = [0u8; 32];
            let iterations = std::num::NonZeroU32::new(n).unwrap();
            ring::pbkdf2::derive(ring::pbkdf2::PBKDF2_HMAC_SHA256, iterations, b"salt", b"pw", &mut want);
            assert_eq!(pbkdf2(b"pw", b"salt", n, &mut || Ok(())).unwrap(), want, "{n}");
        }
        // asked every 4096 iterations: the first refusal stops it
        let mut asked = 0;
        let mut stop = || {
            asked += 1;
            Err(refusal("57014", "canceling statement due to user request", None))
        };
        assert_eq!(pbkdf2(b"pw", b"salt", u32::MAX, &mut stop).unwrap_err().code, "57014");
        assert_eq!(asked, 1);
    }

    #[test]
    fn a_mock_salt_is_the_names_own() {
        let secret = [1u8; 32];
        let a = mock_salt("alice", &secret);
        assert_eq!(a.len(), 16);
        assert_eq!(a, mock_salt("alice", &secret));
        assert_ne!(a, mock_salt("bob", &secret));
        assert_ne!(a, mock_salt("alice", &[2u8; 32]));
        // Postgres's: SHA-256 of the name, then the secret
        let mut whole = digest::Context::new(&digest::SHA256);
        whole.update(b"alice\x01");
        whole.update(&[1u8; 31]);
        assert_eq!(a, whole.finish().as_ref()[..16]);
        // and the iterations a verifier is made with
        let v = parse(&fresh("pw", 1000).unwrap()).unwrap();
        assert_eq!((v.iterations, v.salt.len()), (1000, 16));
        assert_eq!(kept("pw", "r", "scram-sha-256", 7).unwrap().split(':').next(), Some("SCRAM-SHA-256$7"));
    }

    /// RFC 7677's example exchange (user "user", password "pencil"), the server's side.
    const C_FIRST: &[u8] = b"n,,n=user,r=rOprNGfwEbeRWgbNEkqO";
    const S_NONCE: &str = "%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0";
    const C_FINAL: &[u8] =
        b"c=biws,r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,p=dHzbZapWIk4jUhN+Ute9ytag9zjfMHgsqmmiz7AndVQ=";

    fn pencil() -> Verifier {
        static V: std::sync::LazyLock<Verifier> = std::sync::LazyLock::new(|| {
            parse(&verifier("pencil", &unb64("W22ZaJ0SNY7soEsUEjb6gQ==").unwrap(), 4096)).unwrap()
        });
        V.clone()
    }

    fn login(secrets: Vec<Verifier>, doomed: bool, mech: &[u8], binding: Option<&[u8]>) -> Login {
        Login::with_nonce(mech, secrets, doomed, binding, S_NONCE.into()).unwrap()
    }

    fn code(r: Result<impl std::fmt::Debug, SqlError>) -> (&'static str, String) {
        let e = r.unwrap_err();
        (e.code, e.message)
    }

    #[test]
    fn rfc_7677_exchange() {
        let mut l = login(vec![pencil()], false, b"SCRAM-SHA-256", None);
        let first = l.first(C_FIRST).unwrap();
        assert_eq!(
            String::from_utf8(first).unwrap(),
            "r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096"
        );
        let (i, last) = l.last(C_FINAL, "user").unwrap();
        assert_eq!(
            (i, String::from_utf8(last).unwrap().as_str()),
            (0, "v=6rriTRBi23WpRR/wtup+mMhUZUn/dB5nLTJRsjl95G4=")
        );

        // the proof matches the second of two secrets with the same salt: that one answers
        let other = parse(&verifier("crayon", &pencil().salt, 4096)).unwrap();
        let mut l = login(vec![other.clone(), pencil()], false, b"SCRAM-SHA-256", None);
        l.first(C_FIRST).unwrap();
        assert_eq!(l.last(C_FINAL, "user").unwrap().0, 1);
        // a wrong password, or a doomed login with the right one: 28P01
        let wrong = ("28P01", "password authentication failed for user \"user\"".to_string());
        let mut l = login(vec![other], false, b"SCRAM-SHA-256", None);
        l.first(C_FIRST).unwrap();
        assert_eq!(code(l.last(C_FINAL, "user")), wrong);
        let mut l = login(vec![pencil()], true, b"SCRAM-SHA-256", None);
        l.first(C_FIRST).unwrap();
        assert_eq!(code(l.last(C_FINAL, "user")), wrong);
    }

    /// The client's side, for the tests: (client-first, client-final) for `password` with this
    /// GS2 header and channel binding data.
    fn client(password: &str, gs2: &str, binding: &[u8], l: &mut Login) -> Result<Vec<u8>, SqlError> {
        let first_bare = "n=,r=abc";
        let server_first = String::from_utf8(l.first(format!("{gs2}{first_bare}").as_bytes())?).unwrap();
        let nonce = server_first.split(',').next().unwrap().strip_prefix("r=").unwrap();
        let v = &l.secrets[0];
        let salted = salted_password(password, &v.salt, v.iterations);
        let client_key = hmac256(&salted, b"Client Key");
        let stored = digest::digest(&digest::SHA256, &client_key);
        let without = format!("c={},r={nonce}", b64(&[gs2.as_bytes(), binding].concat()));
        let auth = format!("{first_bare},{server_first},{without}");
        let sig = hmac256(stored.as_ref(), auth.as_bytes());
        let proof: Vec<u8> = client_key.iter().zip(&sig).map(|(a, b)| a ^ b).collect();
        let (_, fin) = l.last(format!("{without},p={}", b64(&proof)).as_bytes(), "u")?;
        Ok(fin)
    }

    #[test]
    fn channel_binding_as_postgres_checks_it() {
        let cert = b"a certificate's hash";
        let plus = || login(vec![pencil()], false, b"SCRAM-SHA-256-PLUS", Some(cert));
        // bound to the certificate the server has
        assert!(client("pencil", "p=tls-server-end-point,,", cert, &mut plus()).is_ok());
        // to another one (a server in the middle): refused
        assert_eq!(code(client("pencil", "p=tls-server-end-point,,", b"another", &mut plus())).0, "28000");
        // PLUS chosen, but no binding data: malformed
        assert_eq!(code(plus().first(b"n,,n=,r=abc")).0, "08P01");
        assert_eq!(code(plus().first(b"y,,n=,r=abc")).0, "08P01");
        assert_eq!(
            code(plus().first(b"p=tls-unique\x01,,n=,r=abc")),
            ("08P01", "unsupported SCRAM channel-binding type \"tls-unique?\"".into())
        );
        // plain SCRAM while PLUS was offered: `n` is fine, `y` (it could bind, it thinks the server can't) is refused
        let plain = |binding: Option<&[u8]>| login(vec![pencil()], false, b"SCRAM-SHA-256", binding);
        assert!(client("pencil", "n,,", b"", &mut plain(Some(cert))).is_ok());
        let e = plain(Some(cert)).first(b"y,,n=,r=abc").unwrap_err();
        assert_eq!((e.code, e.message.as_str()), ("28000", "SCRAM channel binding negotiation error"));
        assert_eq!(code(plain(Some(cert)).first(b"p=tls-server-end-point,,n=,r=abc")).0, "08P01");
        // where PLUS wasn't offered, `y` is what a client that could bind says
        assert!(client("pencil", "y,,", b"", &mut plain(None)).is_ok());
        assert_eq!(code(client("pencil", "n,,", b"x", &mut plain(None))).0, "08P01"); // c= isn't biws
        // PLUS where it wasn't offered, or no mechanism Postgres knows
        for m in [&b"SCRAM-SHA-256-PLUS"[..], b"SCRAM-SHA-1", b"", b"scram-sha-256"] {
            let e = Login::with_nonce(m, vec![pencil()], false, None, "n".into()).err().unwrap();
            assert_eq!(
                (e.code, e.message.as_str()),
                ("08P01", "client selected an invalid SASL authentication mechanism")
            );
        }
    }

    #[test]
    fn malformed_messages_are_postgres_refusals() {
        let first = |m: &[u8]| code(login(vec![pencil()], false, b"SCRAM-SHA-256", None).first(m));
        let detail = |m: &[u8]| {
            let e = login(vec![pencil()], false, b"SCRAM-SHA-256", None).first(m).unwrap_err();
            (e.code, e.info.and_then(|i| i.detail).unwrap_or_default())
        };
        assert_eq!(detail(b""), ("08P01", "The message is empty.".into()));
        assert_eq!(detail(b"n,,n=,r=a\0b"), ("08P01", "Message length does not match input length.".into()));
        assert_eq!(detail(b"x,,n=,r=a"), ("08P01", "Unexpected channel-binding flag \"'x'\".".into()));
        assert_eq!(detail(b"\x01,,n=,r=a"), ("08P01", "Unexpected channel-binding flag \"0x01\".".into()));
        assert_eq!(detail(b"n"), ("08P01", "Comma expected, but found character \"0x00\".".into()));
        assert_eq!(detail(b"n,,r=a"), ("08P01", "Expected attribute \"n\" but found \"'r'\".".into()));
        assert_eq!(detail(b"n,,n=,r"), ("08P01", "Expected character \"=\" for attribute \"r\".".into()));
        assert_eq!(
            detail(b"n,,n=,r=a,9=x"),
            ("08P01", "Attribute expected, but found invalid character \"'9'\".".into())
        );
        assert_eq!(
            first(b"n,a=bob,n=,r=a"),
            ("0A000", "client uses authorization identity, but it is not supported".into())
        );
        assert_eq!(first(b"n,,m=ext,n=,r=a"), ("0A000", "client requires an unsupported SCRAM extension".into()));
        assert_eq!(first(b"n,,n=,r=a b"), ("08P01", "non-printable characters in SCRAM nonce".into()));
        // extensions after the nonce are ignored, as are trailing commas
        let mut l = login(vec![pencil()], false, b"SCRAM-SHA-256", None);
        assert!(l.first(b"n,,n=user,r=rOprNGfwEbeRWgbNEkqO,x=1,").is_ok());

        let last = |m: &[u8]| {
            let mut l = login(vec![pencil()], false, b"SCRAM-SHA-256", None);
            l.first(C_FIRST).unwrap();
            let e = l.last(m, "user").unwrap_err();
            (e.code, e.message, e.info.and_then(|i| i.detail).unwrap_or_default())
        };
        let n = "rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0";
        let p = "p=dHzbZapWIk4jUhN+Ute9ytag9zjfMHgsqmmiz7AndVQ=";
        assert_eq!(last(format!("c=biws,r={n}x,{p}").as_bytes()).2, "Nonce does not match.");
        assert_eq!(last(format!("c=biws,r={n},p=abc").as_bytes()).2, "Malformed proof in client-final-message.");
        assert_eq!(
            last(format!("c=biws,r={n},{p},x").as_bytes()).2,
            "Garbage found at the end of client-final-message."
        );
        assert_eq!(
            last(format!("c=eSws,r={n},{p}").as_bytes()).1,
            "unexpected SCRAM channel-binding attribute in client-final-message"
        );
        assert_eq!(last(format!("c=biws,r={n}").as_bytes()).2, "Attribute expected, but found end of string.");
        // an extension before the proof is ignored, and the proof still holds
        let mut l = login(vec![pencil()], false, b"SCRAM-SHA-256", None);
        l.first(C_FIRST).unwrap();
        assert!(l.last(format!("c=biws,r={n},{p},").as_bytes(), "user").is_ok());
    }

    /// Garbage, truncated and oversized messages: always a refusal, never a panic.
    #[test]
    fn hostile_messages_never_panic() {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let alphabet = b"nyp=,armcrsivx0+/\0\x01\xff-tls-server-end-point";
        let mut messages: Vec<Vec<u8>> = vec![];
        for _ in 0..3000 {
            let len = (next() % 64) as usize;
            let m = (0..len).map(|_| match next() % 4 {
                0 => next() as u8,
                _ => alphabet[(next() % alphabet.len() as u64) as usize],
            });
            messages.push(m.collect());
        }
        for i in 0..=C_FIRST.len() {
            messages.push(C_FIRST[..i].to_vec());
        }
        for i in 0..=C_FINAL.len() {
            messages.push(C_FINAL[..i].to_vec());
        }
        let big = "A".repeat(1 << 20);
        for m in [
            format!("n,,n={big},r=a"),
            format!("n,,n=,r={big}"),
            format!("p={big},,n=,r=a"),
            format!("c={big},r=a,p=b"),
            format!("c=biws,r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,p={big}"),
            format!("c=biws,r={big},p=dHzbZapWIk4jUhN+Ute9ytag9zjfMHgsqmmiz7AndVQ="),
            ",".repeat(1 << 16),
            "=".repeat(1 << 16),
        ] {
            messages.push(m.into_bytes());
        }
        let codes = ["08P01", "0A000", "28000", "28P01"];
        for m in &messages {
            for (mech, binding) in [(&b"SCRAM-SHA-256"[..], None), (b"SCRAM-SHA-256-PLUS", Some(&b"h"[..]))] {
                // as a first message
                let mut l = login(vec![pencil()], false, mech, binding);
                if let Err(e) = l.first(m) {
                    assert!(codes.contains(&e.code), "{e:?}");
                }
                // as a final message, after a good first one
                let mut l = login(vec![pencil()], false, mech, binding);
                let gs2 = if binding.is_some() { "p=tls-server-end-point,," } else { "n,," };
                l.first(format!("{gs2}n=,r=abc").as_bytes()).unwrap();
                if let Err(e) = l.last(m, "u") {
                    assert!(codes.contains(&e.code), "{e:?}");
                }
            }
        }
    }

    #[test]
    fn md5_as_postgres_salts_it() {
        // `md5` and 32 lowercase hex digits is a hash; with uppercase ones, a password like any other
        assert!(is_md5("md5ec182998978d1e15ba1af54abc81abe0"));
        assert!(!is_md5("md5EC182998978D1E15BA1AF54ABC81ABE0"));
        assert!(
            kept("md5EC182998978D1E15BA1AF54ABC81ABE0", "r", "scram-sha-256", ITERATIONS)
                .unwrap()
                .starts_with("SCRAM-SHA-256$")
        );
        assert_eq!(md5_hash("secret", "r1_vec"), "md5ec182998978d1e15ba1af54abc81abe0");
        // md5(hex md5(password || role) || salt), as libpq sends it
        assert_eq!(
            md5_salted("md5ec182998978d1e15ba1af54abc81abe0", &[1, 2, 3, 4]),
            "md5daf15a9f815fc14dd42066f97ce82312"
        );
    }

    /// A DER certificate with only what `end_point` reads: an empty tbsCertificate, the signature
    /// algorithm (`oid`, then `params`) and an empty signature.
    fn cert(oid: &[u8], params: &[u8]) -> Vec<u8> {
        let tlv = |tag: u8, v: &[u8]| {
            let mut b = vec![tag];
            if v.len() < 0x80 {
                b.push(v.len() as u8);
            } else {
                b.extend([0x82, (v.len() >> 8) as u8, v.len() as u8]);
            }
            b.extend(v);
            b
        };
        let alg = tlv(0x30, &[tlv(0x06, oid), params.to_vec()].concat());
        let pad = tlv(0x04, &[7; 300]); // long enough for a long-form length
        tlv(0x30, &[tlv(0x30, &pad), alg, tlv(0x03, &[0])].concat())
    }

    #[test]
    fn end_point_hashes_the_certificate_as_its_signature_does() {
        let rsa = |n: u8| [0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, n];
        let ecdsa = |n: &[u8]| [&[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04][..], n].concat();
        let h = |c: &[u8], a| Some(digest::digest(a, c).as_ref().to_vec());
        for (oid, alg) in [
            (rsa(4).to_vec(), &digest::SHA256), // md5WithRSAEncryption: SHA-256, as RFC 5929 says
            (rsa(5).to_vec(), &digest::SHA256), // sha1WithRSAEncryption
            (rsa(11).to_vec(), &digest::SHA256),
            (rsa(12).to_vec(), &digest::SHA384),
            (rsa(13).to_vec(), &digest::SHA512),
            (ecdsa(&[1]), &digest::SHA256), // ecdsa-with-SHA1
            (ecdsa(&[3, 2]), &digest::SHA256),
            (ecdsa(&[3, 3]), &digest::SHA384),
            (ecdsa(&[3, 4]), &digest::SHA512),
        ] {
            let c = cert(&oid, &[0x05, 0x00]);
            assert_eq!(end_point(&c), h(&c, alg), "{oid:x?}");
        }
        // RSASSA-PSS: the hash its parameters name, SHA-1 (so SHA-256) without one
        let sha2 = |n: u8| [0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, n];
        let pss = |hash: &[u8]| {
            let alg = [&[0x30, hash.len() as u8][..], hash].concat();
            let named = [&[0xa0, alg.len() as u8][..], &alg].concat();
            [&[0x30, named.len() as u8][..], &named].concat()
        };
        let c = cert(&rsa(10), &pss(&sha2(2)));
        assert_eq!(end_point(&c), h(&c, &digest::SHA384));
        let c = cert(&rsa(10), &[0x30, 0x00]);
        assert_eq!(end_point(&c), h(&c, &digest::SHA256));
        // SHA-224, which `ring` hasn't: by RSA, ECDSA and PSS
        let sha224 = |c: &[u8]| Some(super::super::bytes::sha224(c));
        for c in [cert(&rsa(14), &[0x05, 0x00]), cert(&ecdsa(&[3, 1]), &[]), cert(&rsa(10), &pss(&sha2(4)))] {
            assert_eq!(end_point(&c), sha224(&c));
        }
        // Ed25519 (no hash of its own), a PSS hash it doesn't know, and garbage: no channel binding
        assert_eq!(end_point(&cert(&[0x2b, 0x65, 0x70], &[])), None);
        assert_eq!(end_point(&cert(&rsa(10), &pss(&sha2(9)))), None);
        let good = cert(&rsa(11), &[0x05, 0x00]);
        for i in 0..good.len() {
            assert_eq!(end_point(&good[..i]), None);
        }
        assert_eq!(end_point(&[0x30, 0x84, 0xff, 0xff, 0xff, 0xff]), None);
    }
}
