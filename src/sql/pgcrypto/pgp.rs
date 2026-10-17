//! OpenPGP's symmetric encryption (RFC 4880) as pgcrypto has it: a symmetric-key session key packet with
//! its S2K (and maybe a separate session key), then the data in a SEIPD packet with an MDC, or a SED packet
//! (`disable-mdc=1`), holding a literal data packet, maybe compressed. Reading takes what gpg writes too:
//! old-format headers, partial and indeterminate lengths. Also the ASCII armor and pgp_key_id.

use super::super::{Result, SqlError, funcs, stopped};
use super::{Block, Hash, Md, px, random};
use std::borrow::Cow;

const CORRUPT: &str = "Wrong key or corrupt data";

fn corrupt<T>() -> Result<T> {
    Err(px(CORRUPT))
}

// ---------------------------------------------------------------- algorithms

/// A cipher by pgcrypto's name for it: its OpenPGP number.
fn cipher_code(name: &str) -> Option<u8> {
    Some(match name.to_ascii_lowercase().as_str() {
        "3des" => 2,
        "cast5" => 3,
        "bf" | "blowfish" => 4,
        "aes" | "aes128" => 7,
        "aes192" => 8,
        "aes256" => 9,
        "twofish" => 10,
        _ => return None,
    })
}

/// A cipher's key size in bytes; 0 for one pgcrypto doesn't know.
fn key_size(code: u8) -> usize {
    match code {
        2 | 8 => 24,
        3 | 4 | 7 => 16,
        9 | 10 => 32,
        _ => 0,
    }
}

/// The block cipher OpenPGP's number `code` means, with `key`.
fn block(code: u8, key: &[u8]) -> Result<Block> {
    if key.len() != key_size(code) {
        return Err(px("Unsupported cipher algorithm"));
    }
    Ok(match code {
        2 => Block::des3(key),
        3 => Block::cast5(key),
        4 => Block::bf(key),
        7..=9 => Block::aes(key),
        _ => return Err(px("Unsupported cipher algorithm")),
    })
}

fn digest_code(name: &str) -> Option<u8> {
    Some(match name.to_ascii_lowercase().as_str() {
        "md5" => 1,
        "sha1" | "sha-1" => 2,
        "ripemd160" => 3,
        "sha256" => 8,
        "sha384" => 9,
        "sha512" => 10,
        _ => return None,
    })
}

fn hash(code: u8) -> Result<Hash> {
    Ok(match code {
        1 => Hash::Md5,
        2 => Hash::Sha1,
        3 => Hash::Ripemd160,
        8 => Hash::Sha256,
        9 => Hash::Sha384,
        10 => Hash::Sha512,
        _ => return Err(px("Unsupported digest algorithm")),
    })
}

// ---------------------------------------------------------------- options

/// pgp_sym_encrypt's and pgp_sym_decrypt's options, pgcrypto's defaults first.
pub(in super::super) struct Options {
    cipher: u8,
    s2k_cipher: Option<u8>,
    s2k_mode: u8,
    s2k_count: Option<u32>,
    s2k_digest: u8,
    compress: u8,
    level: u32,
    disable_mdc: bool,
    sess_key: bool,
    unicode: bool,
    convert_crlf: bool,
}

/// C's atoi, as pgcrypto reads a number: leading digits (and a sign), 0 for none, a long cut to an int.
fn atoi(s: &str) -> i32 {
    let b = s.trim_start().as_bytes();
    let (neg, digits) = match b.first() {
        Some(b'-') => (true, &b[1..]),
        Some(b'+') => (false, &b[1..]),
        _ => (false, b),
    };
    let mut n: i64 = 0;
    for &d in digits.iter().take_while(|d| d.is_ascii_digit()) {
        n = n.saturating_mul(10).saturating_add(i64::from(d - b'0'));
    }
    (if neg { n.saturating_neg() } else { n }) as i32
}

