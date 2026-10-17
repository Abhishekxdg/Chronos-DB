//! crypt() and gen_salt() as pgcrypto has them: bcrypt (`$2a$`, and `$2x$` with the old sign-extension
//! bug), FreeBSD's MD5 crypt (`$1$`), and DES crypt, traditional (two salt characters) and extended (`_`,
//! BSDi's), each giving the string Postgres gives for the same salt.

use super::super::{Result, err};
use md5::{Digest, Md5};

/// What a salt or hash is written in, for DES and MD5 crypt.
const A64: &[u8; 64] = b"./0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
/// bcrypt's.
const BF64: &[u8; 64] = b"./ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";

fn invalid_salt<T>() -> Result<T> {
    err("22023", "invalid salt")
}

/// What pgcrypto reports when the hashing itself gives nothing (an extended DES salt counting 0 rounds,
/// a `$2$` salt).
fn null<T>() -> Result<T> {
    err("39000", "crypt(3) returned NULL")
}

/// crypt(password, salt): the algorithm the salt's start names.
pub(super) fn crypt(pw: &[u8], salt: &[u8]) -> Result<String> {
    if salt.starts_with(b"$2a$") || salt.starts_with(b"$2x$") {
        bcrypt(pw, salt)
    } else if salt.starts_with(b"$2$") {
        null()
    } else if salt.starts_with(b"$1$") {
        md5_crypt(pw, &salt[3..])
    } else {
        des_crypt(pw, salt)
    }
}

// ---------------------------------------------------------------- bcrypt

/// bcrypt as pgcrypto's crypt-blowfish.c (Openwall's) has it: `$2a$NN$` and 22 characters of salt, NN from
/// 04 to 31; `$2x$` reads the password's bytes as signed, as the buggy implementations before 2011 did.
fn bcrypt(pw: &[u8], setting: &[u8]) -> Result<String> {
    let s = |i: usize| setting.get(i).copied().unwrap_or(0);
    if !(b'0'..=b'3').contains(&s(4)) || !s(5).is_ascii_digit() || (s(4) == b'3' && s(5) > b'1') || s(6) != b'$' {
        return invalid_salt();
    }
    let rounds = u32::from(s(4) - b'0') * 10 + u32::from(s(5) - b'0');
    if rounds < 4 {
        return invalid_salt();
    }
    let Some(salt) = bf_decode(setting.get(7..).unwrap_or_default(), 16) else { return invalid_salt() };
    let key = bf_key(pw, s(2) == b'x');
    let mut state = blowfish::Blowfish::bc_init_state();
    state.salted_expand_key(&salt, &key);
    for _ in 0..1u64 << rounds {
        // (each round is long enough to look every time: statement_timeout and cancel stop it)
        super::super::stopped()?;
        state.bc_expand_key(&key);
        state.bc_expand_key(&salt);
    }
    let mut words = [0u32; 6];
    for (w, chunk) in words.iter_mut().zip(b"OrpheanBeholderScryDoubt".chunks(4)) {
        *w = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
    }
    for pair in words.chunks_mut(2) {
        let mut lr = [pair[0], pair[1]];
        for _ in 0..64 {
            lr = state.bc_encrypt(lr);
        }
        pair.copy_from_slice(&lr);
    }
    let hash: Vec<u8> = words.iter().flat_map(|w| w.to_be_bytes()).collect();
    // the setting as given, its last salt character with only the bits that count
    let mut out = String::from_utf8_lossy(&setting[..7 + 21]).into_owned();
    out.push(BF64[usize::from(bf_index(setting[7 + 21]).unwrap_or(0) & 0x30)] as char);
    out.push_str(&bf_encode(&hash[..23]));
    Ok(out)
}

/// The 18 words of key bcrypt XORs into its state: the password's bytes and a zero, over and over.
fn bf_key(pw: &[u8], sign_extension_bug: bool) -> Vec<u8> {
    let mut stream = pw.iter().copied().chain([0]).cycle();
    let mut out = Vec::with_capacity(72);
    for _ in 0..18 {
        let mut w = 0u32;
        for _ in 0..4 {
            let b = stream.next().unwrap_or(0);
            let b = if sign_extension_bug { b as i8 as i32 as u32 } else { u32::from(b) };
            w = (w << 8) | b;
        }
        out.extend(w.to_be_bytes());
    }
    out
}

fn bf_index(c: u8) -> Option<u8> {
    BF64.iter().position(|&x| x == c).map(|i| i as u8)
}

/// `size` bytes from bcrypt's base 64 (only as many characters as they take); None at a character outside it.
fn bf_decode(src: &[u8], size: usize) -> Option<Vec<u8>> {
    let mut chars = src.iter().map(|&c| bf_index(c));
    let mut next = || chars.next().flatten();
    let mut out = Vec::with_capacity(size);
    while out.len() < size {
        let (c1, c2) = (next()?, next()?);
        out.push((c1 << 2) | ((c2 & 0x30) >> 4));
        if out.len() == size {
            break;
        }
        let c3 = next()?;
        out.push(((c2 & 0x0f) << 4) | ((c3 & 0x3c) >> 2));
        if out.len() == size {
            break;
        }
        let c4 = next()?;
        out.push(((c3 & 0x03) << 6) | c4);
    }
    Some(out)
}

fn bf_encode(src: &[u8]) -> String {
    let mut out = String::new();
    let ch = |i: u8| BF64[usize::from(i)] as char;
    for chunk in src.chunks(3) {
        out.push(ch(chunk[0] >> 2));
        let c1 = (chunk[0] & 0x03) << 4;
        let Some(&b) = chunk.get(1) else {
            out.push(ch(c1));
            break;
        };
        out.push(ch(c1 | (b >> 4)));
        let c1 = (b & 0x0f) << 2;
        let Some(&c) = chunk.get(2) else {
            out.push(ch(c1));
            break;
        };
        out.push(ch(c1 | (c >> 6)));
        out.push(ch(c & 0x3f));
    }
    out
}

// ---------------------------------------------------------------- MD5 crypt

/// `n` characters of `v`, six bits each from the lowest.
fn to64(out: &mut String, mut v: u32, n: usize) {
    for _ in 0..n {
        out.push(A64[(v & 0x3f) as usize] as char);
        v >>= 6;
    }
}

/// FreeBSD's MD5 crypt (Poul-Henning Kamp's): `$1$`, up to 8 characters of salt (to a `$`), 1000 rounds.
fn md5_crypt(pw: &[u8], rest: &[u8]) -> Result<String> {
    let salt = &rest[..rest.iter().take(8).position(|&c| c == b'$').unwrap_or(rest.len().min(8))];
    let mut alt = Md5::new();
    alt.update(pw);
    alt.update(salt);
    alt.update(pw);
    let alt = alt.finalize();
    let mut ctx = Md5::new();
    ctx.update(pw);
    ctx.update(b"$1$");
    ctx.update(salt);
    for chunk in (0..pw.len()).step_by(16) {
        ctx.update(&alt[..(pw.len() - chunk).min(16)]);
    }
    // (the bytes of `alt` were cleared by then: the odd bits add a zero)
    let mut i = pw.len();
    while i > 0 {
        ctx.update(if i & 1 == 1 { &[0u8][..] } else { &pw[..1] });
        i >>= 1;
    }
    let mut fin = ctx.finalize();
    for i in 0..1000 {
        // (a round hashes the password twice, however long it is: statement_timeout and cancel stop it)
        super::super::stopped()?;
        let mut c = Md5::new();
        if i & 1 == 1 {
            c.update(pw);
        } else {
            c.update(fin);
        }
        if i % 3 != 0 {
            c.update(salt);
        }
        if i % 7 != 0 {
            c.update(pw);
        }
        if i & 1 == 1 {
            c.update(fin);
        } else {
            c.update(pw);
        }
        fin = c.finalize();
    }
    let f = |i: usize| u32::from(fin[i]);
    let mut out = format!("$1${}$", String::from_utf8_lossy(salt));
    for (a, b, c) in [(0, 6, 12), (1, 7, 13), (2, 8, 14), (3, 9, 15), (4, 10, 5)] {
        to64(&mut out, (f(a) << 16) | (f(b) << 8) | f(c), 4);
    }
    to64(&mut out, f(11), 2);
    Ok(out)
}