impl Options {
    /// `key=value` pairs separated by commas, any case, spaces around each, as pgcrypto's parse_args reads
    /// them; set in order (so `s2k-count` is checked against the `s2k-mode` set before it).
    pub(in super::super) fn parse(args: &str) -> Result<Options> {
        let illegal = || Err(px("Illegal argument to function"));
        let mut o = Options {
            cipher: 7,
            s2k_cipher: None,
            s2k_mode: 3,
            s2k_count: None,
            s2k_digest: 2,
            compress: 0,
            level: 6,
            disable_mdc: false,
            sess_key: false,
            unicode: false,
            convert_crlf: false,
        };
        let s = args.to_ascii_lowercase();
        let b = s.as_bytes();
        let space = |c: u8| matches!(c, b' ' | b'\t' | b'\n');
        let word = |p: &mut usize| -> String {
            while *p < b.len() && space(b[*p]) {
                *p += 1;
            }
            let start = *p;
            while *p < b.len() && !space(b[*p]) && b[*p] != b'=' && b[*p] != b',' {
                *p += 1;
            }
            let w = String::from_utf8_lossy(&b[start..*p]).into_owned();
            while *p < b.len() && space(b[*p]) {
                *p += 1;
            }
            w
        };
        let mut p = 0;
        while p < b.len() {
            let key = word(&mut p);
            if b.get(p) != Some(&b'=') {
                return illegal();
            }
            p += 1;
            let val = word(&mut p);
            match b.get(p) {
                None => {}
                Some(b',') => p += 1,
                Some(_) => return illegal(),
            }
            if key.is_empty() || val.is_empty() {
                return illegal();
            }
            let n = || atoi(&val);
            match key.as_str() {
                "cipher-algo" => o.cipher = cipher_code(&val).ok_or_else(|| px("Unsupported cipher algorithm"))?,
                "disable-mdc" => o.disable_mdc = n() != 0,
                "sess-key" => o.sess_key = n() != 0,
                "s2k-mode" => match n() {
                    m @ (0 | 1 | 3) => o.s2k_mode = m as u8,
                    _ => return illegal(),
                },
                "s2k-count" if o.s2k_mode == 3 && (1024..=65_011_712).contains(&n()) => o.s2k_count = Some(n() as u32),
                "s2k-count" => return illegal(),
                "s2k-digest-algo" => {
                    o.s2k_digest = digest_code(&val).ok_or_else(|| px("Unsupported digest algorithm"))?
                }
                "s2k-cipher-algo" => {
                    o.s2k_cipher = Some(cipher_code(&val).ok_or_else(|| px("Unsupported cipher algorithm"))?)
                }
                // (bzip2, 3, is taken here, and refused when there is something to compress)
                "compress-algo" => match n() {
                    a @ 0..=3 => o.compress = a as u8,
                    _ => return illegal(),
                },
                "compress-level" => match n() {
                    l @ 0..=9 => o.level = l as u32,
                    _ => return illegal(),
                },
                "convert-crlf" => o.convert_crlf = n() != 0,
                "unicode-mode" => o.unicode = n() != 0,
                // for testing pgcrypto: what decryption should find (Postgres says if it doesn't)
                "debug"
                | "expect-cipher-algo"
                | "expect-disable-mdc"
                | "expect-sess-key"
                | "expect-s2k-mode"
                | "expect-s2k-count"
                | "expect-s2k-digest-algo"
                | "expect-s2k-cipher-algo"
                | "expect-compress-algo"
                | "expect-unicode-mode" => {}
                _ => return illegal(),
            }
        }
        Ok(o)
    }
}

// ---------------------------------------------------------------- S2K

/// The count an S2K count byte stands for.
fn decode_count(c: u8) -> usize {
    (16 + usize::from(c & 15)) << (usize::from(c >> 4) + 6)
}

/// The key of `len` bytes S2K makes from the password: the digest of the salt and password (repeated to
/// `count` bytes, for mode 3), more digests preloaded with zeros where one isn't enough.
fn s2k(mode: u8, digest: u8, salt: &[u8], count: usize, psw: &[u8], len: usize) -> Result<Vec<u8>> {
    let h = hash(digest)?;
    let mut key = Vec::with_capacity(len);
    let mut preload = 0;
    while key.len() < len {
        let mut md = Md::new(h);
        md.update(&vec![0; preload]);
        preload += 1;
        match mode {
            0 => md.update(psw),
            1 => {
                md.update(salt);
                md.update(psw);
            }
            _ => {
                // the salt and password over and over, `count` bytes (at least the whole of them once)
                let unit: Vec<u8> = salt.iter().chain(psw).copied().collect();
                md.update(&unit);
                let mut done = unit.len();
                let block: Vec<u8> =
                    unit.iter().copied().cycle().take(unit.len() * (65_536 / unit.len()).max(1)).collect();
                while done < count {
                    stopped()?;
                    let off = done % unit.len();
                    let n = (count - done).min(block.len() - off);
                    md.update(&block[off..off + n]);
                    done += n;
                }
            }
        }
        let d = md.finish();
        key.extend_from_slice(&d[..d.len().min(len - key.len())]);
    }
    Ok(key)
}

// ---------------------------------------------------------------- CFB

/// OpenPGP's CFB (pgcrypto's pgp-cfb.c): from a zero IV; with `resync` (a SED packet), the IV is set
/// again after the prefix's two check bytes.
struct Cfb {
    c: Block,
    bs: usize,
    fr: Vec<u8>,
    fre: Vec<u8>,
    encbuf: Vec<u8>,
    pos: usize,
    block_no: u32,
    resync: bool,
}

impl Cfb {
    fn new(c: Block, resync: bool) -> Cfb {
        let bs = c.size();
        Cfb { c, bs, fr: vec![0; bs], fre: vec![0; bs], encbuf: vec![0; bs], pos: 0, block_no: 0, resync }
    }