// ---------------------------------------------------------------- DES crypt

/// A character of a DES salt or count as a number (FreeSec's ascii_to_bin): anything outside the alphabet
/// is 0, as `.` is.
fn ascii_to_bin(c: u8) -> u32 {
    match c {
        b'a'..=b'z' => u32::from(c - b'a') + 38,
        b'A'..=b'Z' => u32::from(c - b'A') + 12,
        b'.'..=b'9' => u32::from(c - b'.'),
        _ => 0,
    }
}

/// DES crypt as FreeSec's (pgcrypto's crypt-des.c): traditional, two salt characters and the first eight
/// characters of the password, 25 rounds; or extended (`_`, then four characters of round count and four of
/// salt), every character of the password folded in.
fn des_crypt(pw: &[u8], setting: &[u8]) -> Result<String> {
    // the key: each character shifted up a bit, eight of them
    let mut keybuf = [0u8; 8];
    for (k, c) in keybuf.iter_mut().zip(pw) {
        *k = c << 1;
    }
    let mut des = Des::new(u64::from_be_bytes(keybuf));
    let (count, salt, mut out) = if setting.first() == Some(&b'_') {
        if setting.len() < 9 {
            return invalid_salt();
        }
        let count = (1..5).fold(0, |n, i| n | ascii_to_bin(setting[i]) << ((i - 1) * 6));
        let salt = (5..9).fold(0, |n, i| n | ascii_to_bin(setting[i]) << ((i - 5) * 6));
        // the rest of the key: encrypted with itself, and the next eight characters XORed in
        let mut rest = pw.get(8..).unwrap_or_default();
        while !rest.is_empty() {
            let block = des.encrypt(u64::from_be_bytes(keybuf), 0, 1)?;
            keybuf = block.to_be_bytes();
            for (k, c) in keybuf.iter_mut().zip(rest) {
                *k ^= c << 1;
            }
            rest = rest.get(8..).unwrap_or_default();
            des = Des::new(u64::from_be_bytes(keybuf));
        }
        if count == 0 {
            return null();
        }
        (count, salt, String::from_utf8_lossy(&setting[..9]).into_owned())
    } else {
        if setting.len() < 2 {
            return invalid_salt();
        }
        let salt = ascii_to_bin(setting[1]) << 6 | ascii_to_bin(setting[0]);
        (25, salt, String::from_utf8_lossy(&setting[..2]).into_owned())
    };
    let block = des.encrypt(0, salt, count)?;
    let (r0, r1) = ((block >> 32) as u32, block as u32);
    let push = |out: &mut String, l: u32, n: u32| {
        for i in (0..n).rev() {
            out.push(A64[((l >> (6 * i)) & 0x3f) as usize] as char);
        }
    };
    push(&mut out, r0 >> 8, 4);
    push(&mut out, (r0 << 16) | ((r1 >> 16) & 0xffff), 4);
    push(&mut out, r1 << 2, 3);
    Ok(out)
}

/// DES with crypt's salt: each bit set in the salt swaps two bits of the expansion's output.
struct Des {
    /// the 16 round keys, 48 bits each
    keys: [u64; 16],
}