    /// Mixes up to the end of the block (or of block 2's two bytes, resyncing), returning how much it took.
    fn mix(&mut self, data: &[u8], out: &mut Vec<u8>, encrypt: bool) -> usize {
        let mut put = |cfb: &mut Cfb, i: usize, x: u8| {
            let (c, p) = if encrypt { (cfb.fre[i] ^ x, cfb.fre[i] ^ x) } else { (x, cfb.fre[i] ^ x) };
            cfb.encbuf[i] = c;
            out.push(p);
        };
        if self.resync && self.block_no == 2 {
            let n = (2 - self.pos).min(data.len());
            for (k, &x) in data[..n].iter().enumerate() {
                let i = self.pos + k;
                put(self, i, x);
            }
            self.pos += n;
            if self.pos == 2 {
                let bs = self.bs;
                let mut fr = self.encbuf[2..bs].to_vec();
                fr.extend_from_slice(&self.encbuf[..2]);
                self.fr = fr;
                self.pos = 0;
                return n;
            }
            return n;
        }
        let n = data.len();
        for (k, &x) in data.iter().enumerate() {
            let i = self.pos + k;
            put(self, i, x);
        }
        self.pos += n;
        n
    }

    fn process(&mut self, data: &[u8], encrypt: bool) -> Vec<u8> {
        let mut out = Vec::with_capacity(data.len());
        self.process_into(data, encrypt, &mut out);
        out
    }

    /// `process`, onto the end of `out`.
    fn process_into(&mut self, mut data: &[u8], encrypt: bool, out: &mut Vec<u8>) {
        while !data.is_empty() && self.pos > 0 {
            let n = (self.bs - self.pos).min(data.len());
            let took = self.mix(&data[..n], out, encrypt);
            data = &data[took..];
            if self.pos == self.bs {
                self.fr.copy_from_slice(&self.encbuf);
                self.pos = 0;
            }
        }
        while !data.is_empty() {
            self.fre.copy_from_slice(&self.fr);
            self.c.encrypt(&mut self.fre);
            if self.block_no < 5 {
                self.block_no += 1;
            }
            let n = self.bs.min(data.len());
            let took = self.mix(&data[..n], out, encrypt);
            data = &data[took..];
            if self.pos == self.bs {
                self.fr.copy_from_slice(&self.encbuf);
                self.pos = 0;
            }
        }
    }
}

// ---------------------------------------------------------------- packets

/// A packet's header (RFC 4880 4.2): new format (`0xC0 | tag`) and old (`0x80 | tag << 2 | length type`).
enum Len {
    Normal(usize),
    /// partial body lengths: this chunk, then another length
    Partial(usize),
    /// old format's indeterminate length: to the end of what holds it
    Context,
}

/// pgcrypto's longest chunk.
const MAX_CHUNK: usize = 16 * 1024 * 1024;

fn byte(b: &[u8], at: &mut usize) -> Result<u8> {
    let Some(&x) = b.get(*at) else { return corrupt() };
    *at += 1;
    Ok(x)
}

fn new_len(b: &[u8], at: &mut usize) -> Result<Len> {
    let first = byte(b, at)?;
    let len = match first {
        0..=191 => usize::from(first),
        192..=223 => ((usize::from(first) - 192) << 8) + usize::from(byte(b, at)?) + 192,
        255 => (0..4).try_fold(0usize, |n, _| Ok::<_, SqlError>(n << 8 | usize::from(byte(b, at)?)))?,
        _ => {
            let len = 1usize << (first & 0x1f);
            if len > MAX_CHUNK {
                return corrupt();
            }
            return Ok(Len::Partial(len));
        }
    };
    if len > MAX_CHUNK {
        return corrupt();
    }
    Ok(Len::Normal(len))
}

/// The next packet's tag and length, None at the end; an indeterminate length only where `context` allows.
fn header(b: &[u8], at: &mut usize, context: bool) -> Result<Option<(u8, Len)>> {
    let Some(&h) = b.get(*at) else { return Ok(None) };
    *at += 1;
    if h & 0x80 == 0 {
        return corrupt();
    }
    if h & 0x40 != 0 {
        return Ok(Some((h & 0x3f, new_len(b, at)?)));
    }
    let tag = (h >> 2) & 0x0f;
    let len = match h & 3 {
        3 if context => return Ok(Some((tag, Len::Context))),
        3 => return corrupt(),
        t => (0..[1, 2, 4][usize::from(t)])
            .try_fold(0usize, |n, _| Ok::<_, SqlError>(n << 8 | usize::from(byte(b, at)?)))?,
    };
    if len > MAX_CHUNK {
        return corrupt();
    }
    Ok(Some((tag, Len::Normal(len))))
}

/// A packet's body, as the pieces of `b` its chunks are (as much of each as there is).
fn chunks<'a>(b: &'a [u8], at: &mut usize, mut len: Len) -> Result<Vec<&'a [u8]>> {
    let mut out = vec![];
    loop {
        let (n, more) = match len {
            Len::Context => (b.len() - *at, false),
            Len::Normal(n) => (n, false),
            Len::Partial(n) => (n, true),
        };
        let end = (*at + n).min(b.len());
        out.push(&b[*at..end]);
        *at = end;
        if !more {
            return Ok(out);
        }
        len = new_len(b, at)?;
    }
}

/// A packet's body in one piece: `b`'s own bytes unless its chunks must be joined.
fn body<'a>(b: &'a [u8], at: &mut usize, len: Len) -> Result<Cow<'a, [u8]>> {
    let mut c = chunks(b, at, len)?;
    Ok(if c.len() == 1 { Cow::Borrowed(c.remove(0)) } else { Cow::Owned(c.concat()) })
}

/// A packet: new-format header, the body in one piece up to 16 KiB and in partial chunks of 16 KiB past
/// that, as pgcrypto writes it (it reads no length past 16 MiB).
fn packet(tag: u8, body: &[u8]) -> Vec<u8> {
    const CHUNK: usize = 1 << 14;
    let mut out = Vec::with_capacity(body.len() + 6 + body.len() / CHUNK);
    out.push(0xc0 | tag);
    let mut rest = body;
    while rest.len() > CHUNK {
        out.push(0xe0 | 14);
        out.extend_from_slice(&rest[..CHUNK]);
        rest = &rest[CHUNK..];
    }
    let n = rest.len();
    if n < 192 {
        out.push(n as u8);
    } else if n < 8384 {
        out.push((((n - 192) >> 8) + 192) as u8);
        out.push((n - 192) as u8);
    } else {
        out.push(255);
        out.extend_from_slice(&(n as u32).to_be_bytes());
    }
    out.extend_from_slice(rest);
    out
}

// ---------------------------------------------------------------- encrypt

pub(in super::super) fn encrypt(data: &[u8], psw: &[u8], text: bool, o: &Options) -> Result<Vec<u8>> {
    let s2k_cipher = o.s2k_cipher.unwrap_or(o.cipher);
    let s2k_len = key_size(s2k_cipher);
    // the S2K: a random salt, and a random count (65536 to 253952 bytes) unless one was given
    let r = random(9)?;
    let salt = &r[..8];
    let count_byte = match o.s2k_count {
        Some(c) => (0..=255u8).find(|&i| decode_count(i) >= c as usize).unwrap_or(255),
        None => 96 + (r[8] & 0x1f),
    };
    let key = s2k(o.s2k_mode, o.s2k_digest, salt, decode_count(count_byte), psw, s2k_len)?;
    // the data's key: a session key of its own, or the S2K's (with the S2K's cipher, which the packet names)
    let (data_cipher, sess) =
        if o.sess_key { (o.cipher, random(key_size(o.cipher))?) } else { (s2k_cipher, key.clone()) };
    let mut esk = vec![4, s2k_cipher, o.s2k_mode, o.s2k_digest];
    if o.s2k_mode != 0 {
        esk.extend_from_slice(salt);
    }
    if o.s2k_mode == 3 {
        esk.push(count_byte);
    }
    if o.sess_key {
        let mut cfb = Cfb::new(block(s2k_cipher, &key)?, false);
        esk.extend(cfb.process(&[data_cipher], true));
        esk.extend(cfb.process(&sess, true));
    }
    let mut out = packet(3, &esk);
    // the literal data, typed and dated, maybe compressed
    let kind = match (text, o.unicode) {
        (true, true) => b'u',
        (true, false) => b't',
        _ => b'b',
    };
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as u32);
    let mut lit = vec![kind, 0];
    lit.extend_from_slice(&now.to_be_bytes());
    if text && o.convert_crlf {
        for &c in data {
            if c == b'\n' {
                lit.push(b'\r');
            }
            lit.push(c);
        }
    } else {
        lit.extend_from_slice(data);
    }
    let mut inner = packet(11, &lit);
    // (compress-level=0 is no compression, as in pgcrypto: no compressed packet at all)
    if o.compress == 3 && o.level != 0 {
        return Err(px("Unsupported compression algorithm"));
    }
    if o.compress != 0 && o.level != 0 {
        let level = o.level as u8;
        let mut c = vec![o.compress];
        c.extend(match o.compress {
            1 => miniz_oxide::deflate::compress_to_vec(&inner, level),
            _ => miniz_oxide::deflate::compress_to_vec_zlib(&inner, level),
        });
        inner = packet(8, &c);
    }
    // the prefix (a random block, its last two bytes again), the data, and the MDC
    let c = block(data_cipher, &sess)?;
    let bs = c.size();
    let mut plain = random(bs)?;
    plain.extend_from_within(bs - 2..bs);
    plain.extend(inner);
    if o.disable_mdc {
        out.extend(packet(9, &Cfb::new(c, true).process(&plain, true)));
    } else {
        plain.extend_from_slice(&[0xd3, 0x14]);
        let mdc = Hash::Sha1.digest(&plain);
        plain.extend(mdc);
        let mut body = vec![1];
        body.extend(Cfb::new(c, false).process(&plain, true));
        out.extend(packet(18, &body));
    }
    funcs::fits(Some(out.len()))?;
    Ok(out)
}

// ---------------------------------------------------------------- decrypt

/// What decrypting found that pgcrypto reports only when nothing else went wrong (so as not to say more
/// about a wrong key than that).
struct State {
    text: bool,
    convert_crlf: bool,
    corrupt_prefix: bool,
    unexpected_binary: bool,
    unsupported_compr: bool,
}