// The standard's tables (FIPS 46-3), bit 1 the leftmost.
const IP: [u8; 64] = [
    58, 50, 42, 34, 26, 18, 10, 2, 60, 52, 44, 36, 28, 20, 12, 4, 62, 54, 46, 38, 30, 22, 14, 6, 64, 56, 48, 40, 32,
    24, 16, 8, 57, 49, 41, 33, 25, 17, 9, 1, 59, 51, 43, 35, 27, 19, 11, 3, 61, 53, 45, 37, 29, 21, 13, 5, 63, 55, 47,
    39, 31, 23, 15, 7,
];
const FP: [u8; 64] = [
    40, 8, 48, 16, 56, 24, 64, 32, 39, 7, 47, 15, 55, 23, 63, 31, 38, 6, 46, 14, 54, 22, 62, 30, 37, 5, 45, 13, 53, 21,
    61, 29, 36, 4, 44, 12, 52, 20, 60, 28, 35, 3, 43, 11, 51, 19, 59, 27, 34, 2, 42, 10, 50, 18, 58, 26, 33, 1, 41, 9,
    49, 17, 57, 25,
];
const P: [u8; 32] = [
    16, 7, 20, 21, 29, 12, 28, 17, 1, 15, 23, 26, 5, 18, 31, 10, 2, 8, 24, 14, 32, 27, 3, 9, 19, 13, 30, 6, 22, 11, 4,
    25,
];
const PC1: [u8; 56] = [
    57, 49, 41, 33, 25, 17, 9, 1, 58, 50, 42, 34, 26, 18, 10, 2, 59, 51, 43, 35, 27, 19, 11, 3, 60, 52, 44, 36, 63, 55,
    47, 39, 31, 23, 15, 7, 62, 54, 46, 38, 30, 22, 14, 6, 61, 53, 45, 37, 29, 21, 13, 5, 28, 20, 12, 4,
];
const PC2: [u8; 48] = [
    14, 17, 11, 24, 1, 5, 3, 28, 15, 6, 21, 10, 23, 19, 12, 4, 26, 8, 16, 7, 27, 20, 13, 2, 41, 52, 31, 37, 47, 55, 30,
    40, 51, 45, 33, 48, 44, 49, 39, 56, 34, 53, 46, 42, 50, 36, 29, 32,
];
const SHIFTS: [u32; 16] = [1, 1, 2, 2, 2, 2, 2, 2, 1, 2, 2, 2, 2, 2, 2, 1];
const S: [[u8; 64]; 8] = [
    [
        14, 4, 13, 1, 2, 15, 11, 8, 3, 10, 6, 12, 5, 9, 0, 7, 0, 15, 7, 4, 14, 2, 13, 1, 10, 6, 12, 11, 9, 5, 3, 8, 4,
        1, 14, 8, 13, 6, 2, 11, 15, 12, 9, 7, 3, 10, 5, 0, 15, 12, 8, 2, 4, 9, 1, 7, 5, 11, 3, 14, 10, 0, 6, 13,
    ],
    [
        15, 1, 8, 14, 6, 11, 3, 4, 9, 7, 2, 13, 12, 0, 5, 10, 3, 13, 4, 7, 15, 2, 8, 14, 12, 0, 1, 10, 6, 9, 11, 5, 0,
        14, 7, 11, 10, 4, 13, 1, 5, 8, 12, 6, 9, 3, 2, 15, 13, 8, 10, 1, 3, 15, 4, 2, 11, 6, 7, 12, 0, 5, 14, 9,
    ],
    [
        10, 0, 9, 14, 6, 3, 15, 5, 1, 13, 12, 7, 11, 4, 2, 8, 13, 7, 0, 9, 3, 4, 6, 10, 2, 8, 5, 14, 12, 11, 15, 1, 13,
        6, 4, 9, 8, 15, 3, 0, 11, 1, 2, 12, 5, 10, 14, 7, 1, 10, 13, 0, 6, 9, 8, 7, 4, 15, 14, 3, 11, 5, 2, 12,
    ],
    [
        7, 13, 14, 3, 0, 6, 9, 10, 1, 2, 8, 5, 11, 12, 4, 15, 13, 8, 11, 5, 6, 15, 0, 3, 4, 7, 2, 12, 1, 10, 14, 9, 10,
        6, 9, 0, 12, 11, 7, 13, 15, 1, 3, 14, 5, 2, 8, 4, 3, 15, 0, 6, 10, 1, 13, 8, 9, 4, 5, 11, 12, 7, 2, 14,
    ],
    [
        2, 12, 4, 1, 7, 10, 11, 6, 8, 5, 3, 15, 13, 0, 14, 9, 14, 11, 2, 12, 4, 7, 13, 1, 5, 0, 15, 10, 3, 9, 8, 6, 4,
        2, 1, 11, 10, 13, 7, 8, 15, 9, 12, 5, 6, 3, 0, 14, 11, 8, 12, 7, 1, 14, 2, 13, 6, 15, 0, 9, 10, 4, 5, 3,
    ],
    [
        12, 1, 10, 15, 9, 2, 6, 8, 0, 13, 3, 4, 14, 7, 5, 11, 10, 15, 4, 2, 7, 12, 9, 5, 6, 1, 13, 14, 0, 11, 3, 8, 9,
        14, 15, 5, 2, 8, 12, 3, 7, 0, 4, 10, 1, 13, 11, 6, 4, 3, 2, 12, 9, 5, 15, 10, 11, 14, 1, 7, 6, 0, 8, 13,
    ],
    [
        4, 11, 2, 14, 15, 0, 8, 13, 3, 12, 9, 7, 5, 10, 6, 1, 13, 0, 11, 7, 4, 9, 1, 10, 14, 3, 5, 12, 2, 15, 8, 6, 1,
        4, 11, 13, 12, 3, 7, 14, 10, 15, 6, 8, 0, 5, 9, 2, 6, 11, 13, 8, 1, 4, 10, 7, 9, 5, 0, 15, 14, 2, 3, 12,
    ],
    [
        13, 2, 8, 4, 6, 15, 11, 1, 10, 9, 3, 14, 5, 0, 12, 7, 1, 15, 13, 8, 10, 3, 7, 4, 12, 5, 6, 11, 0, 14, 9, 2, 7,
        11, 4, 1, 9, 12, 14, 2, 0, 6, 10, 13, 15, 3, 5, 8, 2, 1, 14, 7, 4, 10, 8, 13, 15, 12, 9, 0, 3, 5, 6, 11,
    ],
];

/// The bits of `x` (`width` wide, bit 1 its highest) that `table` picks, in its order.
fn permute(x: u64, width: u32, table: &[u8]) -> u64 {
    table.iter().fold(0, |out, &bit| (out << 1) | ((x >> (width - u32::from(bit))) & 1))
}

/// Each S-box with the P permutation applied to its output: one lookup per box per round.
fn sp() -> &'static [[u32; 64]; 8] {
    static SP: std::sync::OnceLock<[[u32; 64]; 8]> = std::sync::OnceLock::new();
    SP.get_or_init(|| {
        let mut sp = [[0u32; 64]; 8];
        for (k, (box_, out)) in S.iter().zip(sp.iter_mut()).enumerate() {
            for (six, o) in out.iter_mut().enumerate() {
                let row = ((six >> 4) & 2) | (six & 1);
                let col = (six >> 1) & 0xf;
                *o = permute(u64::from(box_[row * 16 + col]) << (28 - 4 * k), 32, &P) as u32;
            }
        }
        sp
    })
}

impl Des {
    fn new(key: u64) -> Des {
        let cd = permute(key, 64, &PC1);
        let (mut c, mut d) = ((cd >> 28) as u32, (cd & 0x0fff_ffff) as u32);
        let mut keys = [0u64; 16];
        for (k, shift) in keys.iter_mut().zip(SHIFTS) {
            c = ((c << shift) | (c >> (28 - shift))) & 0x0fff_ffff;
            d = ((d << shift) | (d >> (28 - shift))) & 0x0fff_ffff;
            *k = permute((u64::from(c) << 28) | u64::from(d), 56, &PC2);
        }
        Des { keys }
    }

    /// `block` encrypted `count` times, with crypt's `salt` (24 bits; 0 is plain DES).
    fn encrypt(&self, block: u64, salt: u32, count: u32) -> Result<u64> {
        // salt bit i swaps bits i + 1 and i + 25 of the expansion (bit 1 its highest)
        let saltbits = (0..24).filter(|i| salt & (1 << i) != 0).fold(0u32, |s, i| s | 1 << (23 - i));
        let sp = sp();
        let x = permute(block, 64, &IP);
        let (mut l, mut r) = ((x >> 32) as u32, x as u32);
        for _ in 0..count {
            super::super::tick()?;
            for k in &self.keys {
                // E: eight groups of six bits, each four bits of R and a bit of each neighbour
                let s = (u64::from(r & 1) << 33) | (u64::from(r) << 1) | u64::from(r >> 31);
                let e = (0..8).fold(0u64, |e, g| (e << 6) | ((s >> (28 - 4 * g)) & 63));
                let (mut el, mut er) = ((e >> 24) as u32, (e & 0xff_ffff) as u32);
                let f = (el ^ er) & saltbits;
                el ^= f;
                er ^= f;
                let x = ((u64::from(el) << 24) | u64::from(er)) ^ k;
                let f = (0..8).fold(0u32, |f, b| f | sp[b][((x >> (42 - 6 * b)) & 63) as usize]);
                (l, r) = (r, l ^ f);
            }
            (l, r) = (r, l);
        }
        Ok(permute((u64::from(l) << 32) | u64::from(r), 64, &FP))
    }
}