pub(in super::super) fn decrypt(msg: &[u8], psw: &[u8], text: bool, o: &Options) -> Result<Vec<u8>> {
    let mut st = State {
        text,
        convert_crlf: o.convert_crlf,
        corrupt_prefix: false,
        unexpected_binary: false,
        unsupported_compr: false,
    };
    let (mut at, mut key, mut got_data, mut out) = (0, None, false, vec![]);
    while let Some((tag, len)) = header(msg, &mut at, false)? {
        let b = chunks(msg, &mut at, len)?;
        match tag {
            10 => {} // marker
            1 => return Err(px("pgp_sym_decrypt can't read a message encrypted with a public key")),
            3 if key.is_none() => key = Some(session_key(&b.concat(), psw)?),
            9 | 18 if !got_data => {
                let Some((cipher, k)) = &key else { return corrupt() };
                got_data = true;
                data(&mut st, tag, &b, block(*cipher, k)?, &mut out)?;
            }
            _ => return corrupt(),
        }
    }
    if !got_data || st.corrupt_prefix {
        return corrupt();
    }
    if st.unsupported_compr {
        return Err(px("Unsupported compression algorithm"));
    }
    if st.unexpected_binary {
        return Err(px("Not text data"));
    }
    Ok(out)
}

/// A symmetric-key session key packet: the cipher and key of the data.
fn session_key(b: &[u8], psw: &[u8]) -> Result<(u8, Vec<u8>)> {
    let mut at = 0;
    let (ver, cipher) = (byte(b, &mut at)?, byte(b, &mut at)?);
    if ver != 4 {
        return corrupt();
    }
    let (mode, digest) = (byte(b, &mut at)?, byte(b, &mut at)?);
    let (salt, count) = match mode {
        0 => (vec![], 0),
        1 | 3 => {
            let Some(salt) = b.get(at..at + 8) else { return corrupt() };
            at += 8;
            let count = if mode == 3 { decode_count(byte(b, &mut at)?) } else { 0 };
            (salt.to_vec(), count)
        }
        _ => return Err(px("Bad S2K mode")),
    };
    if key_size(cipher) == 0 {
        return Err(px("Unsupported cipher algorithm"));
    }
    // (a digest it doesn't know reads as a wrong key, as pgcrypto has it)
    if hash(digest).is_err() {
        return corrupt();
    }
    let key = s2k(mode, digest, &salt, count, psw, key_size(cipher))?;
    let rest = &b[at..b.len().min(at + 34)];
    if rest.is_empty() {
        return Ok((cipher, key));
    }
    if rest.len() < 17 || rest.len() > 33 {
        return corrupt();
    }
    let mut cfb = Cfb::new(block(cipher, &key)?, false);
    let plain = cfb.process(rest, false);
    if key_size(plain[0]) != plain.len() - 1 {
        return corrupt();
    }
    Ok((plain[0], plain[1..].to_vec()))
}

/// A SED (9) or SEIPD (18) packet's contents (its chunks), decrypted, their literal data added to `out`.
fn data(st: &mut State, tag: u8, b: &[&[u8]], c: Block, out: &mut Vec<u8>) -> Result<()> {
    let bs = c.size();
    let mut cfb = Cfb::new(c, tag == 9);
    let mut plain = Vec::with_capacity(b.iter().map(|c| c.len()).sum());
    // (a SEIPD packet starts with its version, 1)
    let mut version = tag == 18;
    for &chunk in b {
        stopped()?;
        let chunk = match chunk.split_first() {
            Some((&v, rest)) if version => {
                if v != 1 {
                    return corrupt();
                }
                version = false;
                rest
            }
            _ => chunk,
        };
        cfb.process_into(chunk, false, &mut plain);
    }
    if version || plain.len() < bs + 2 {
        return corrupt();
    }
    // the quick check: a wrong key shows here, reported last
    if plain[bs - 2..bs] != plain[bs..bs + 2] {
        st.corrupt_prefix = true;
    }
    let mdc = (tag == 18).then_some(&plain[..]);
    packets(st, &plain[bs + 2..], bs + 2, mdc, out)
}

/// The packets inside data (`d`, at `base` in the decrypted `mdc` when it has an MDC): literal data, or
/// compressed packets of it, and the MDC last.
fn packets(st: &mut State, d: &[u8], base: usize, mdc: Option<&[u8]>, out: &mut Vec<u8>) -> Result<()> {
    let (mut at, mut got_data, mut got_mdc) = (0, false, false);
    while let Some((tag, len)) = header(d, &mut at, true)? {
        if got_mdc {
            return corrupt();
        }
        let b = match (len, mdc) {
            // to the end, but for the MDC packet there, which ends the data
            (Len::Context, Some(full)) => {
                if d.len() - at < 22 {
                    return corrupt();
                }
                let end = d.len() - 22;
                if d[end] != 0xd3 || d[end + 1] != 0x14 {
                    return corrupt();
                }
                check_mdc(full, base + end + 2, &d[end + 2..])?;
                got_mdc = true;
                let b = vec![&d[at..end]];
                at = d.len();
                b
            }
            (len @ Len::Normal(n), Some(full)) if tag == 19 => {
                if n != 20 {
                    return corrupt();
                }
                let b = body(d, &mut at, len)?;
                check_mdc(full, base + at - b.len(), &b)?;
                got_mdc = true;
                continue;
            }
            (len, _) => chunks(d, &mut at, len)?,
        };
        match tag {
            11 => {
                got_data = true;
                literal(st, &b, out)?;
            }
            8 if !got_data => {
                got_data = true;
                compressed_data(st, &b, out)?;
            }
            _ => return corrupt(),
        }
    }
    if !got_data || (mdc.is_some() && !got_mdc) {
        return corrupt();
    }
    Ok(())
}