// ---------------------------------------------------------------- gen_salt

/// gen_salt(kind, rounds): a salt for crypt, from `random` (16 bytes); rounds 0 is the kind's default.
/// None for a kind pgcrypto doesn't know.
pub(super) fn gen_salt(kind: &str, rounds: i64, random: &[u8; 16]) -> Option<Result<String>> {
    let bad = || err("22023", "gen_salt: Incorrect number of rounds");
    let r = |i: usize| u32::from(random[i]);
    Some(match kind.to_ascii_lowercase().as_str() {
        "des" if rounds != 0 && rounds != 25 => bad(),
        "des" => Ok([A64[usize::from(random[0] & 0x3f)], A64[usize::from(random[1] & 0x3f)]]
            .iter()
            .map(|&c| c as char)
            .collect()),
        "md5" if rounds != 0 && rounds != 1000 => bad(),
        "md5" => {
            let mut out = String::from("$1$");
            to64(&mut out, r(0) | r(1) << 8 | r(2) << 16, 4);
            to64(&mut out, r(3) | r(4) << 8 | r(5) << 16, 4);
            Ok(out)
        }
        "xdes" => {
            let count = if rounds == 0 { 725 } else { rounds };
            if !(1..=0xff_ffff).contains(&count) || count & 1 == 0 {
                return Some(bad());
            }
            let mut out = String::from("_");
            to64(&mut out, count as u32, 4);
            to64(&mut out, r(0) | r(1) << 8 | r(2) << 16, 4);
            Ok(out)
        }
        "bf" => {
            let count = if rounds == 0 { 6 } else { rounds };
            if !(4..=31).contains(&count) {
                return Some(bad());
            }
            Ok(format!("$2a${count:02}${}", bf_encode(random)))
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use des::cipher::{BlockCipherEncrypt, KeyInit, array::Array};

    /// With no salt, the DES here is the standard's: the `des` crate's, for keys and blocks of every kind.
    #[test]
    fn des_without_salt_is_des() {
        let mut x = 0x0123_4567_89ab_cdefu64;
        for _ in 0..200 {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let (key, block) = (x.rotate_left(17) ^ 0x5555, x);
            let mut want = Array::from(block.to_be_bytes());
            des::Des::new(&Array::from(key.to_be_bytes())).encrypt_block(&mut want);
            assert_eq!(Des::new(key).encrypt(block, 0, 1).unwrap().to_be_bytes(), want.0, "key {key:x}");
        }
    }

    /// Hashes from crypt(3) and OpenBSD's tests.
    #[test]
    fn known_hashes() {
        assert_eq!(crypt(b"test", b"aa").unwrap(), "aaqPiZY5xR5l.");
        assert_eq!(
            crypt(b"U*U", b"$2a$05$CCCCCCCCCCCCCCCCCCCCC.").unwrap(),
            "$2a$05$CCCCCCCCCCCCCCCCCCCCC.E5YPO9kmyuRGyh0XouQYb4YMJKvyOeW"
        );
        assert_eq!(
            crypt(b"", b"$2a$05$CCCCCCCCCCCCCCCCCCCCC.").unwrap(),
            "$2a$05$CCCCCCCCCCCCCCCCCCCCC.7uG0VCzI2bS7j6ymqJi9CdcdxiRTWNy"
        );
        assert_eq!(crypt(b"Hello world!", b"$1$saltstring").unwrap(), "$1$saltstri$YMyguxXMBpd2TEZ.vS/3q1");
    }
}