/// An MDC packet's 20 bytes are the SHA-1 of what was decrypted before them (its header included): `full`
/// up to `upto`.
fn check_mdc(full: &[u8], upto: usize, sha1: &[u8]) -> Result<()> {
    if sha1.len() != 20 || Hash::Sha1.digest(&full[..upto]) != sha1 {
        return corrupt();
    }
    Ok(())
}

/// A literal data packet's header (type, name, date) at the start of its body: whether it's text.
fn literal_header(st: &mut State, b: &[u8]) -> Result<usize> {
    let (Some(&kind), Some(&name)) = (b.first(), b.get(1)) else { return corrupt() };
    let start = 2 + usize::from(name) + 4;
    if b.len() < start {
        return corrupt();
    }
    if st.text && kind != b't' && kind != b'u' {
        st.unexpected_binary = true;
    }
    Ok(start)
}

/// A literal data packet (its body's chunks): its data added to `out`.
fn literal(st: &mut State, b: &[&[u8]], out: &mut Vec<u8>) -> Result<()> {
    // (the header is in the first chunks: at most 262 bytes)
    let head: Vec<u8> = b.iter().flat_map(|c| c.iter()).take(262).copied().collect();
    let mut skip = literal_header(st, &head)?;
    let start = out.len();
    for &c in b {
        let n = skip.min(c.len());
        skip -= n;
        funcs::fits(Some(out.len() + c.len() - n))?;
        out.extend_from_slice(&c[n..]);
    }
    let end = out.len();
    let end = crlf(st, out, start, end);
    out.truncate(end);
    Ok(())
}

/// `out[start..end]` with each CR LF made LF, in place, when decrypting text with `convert-crlf`: where it ends now.
fn crlf(st: &State, out: &mut [u8], start: usize, end: usize) -> usize {
    if !(st.text && st.convert_crlf) {
        return end;
    }
    let mut w = start;
    for r in start..end {
        if !(out[r] == b'\r' && r + 1 < end && out[r + 1] == b'\n') {
            out[w] = out[r];
            w += 1;
        }
    }
    w
}

/// A compressed data packet (its body's chunks): what it holds, added to `out`.
fn compressed_data(st: &mut State, b: &[&[u8]], out: &mut Vec<u8>) -> Result<()> {
    let Some(&algo) = b.iter().find_map(|c| c.first()) else { return corrupt() };
    // the compressed bytes, after the algorithm's
    let mut first = true;
    let input = b.iter().map(|c| {
        if first && !c.is_empty() {
            first = false;
            &c[1..]
        } else {
            *c
        }
    });
    let start = out.len();
    match algo {
        0 => {
            for c in input {
                funcs::fits(Some(out.len() + c.len()))?;
                out.extend_from_slice(c);
            }
        }
        1 | 2 => inflate(input, algo == 2, out)?,
        3 => {
            st.unsupported_compr = true;
            return Ok(());
        }
        _ => return corrupt(),
    }
    unpack(st, out, start)
}

/// Inflates `input` (raw deflate, or zlib) onto the end of `out` a piece at a time, so statement_timeout and
/// cancel stop it and only what it holds takes memory.
fn inflate<'a>(input: impl Iterator<Item = &'a [u8]>, zlib: bool, out: &mut Vec<u8>) -> Result<()> {
    use miniz_oxide::inflate::stream::{InflateState, inflate};
    use miniz_oxide::{DataFormat, MZError, MZFlush, MZStatus};
    const STEP: usize = 1 << 16;
    let mut state = InflateState::new_boxed(if zlib { DataFormat::Zlib } else { DataFormat::Raw });
    let start = out.len();
    for mut c in input {
        loop {
            stopped()?;
            let at = out.len();
            funcs::fits(Some(at - start + STEP))?;
            out.resize(at + STEP, 0);
            let r = inflate(&mut state, c, &mut out[at..], MZFlush::None);
            out.truncate(at + r.bytes_written);
            c = &c[r.bytes_consumed..];
            match r.status {
                Ok(MZStatus::StreamEnd) => return Ok(()),
                Ok(_) | Err(MZError::Buf) => {}
                Err(_) => return corrupt(),
            }
            if r.bytes_written == 0 && (c.is_empty() || r.bytes_consumed == 0) {
                if !c.is_empty() {
                    return corrupt();
                }
                break; // the next chunk
            }
        }
    }
    // the stream never ended
    corrupt()
}

/// The packets compressed data held, inflated to `out[start..]`: literal data only, of definite lengths (as
/// pgcrypto reads them there). Their data is moved down over their headers in place, so the decompressed
/// message is in memory once.
fn unpack(st: &mut State, out: &mut Vec<u8>, start: usize) -> Result<()> {
    let (mut r, mut w, mut got_data) = (start, start, false);
    while let Some((tag, mut len)) = header(out, &mut r, false)? {
        if tag != 11 {
            return corrupt();
        }
        got_data = true;
        // the body's chunks together, from `body`
        let body = w;
        loop {
            let (n, more) = match len {
                Len::Normal(n) => (n, false),
                Len::Partial(n) => (n, true),
                Len::Context => return corrupt(),
            };
            let end = (r + n).min(out.len());
            out.copy_within(r..end, w);
            w += end - r;
            r = end;
            if !more {
                break;
            }
            len = new_len(out, &mut r)?;
        }
        let skip = literal_header(st, &out[body..w])?;
        out.copy_within(body + skip..w, body);
        w = crlf(st, out, body, w - skip);
    }
    out.truncate(w);
    if !got_data {
        return corrupt();
    }
    Ok(())
}

// ---------------------------------------------------------------- pgp_key_id

/// The key a message or key is for: `SYMKEY` for a password, the hex id of the public (sub)key it
/// encrypts to, `ANYKEY` for an anonymous one; as pgcrypto's pgp_get_keyid looks.
pub(in super::super) fn key_id(msg: &[u8]) -> Result<String> {
    let mut at = 0;
    let (mut main_key, mut pub_keys, mut pubenc, mut symenc) = (false, 0, 0, 0);
    let mut id = [0u8; 8];
    while let Some((tag, len)) = header(msg, &mut at, false)? {
        match tag {
            // data: stop here
            9 | 18 => break,
            5 | 6 => {
                // the main key is for signing
                if main_key {
                    return Err(px("Several keys given - pgcrypto does not handle keyring"));
                }
                main_key = true;
                body(msg, &mut at, len)?;
            }
            7 | 14 => {
                if let Some(k) = public_key_id(&body(msg, &mut at, len)?)? {
                    id = k;
                    pub_keys += 1;
                }
            }
            1 => {
                pubenc += 1;
                let b = body(msg, &mut at, len)?;
                match b.first() {
                    Some(3) if b.len() >= 9 => id.copy_from_slice(&b[1..9]),
                    Some(3) | None => return corrupt(),
                    // (pgcrypto returns -1 here, which it has no words for)
                    Some(_) => return Err(px("Bad error code")),
                }
            }
            3 => {
                symenc += 1;
                body(msg, &mut at, len)?;
            }
            2 | 10 | 12 | 13 | 17 | 61 => {
                body(msg, &mut at, len)?;
            }
            _ => return corrupt(),
        }
    }
    if pub_keys > 0 && pubenc > 0 {
        return corrupt();
    }
    if pub_keys > 1 || pubenc > 1 {
        return Err(px("Several keys given - pgcrypto does not handle keyring"));
    }
    Ok(if pub_keys > 0 || pubenc > 0 {
        if id == [0; 8] { "ANYKEY".into() } else { id.iter().map(|b| format!("{b:02X}")).collect() }
    } else if symenc > 0 {
        "SYMKEY".into()
    } else {
        return Err(px("No encryption key found"));
    })
}

/// A (sub)key packet's key id, if it's a key that encrypts (RSA or Elgamal): the last 8 bytes of the
/// SHA-1 of its public part (a v4 fingerprint).
fn public_key_id(b: &[u8]) -> Result<Option<[u8; 8]>> {
    let mut at = 0;
    if byte(b, &mut at)? != 4 {
        return Err(px("Only V4 key packets are supported"));
    }
    if b.len() < 5 {
        return corrupt();
    }
    at += 4;
    let algo = byte(b, &mut at)?;
    let (mpis, encrypts) = match algo {
        1 | 2 => (2, true),
        3 => (2, false),
        16 => (3, true),
        17 => (4, false),
        _ => return Err(px("Unknown public-key encryption algorithm")),
    };
    for _ in 0..mpis {
        let bits = usize::from(byte(b, &mut at)?) << 8 | usize::from(byte(b, &mut at)?);
        at += bits.div_ceil(8);
        if at > b.len() {
            return corrupt();
        }
    }
    let mut md = Md::new(Hash::Sha1);
    md.update(&[0x99, (at >> 8) as u8, at as u8]);
    md.update(&b[..at]);
    let h = md.finish();
    Ok(encrypts.then(|| h[12..20].try_into().unwrap_or([0; 8])))
}

// ---------------------------------------------------------------- armor

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// RFC 4880's CRC-24.
fn crc24(data: &[u8]) -> u32 {
    let mut crc = 0xb7_04ceu32;
    for &b in data {
        crc ^= u32::from(b) << 16;
        for _ in 0..8 {
            crc <<= 1;
            if crc & 0x100_0000 != 0 {
                crc ^= 0x186_4cfb;
            }
        }
    }
    crc & 0xff_ffff
}

/// armor(): a PGP MESSAGE block, its headers, base 64 in lines of 76, and the CRC-24.
pub(in super::super) fn armor(data: &[u8], headers: &[(String, String)]) -> String {
    let mut out = String::from("-----BEGIN PGP MESSAGE-----\n");
    for (k, v) in headers {
        out.push_str(&format!("{k}: {v}\n"));
    }
    out.push('\n');
    let mut line = 0;
    for chunk in data.chunks(3) {
        let n = chunk.iter().fold(0u32, |n, &b| n << 8 | u32::from(b)) << (8 * (3 - chunk.len()));
        for i in 0..4 {
            out.push(if i <= chunk.len() { B64[((n >> (18 - 6 * i)) & 0x3f) as usize] as char } else { '=' });
        }
        line += 4;
        if line >= 76 {
            out.push('\n');
            line = 0;
        }
    }
    if !out.ends_with('\n') {
        out.push('\n');
    }
    let crc = crc24(data);
    out.push('=');
    for i in 0..4 {
        out.push(B64[((crc >> (18 - 6 * i)) & 0x3f) as usize] as char);
    }
    out.push_str("\n-----END PGP MESSAGE-----\n");
    out
}

/// Where a `-----BEGIN` (or `-----END`) line starts in `d` from `from`, and where the line after it does:
/// at the start of a line, any text but control characters, then five dashes and the line's end.
fn armor_line(d: &[u8], from: usize, end: bool) -> Option<(usize, usize)> {
    let sep: &[u8] = if end { b"-----END" } else { b"-----BEGIN" };
    let mut p = from;
    let start = loop {
        let i = p + d.get(p..)?.windows(sep.len()).position(|w| w == sep)?;
        if i == from || d[i - 1] == b'\n' {
            break i;
        }
        p = i + sep.len();
    };
    let mut q = start + sep.len();
    while q < d.len() && d[q] != b'-' {
        if d[q] < b' ' {
            return None;
        }
        q += 1;
    }
    if d.get(q..q + 5) != Some(b"-----") {
        return None;
    }
    q += 5;
    if q < d.len() {
        if d[q] != b'\n' && d[q] != b'\r' {
            return None;
        }
        if d[q] == b'\r' {
            q += 1;
        }
        if q < d.len() && d[q] == b'\n' {
            q += 1;
        }
    }
    Some((start, q))
}

/// pgcrypto's base 64 reader: whitespace skipped, `=` only at the end of a group.
fn b64_decode(s: &[u8]) -> Option<Vec<u8>> {
    let (mut out, mut buf, mut pos, mut end) = (vec![], 0u32, 0, 0);
    for &c in s {
        let b = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => {
                if end == 0 {
                    end = match pos {
                        2 => 1,
                        3 => 2,
                        _ => return None,
                    };
                }
                0
            }
            b' ' | b'\t' | b'\n' | b'\r' => continue,
            _ => return None,
        };
        buf = (buf << 6) | u32::from(b);
        pos += 1;
        if pos == 4 {
            out.push((buf >> 16) as u8);
            if end == 0 || end > 1 {
                out.push((buf >> 8) as u8);
            }
            if end == 0 || end > 2 {
                out.push(buf as u8);
            }
            (buf, pos) = (0, 0);
        }
    }
    (pos == 0).then_some(out)
}

/// dearmor(): the bytes of the first armored block in `d`, headers skipped, its CRC-24 checked.
pub(in super::super) fn dearmor(d: &[u8]) -> Result<Vec<u8>> {
    let bad = || Err(px("Corrupt ascii-armor"));
    let Some((_, mut p)) = armor_line(d, 0, false) else { return bad() };
    let Some((armor_end, _)) = armor_line(d, p, true) else { return bad() };
    // the headers end at an empty line
    while p < armor_end && d[p] != b'\n' && d[p] != b'\r' {
        let Some(nl) = d[p..armor_end].iter().position(|&c| c == b'\n') else { return bad() };
        p += nl + 1;
    }
    let start = p;
    // the CRC: after the last `=`
    let Some(eq) = (start..=armor_end.min(d.len() - 1)).rev().find(|&i| d[i] == b'=') else { return bad() };
    let crc = match d.get(eq + 1..eq + 5).and_then(b64_decode) {
        Some(c) if c.len() == 3 => u32::from(c[0]) << 16 | u32::from(c[1]) << 8 | u32::from(c[2]),
        _ => return bad(),
    };
    // (the character before the `=` is left out: the line's end, in an armor written as it should be)
    let Some(data) = d.get(start..(eq - 1).max(start)).and_then(b64_decode) else { return bad() };
    if crc24(&data) != crc {
        return bad();
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What Postgres's armor() gives, and dearmor of it.
    #[test]
    fn armor_round_trip() {
        assert_eq!(
            armor(&[1, 2, 3, 4], &[]),
            "-----BEGIN PGP MESSAGE-----\n\nAQIDBA==\n=eHjN\n-----END PGP MESSAGE-----\n"
        );
        assert_eq!(armor(&[], &[]), "-----BEGIN PGP MESSAGE-----\n\n=twTO\n-----END PGP MESSAGE-----\n");
        for n in [0, 1, 2, 3, 56, 57, 58, 114, 1000] {
            let data: Vec<u8> = (0..n).map(|i| (i * 7) as u8).collect();
            assert_eq!(dearmor(armor(&data, &[]).as_bytes()).unwrap(), data, "{n}");
        }
    }

    /// The S2K count byte and what it stands for (RFC 4880 3.7.1.3).
    #[test]
    fn s2k_counts() {
        assert_eq!(decode_count(96), 65536);
        assert_eq!(decode_count(255), 65_011_712);
        assert_eq!(decode_count(0), 1024);
    }
}
