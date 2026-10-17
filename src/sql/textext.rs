//! What three of Postgres's contrib extensions do to text, as plain functions over `&str`: pg_trgm
//! (`show_trgm`, `similarity`, `word_similarity`, `strict_word_similarity` and the `%` family of
//! operators), fuzzystrmatch (`levenshtein`, `soundex`, `difference`, `metaphone`, `dmetaphone`,
//! `daitch_mokotoff`) and unaccent (`unaccent(text)` with the default rules).
//!
//! Each is the extension's own algorithm, step for step, so the answers match a real Postgres 16 to
//! the last bit, `f32` rounding included (pg_trgm computes in `float4`; the caller prints it). The
//! oracle was a server on a UTF8 database with the `C.UTF-8` locale, and the parts that depend on
//! the locale follow it: which characters are letters (`iswalnum`) and how they lower-case
//! (`towlower`) come from tables of what that server answered (`textext/ctype.rs`), and the ASCII
//! functions treat every byte at or above 0x80 as not a letter, as `isalpha` does there.
//!
//! Nothing here touches SQL (the functions and operators that call it are in funcs.rs): an error comes back as
//! the SQLSTATE and message Postgres raises with.
//! ponytail: the tables are the `C.UTF-8` ones. A database with the plain `C` locale counts only
//! ASCII letters as letters and lower-cases only ASCII, and another locale would differ again; the
//! `*_threshold` arguments stand in for the `pg_trgm.*` settings.
//!
//! Memory limits: Postgres cannot hand out a chunk of 1 GB or more (`MaxAllocSize`), and pg_trgm and
//! unaccent reach that with very long texts (pg_trgm from about 130 MB, unaccent when the result is 1 GB).
//! Both fail at the same lengths as Postgres, with its SQLSTATE and message: "out of memory" (54000) where
//! the code checks a size itself or a string buffer cannot grow, "invalid memory alloc request size N"
//! (XX000, not 54000) where `palloc` refuses the request.
//! ponytail: not modelled are the server running out of RAM for a text just under a limit, and the DETAIL
//! line of unaccent's 54000 ("Cannot enlarge string buffer containing N bytes by M more bytes.").

mod ctype;
mod daitch;
mod dmetaphone;
mod unaccent_rules;

use std::cmp::Ordering;

/// A Postgres error: the SQLSTATE and the message (what `err` takes).
pub(super) type Fail = (&'static str, String);

fn fail<T>(code: &'static str, message: impl Into<String>) -> Result<T, Fail> {
    Err((code, message.into()))
}

const INVALID_PARAMETER: &str = "22023"; // invalid_parameter_value
const ZERO_LENGTH: &str = "2200F"; // zero_length_character_string
const PROGRAM_LIMIT: &str = "54000"; // program_limit_exceeded
const INTERNAL_ERROR: &str = "XX000"; // internal_error

// ---- memory limits ----

/// The largest request `palloc` grants, 1 GB less a byte (`MaxAllocSize`).
const MAX_ALLOC_SIZE: u64 = 0x3fff_ffff;

/// The header of a varlena (`VARHDRSZ`).
const VARHDRSZ: u64 = 4;

/// `palloc(size)` on a server whose largest chunk is `max_alloc`: only its refusal is modelled,
/// "invalid memory alloc request size N" with N the size asked for, whatever the SQLSTATE's name.
fn palloc_up_to(size: u64, max_alloc: u64) -> Result<(), Fail> {
    if size > max_alloc {
        return fail(INTERNAL_ERROR, format!("invalid memory alloc request size {size}"));
    }
    Ok(())
}

fn palloc(size: u64) -> Result<(), Fail> {
    palloc_up_to(size, MAX_ALLOC_SIZE)
}

// ---- the locale: which characters are letters, and how they lower-case ----

/// `iswalnum` for a character (a byte below 0x80 goes through `isalnum`, which is ASCII only).
fn is_word_char(c: char) -> bool {
    if c.is_ascii() {
        return c.is_ascii_alphanumeric();
    }
    let cp = c as u32;
    ctype::WORD_RANGES
        .binary_search_by(|&(lo, hi)| {
            if cp < lo {
                Ordering::Greater
            } else if cp > hi {
                Ordering::Less
            } else {
                Ordering::Equal
            }
        })
        .is_ok()
}

/// `towlower` for a character: always one character for one, never a string (`İ` goes to `i`).
fn lower_char(c: char) -> char {
    if c.is_ascii() {
        return c.to_ascii_lowercase();
    }
    let cp = c as u32;
    let i = ctype::LOWER_RUNS.partition_point(|&(first, ..)| first <= cp);
    if i == 0 {
        return c;
    }
    let (first, last, step, delta) = ctype::LOWER_RUNS[i - 1];
    if cp <= last && (cp - first).is_multiple_of(step) {
        char::from_u32((i64::from(cp) + i64::from(delta)) as u32).unwrap_or(c)
    } else {
        c
    }
}

// ---- pg_trgm ----

/// `pg_trgm.similarity_threshold` and its two siblings at their defaults. Postgres stores them as
/// doubles set from a `float4` constant, so 0.3 is really 0.30000001192...; a `SET` replaces that
/// with the exact double that was typed.
pub(super) const SIMILARITY_THRESHOLD: f64 = 0.3f32 as f64;
pub(super) const WORD_SIMILARITY_THRESHOLD: f64 = 0.6f32 as f64;
pub(super) const STRICT_WORD_SIMILARITY_THRESHOLD: f64 = 0.5f32 as f64;

/// A trigram is three bytes: the three characters themselves when all three are one byte, else the
/// low three bytes of the CRC-32 of their UTF-8.
type Trgm = [u8; 3];

/// Orders trigrams as pg_trgm does, byte by byte as *signed* chars: a hashed trigram whose first byte
/// has the top bit set sorts before every plain one.
fn trgm_cmp(a: &Trgm, b: &Trgm) -> Ordering {
    let key = |t: &Trgm| [t[0] as i8, t[1] as i8, t[2] as i8];
    key(a).cmp(&key(b))
}

/// Which side of a word a trigram is on (strict word similarity only counts whole words).
const BOUND_LEFT: u8 = 1;
const BOUND_RIGHT: u8 = 2;

/// The lookup table of the Ethernet CRC-32 (reflected 0xEDB88320), as Postgres has it.
const CRC_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut bit = 0;
        while bit < 8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            bit += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
};

/// Postgres's legacy CRC-32, which pg_trgm uses to squeeze a trigram with a multibyte character down
/// to three bytes. It is not the CRC-32 of zlib: it runs the table the other way round, taking the
/// index from the top byte of the register and shifting left ("normal" table, "reflected" code).
pub(super) fn legacy_crc32(bytes: &[u8]) -> u32 {
    let mut c = 0xFFFF_FFFFu32;
    for &b in bytes {
        c = CRC_TABLE[(((c >> 24) ^ u32::from(b)) & 0xFF) as usize] ^ (c << 8);
    }
    !c
}

/// The trigram of three characters (`compact_trigram`).
fn compact_trigram(window: &[char]) -> Trgm {
    if window.iter().all(char::is_ascii) {
        return [window[0] as u8, window[1] as u8, window[2] as u8];
    }
    let mut bytes = Vec::with_capacity(12);
    let mut buf = [0u8; 4];
    for c in window {
        bytes.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
    }
    // the first three bytes of the CRC as it sits in memory, which is its low three on little-endian
    let crc = legacy_crc32(&bytes);
    [crc as u8, (crc >> 8) as u8, (crc >> 16) as u8]
}

/// Every word's trigrams in order of appearance, not sorted and not made unique (`generate_trgm_only`),
/// with which trigrams start and end a word. A word is a run of letters and digits; it is lower-cased
/// and padded with two blanks in front and one behind, and each three characters in a row make a
/// trigram, so a word of n characters has n + 1. Anything else only ends a word.
fn trgm_only(s: &str) -> (Vec<Trgm>, Vec<u8>) {
    let chars: Vec<char> = s.chars().collect();
    let (mut out, mut bounds) = (vec![], vec![]);
    let mut i = 0;
    while i < chars.len() {
        if !is_word_char(chars[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len() && is_word_char(chars[i]) {
            i += 1;
        }
        let mut padded = vec![' ', ' '];
        padded.extend(chars[start..i].iter().map(|&c| lower_char(c)));
        padded.push(' ');
        let first = out.len();
        out.extend(padded.windows(3).map(compact_trigram));
        bounds.resize(out.len(), 0);
        bounds[first] |= BOUND_LEFT;
        bounds[out.len() - 1] |= BOUND_RIGHT;
    }
    (out, bounds)
}

/// How many trigrams `trgm_only` gives, without making them: a word of n characters has n + 1.
fn trgm_count(s: &str) -> usize {
    let (mut count, mut in_word) = (0, false);
    for c in s.chars() {
        if is_word_char(c) {
            count += if in_word { 1 } else { 2 };
            in_word = true;
        } else {
            in_word = false;
        }
    }
    count
}

/// `sizeof(trgm)`, and `sizeof(pos_trgm)`: a trigram and the `int` that says where it came from, padded.
const TRGM_SIZE: u64 = 3;
const POS_TRGM_SIZE: u64 = 8;
/// The widest character of the database encoding, UTF-8 (`pg_database_encoding_max_length`).
const MAX_CHAR_BYTES: u64 = 4;

/// `protect_out_of_mem`: error 54000 "out of memory" when a text of `slen` bytes (the total, for two texts) could
/// need more than a chunk: its trigrams at three for every two bytes (the first test), or its case-folded copy
/// at the widest character for every byte (the second, which for UTF-8 the first always trips before).
fn protect_out_of_mem(slen: usize) -> Result<(), Fail> {
    let slen = slen as u64;
    if slen / 2 >= MAX_ALLOC_SIZE / (TRGM_SIZE * 3) || slen >= MAX_ALLOC_SIZE / MAX_CHAR_BYTES {
        return fail(PROGRAM_LIMIT, "out of memory");
    }
    Ok(())
}

/// What `generate_trgm` asks of the allocator before it reads a text of `slen` bytes: the check above, then
/// the array for as many trigrams as the text could have, with its header. The array is what fails for a
/// text of 238,609,292 or 238,609,293 bytes (XX000); from 238,609,294 the check does (54000).
fn generate_trgm_alloc(slen: usize) -> Result<(), Fail> {
    protect_out_of_mem(slen)?;
    palloc(VARHDRSZ + 1 + TRGM_SIZE * (slen as u64 / 2 + 1) * 3)
}

/// What word similarity asks of the allocator before it reads the texts (`calc_word_similarity`): the check on
/// both lengths together, then the sorted array of both texts' trigrams (`make_positional_trgm`), which fails
/// from 134,217,728 trigrams. The two arrays of each text's own trigrams come first in Postgres, and fit
/// whenever the check passes.
fn word_similarity_alloc(a: &str, b: &str) -> Result<(), Fail> {
    protect_out_of_mem(a.len() + b.len())?;
    positional_trgm_alloc(trgm_count(a) + trgm_count(b))
}

/// The array `make_positional_trgm` sorts, one entry for each trigram of both texts.
fn positional_trgm_alloc(trigrams: usize) -> Result<(), Fail> {
    palloc(POS_TRGM_SIZE * trigrams as u64)
}

/// The sorted set of a text's trigrams (`generate_trgm`).
fn trgm_set(s: &str) -> Vec<Trgm> {
    let mut v = trgm_only(s).0;
    v.sort_by(trgm_cmp);
    v.dedup();
    v
}

/// `show_trgm(text)`: the trigram set as Postgres prints its elements. A trigram made of letters,
/// digits and blanks is shown as itself, any other (one that was hashed) as `0x` and six hex digits.
/// A text of 238,609,292 bytes or more is refused (see `generate_trgm_alloc`).
pub(super) fn show_trgm(s: &str) -> Result<Vec<String>, Fail> {
    generate_trgm_alloc(s.len())?;
    Ok(trgm_set(s)
        .iter()
        .map(|t| {
            if t.iter().all(|&b| b.is_ascii_alphanumeric() || b == b' ') {
                t.iter().map(|&b| b as char).collect()
            } else {
                format!("0x{:06x}", u32::from(t[0]) << 16 | u32::from(t[1]) << 8 | u32::from(t[2]))
            }
        })
        .collect())
}

/// count / (len1 + len2 - count), in `float4` like Postgres (`CALCSML`).
fn calc_sml(count: i32, len1: i32, len2: i32) -> f32 {
    count as f32 / (len1 + len2 - count) as f32
}

/// `similarity(a, b)`: the trigrams the two share over the trigrams in either (`cnt_sml`). A text of
/// 238,609,292 bytes or more is refused (see `generate_trgm_alloc`); Postgres makes `a`'s set first, so
/// when both are too long it is `a`'s error that comes.
pub(super) fn similarity(a: &str, b: &str) -> Result<f32, Fail> {
    generate_trgm_alloc(a.len())?;
    generate_trgm_alloc(b.len())?;
    let (t1, t2) = (trgm_set(a), trgm_set(b));
    if t1.is_empty() || t2.is_empty() {
        return Ok(0.0);
    }
    let (mut i, mut j, mut count) = (0, 0, 0);
    while i < t1.len() && j < t2.len() {
        match trgm_cmp(&t1[i], &t2[j]) {
            Ordering::Less => i += 1,
            Ordering::Greater => j += 1,
            Ordering::Equal => {
                i += 1;
                j += 1;
                count += 1;
            }
        }
    }
    Ok(calc_sml(count, t1.len() as i32, t2.len() as i32))
}

/// `similarity_dist`, the `<->` operator: 1 - similarity. (Postgres subtracts in double and stores
/// a `float4`; that is the same number as subtracting in `float4`.)
pub(super) fn similarity_dist(a: &str, b: &str) -> Result<f32, Fail> {
    similarity(a, b).map(|s| (1.0 - f64::from(s)) as f32)
}

/// `a % b`: similarity at or above the threshold (`pg_trgm.similarity_threshold`, 0.3 unless set).
pub(super) fn similarity_op(a: &str, b: &str, threshold: f64) -> Result<bool, Fail> {
    similarity(a, b).map(|s| f64::from(s) >= threshold)
}

/// Flags for the word similarity search.
const WORD_CHECK_ONLY: u8 = 1; // only say whether the threshold is reached: stop as soon as it is
const WORD_STRICT: u8 = 2; // the extent must start and end on word bounds

/// The search over the trigrams of the text for the extent that is most like the pattern
/// (`iterate_word_similarity`). `trg2indexes[i]` is the number of the i-th text trigram among the
/// distinct trigrams, `found` says which of those the pattern has, `ulen1` counts the pattern's
/// distinct trigrams. The extent is a run of text trigrams [lower, upper]; moving `upper` along, the
/// best `lower` for it is kept, and the answer is the best ratio of trigrams the extent shares with
/// the pattern to those in either.
fn iterate_word_similarity(
    trg2indexes: &[usize],
    found: &[bool],
    ulen1: i32,
    flags: u8,
    bounds: &[u8],
    threshold: f64,
) -> f32 {
    let strict = flags & WORD_STRICT != 0;
    let check_only = flags & WORD_CHECK_ONLY != 0;
    let (mut ulen2, mut count) = (0i32, 0i32);
    // a strict extent starts at the first trigram; a plain one at the first trigram the pattern has
    let mut lower: i64 = if strict { 0 } else { -1 };
    let mut smlr_max = 0.0f32;
    // where each distinct trigram was last seen inside the extent
    let mut lastpos = vec![-1i64; found.len()];
    for (i, &trgindex) in trg2indexes.iter().enumerate() {
        if lower >= 0 || found[trgindex] {
            if lastpos[trgindex] < 0 {
                ulen2 += 1;
                if found[trgindex] {
                    count += 1;
                }
            }
            lastpos[trgindex] = i as i64;
        }
        // the extent can end here: on a word's last trigram (strict), or on one the pattern has
        let ends_here = if strict { bounds[i] & BOUND_RIGHT != 0 } else { found[trgindex] };
        if !ends_here {
            continue;
        }
        let upper = i as i64;
        if lower == -1 {
            lower = i as i64;
            ulen2 = 1;
        }
        let mut smlr_cur = calc_sml(count, ulen1, ulen2);
        // try moving the start forward for a better ratio
        let (mut tmp_count, mut tmp_ulen2, prev_lower) = (count, ulen2, lower);
        let mut tmp_lower = lower;
        while tmp_lower <= upper {
            let t = tmp_lower as usize;
            if !strict || bounds[t] & BOUND_LEFT != 0 {
                let smlr_tmp = calc_sml(tmp_count, ulen1, tmp_ulen2);
                if smlr_tmp > smlr_cur {
                    smlr_cur = smlr_tmp;
                    ulen2 = tmp_ulen2;
                    lower = tmp_lower;
                    count = tmp_count;
                }
                if check_only && f64::from(smlr_cur) >= threshold {
                    break;
                }
            }
            let tmp_trgindex = trg2indexes[t];
            if lastpos[tmp_trgindex] == tmp_lower {
                tmp_ulen2 -= 1;
                if found[tmp_trgindex] {
                    tmp_count -= 1;
                }
            }
            tmp_lower += 1;
        }
        // C's `Max`: the first unless the second is bigger
        smlr_max = if smlr_max > smlr_cur { smlr_max } else { smlr_cur };
        if check_only && f64::from(smlr_max) >= threshold {
            break;
        }
        // trigrams the start moved past are no longer in the extent
        for t in prev_lower..lower {
            let tmp_trgindex = trg2indexes[t as usize];
            if lastpos[tmp_trgindex] == t {
                lastpos[tmp_trgindex] = -1;
            }
        }
    }
    smlr_max
}

/// How much of `b` a word-like stretch of it is like `a` (`calc_word_similarity`): the pattern's
/// trigrams against the text's, numbered by sorting both together. Two texts of 238,609,294 bytes together,
/// or with 134,217,728 trigrams together, are refused (see `word_similarity_alloc`), either way round.
fn calc_word_similarity(a: &str, b: &str, flags: u8, threshold: f64) -> Result<f32, Fail> {
    word_similarity_alloc(a, b)?;
    let (trg1, _) = trgm_only(a);
    let (trg2, bounds) = trgm_only(b);
    let len = trg1.len() + trg2.len();
    // every trigram with where it came from: -1 for the pattern, its place for the text
    let mut ptrg: Vec<(Trgm, i64)> = trg1.iter().map(|&t| (t, -1)).collect();
    ptrg.extend(trg2.iter().enumerate().map(|(i, &t)| (t, i as i64)));
    ptrg.sort_by(|x, y| trgm_cmp(&x.0, &y.0).then(x.1.cmp(&y.1)));
    let mut trg2indexes = vec![0usize; trg2.len()];
    let mut found = vec![false; len];
    let (mut ulen1, mut j) = (0, 0);
    for i in 0..len {
        if i > 0 && trgm_cmp(&ptrg[i - 1].0, &ptrg[i].0) != Ordering::Equal {
            if found[j] {
                ulen1 += 1;
            }
            j += 1;
        }
        if ptrg[i].1 >= 0 {
            trg2indexes[ptrg[i].1 as usize] = j;
        } else {
            found[j] = true;
        }
    }
    if len > 0 && found[j] {
        ulen1 += 1;
    }
    Ok(iterate_word_similarity(&trg2indexes, &found, ulen1, flags, &bounds, threshold))
}

/// `word_similarity(a, b)`: the greatest similarity between the trigrams of `a` and those of any
/// stretch of words in `b`.
pub(super) fn word_similarity(a: &str, b: &str) -> Result<f32, Fail> {
    calc_word_similarity(a, b, 0, WORD_SIMILARITY_THRESHOLD)
}

/// `strict_word_similarity(a, b)`: as `word_similarity`, but the stretch has to be whole words.
pub(super) fn strict_word_similarity(a: &str, b: &str) -> Result<f32, Fail> {
    calc_word_similarity(a, b, WORD_STRICT, STRICT_WORD_SIMILARITY_THRESHOLD)
}

/// `a <% b`: `a` is similar to some stretch of `b`: word similarity at or above the threshold
/// (`pg_trgm.word_similarity_threshold`, 0.6 unless set).
pub(super) fn word_similarity_op(a: &str, b: &str, threshold: f64) -> Result<bool, Fail> {
    calc_word_similarity(a, b, WORD_CHECK_ONLY, threshold).map(|s| f64::from(s) >= threshold)
}

/// `a %> b`: `b <% a`.
pub(super) fn word_similarity_commutator_op(a: &str, b: &str, threshold: f64) -> Result<bool, Fail> {
    word_similarity_op(b, a, threshold)
}

/// `a <<% b`: strict word similarity at or above the threshold (`pg_trgm.strict_word_similarity_threshold`,
/// 0.5 unless set).
pub(super) fn strict_word_similarity_op(a: &str, b: &str, threshold: f64) -> Result<bool, Fail> {
    calc_word_similarity(a, b, WORD_CHECK_ONLY | WORD_STRICT, threshold).map(|s| f64::from(s) >= threshold)
}

/// `a %>> b`: `b <<% a`.
pub(super) fn strict_word_similarity_commutator_op(a: &str, b: &str, threshold: f64) -> Result<bool, Fail> {
    strict_word_similarity_op(b, a, threshold)
}

/// `a <<-> b`: 1 - `word_similarity(a, b)`.
pub(super) fn word_similarity_dist(a: &str, b: &str) -> Result<f32, Fail> {
    word_similarity(a, b).map(|s| (1.0 - f64::from(s)) as f32)
}

/// `a <->> b`: 1 - `word_similarity(b, a)`.
pub(super) fn word_similarity_dist_commutator(a: &str, b: &str) -> Result<f32, Fail> {
    word_similarity_dist(b, a)
}

/// `a <<<-> b`: 1 - `strict_word_similarity(a, b)`.
pub(super) fn strict_word_similarity_dist(a: &str, b: &str) -> Result<f32, Fail> {
    strict_word_similarity(a, b).map(|s| (1.0 - f64::from(s)) as f32)
}

/// `a <->>> b`: 1 - `strict_word_similarity(b, a)`.
pub(super) fn strict_word_similarity_dist_commutator(a: &str, b: &str) -> Result<f32, Fail> {
    strict_word_similarity_dist(b, a)
}

// ---- fuzzystrmatch: levenshtein ----

/// The most characters either string may have (the empty string is exempt: its distance is computed first).
const MAX_LEVENSHTEIN_STRLEN: usize = 255;

/// The Levenshtein distance with a cost for each insertion, deletion and substitution
/// (`varstr_levenshtein`), two rows of the matrix at a time. With `max_d` of 0 or more it is the
/// `_less_equal` variant: only the band of the matrix that could still lead to a distance of
/// `max_d` or less is filled in, and as soon as no cell in it can, the answer is `max_d + 1`. That
/// shortcut, and the value left behind when the band runs out, are Postgres's, so a distance above
/// `max_d` is the number Postgres gives and not just any number above it.
///
/// Everything is `int` arithmetic that wraps, as in the 16.13 server the answers were checked
/// against (later 16.x releases widen it to 64 bits and raise 22003 "levenshtein distance out of
/// range" instead): costs near 2^31 give what the C code gives, negative ones too. Two places where
/// Postgres does something no port can copy, and what is done here instead:
///
/// - The first row's stop column can wrap negative (costs of about 2^28 and up, positive ones included).
///   Postgres keeps it: the band is empty and the answer is `max_d + 1`. It also writes `max_d + 1` into
///   the row at that column, outside the row, which is undefined behaviour: harmless for a small column,
///   a segfault of the backend when it is far out (about -2^31). That write is skipped here, so a call
///   that crashes the server answers `max_d + 1`.
/// - With costs of about 2^30 and up the band can shrink to nothing without the cell that is returned
///   ever being written. Postgres then returns what was left in that memory by an earlier allocation: 0 in a
///   fresh backend, and other numbers later in a session, so there is nothing to match but the first
///   answer. The rows here start as zeros, which is that one.
fn levenshtein_impl(a: &str, b: &str, ins_c: i32, del_c: i32, sub_c: i32, max_d: i32) -> Result<i32, Fail> {
    let s: Vec<char> = a.chars().collect();
    let t: Vec<char> = b.chars().collect();
    let mut sub_c = sub_c;
    let mut max_d = max_d;
    let (m0, n0) = (s.len(), t.len());
    // an empty string turns into the other by insertions, or the other into it by deletions
    if m0 == 0 {
        return Ok((n0 as i32).wrapping_mul(ins_c));
    }
    if n0 == 0 {
        return Ok((m0 as i32).wrapping_mul(del_c));
    }
    if m0 > MAX_LEVENSHTEIN_STRLEN || n0 > MAX_LEVENSHTEIN_STRLEN {
        return fail(
            INVALID_PARAMETER,
            format!("levenshtein argument exceeds maximum length of {MAX_LEVENSHTEIN_STRLEN} characters"),
        );
    }
    let (m0, n0) = (m0 as i32, n0 as i32);
    let mut start_column = 0i32;
    let mut stop_column = m0 + 1;
    if max_d >= 0 {
        // is the bound impossibly tight? if not, is it tight enough to cut the work?
        let net_inserts = n0 - m0;
        let min_theo_d =
            if net_inserts < 0 { (-net_inserts).wrapping_mul(del_c) } else { net_inserts.wrapping_mul(ins_c) };
        if min_theo_d > max_d {
            return Ok(max_d.wrapping_add(1));
        }
        if ins_c.wrapping_add(del_c) < sub_c {
            sub_c = ins_c.wrapping_add(del_c);
        }
        let max_theo_d = min_theo_d.wrapping_add(sub_c.wrapping_mul(m0.min(n0)));
        if max_d >= max_theo_d {
            max_d = -1;
        } else if ins_c.wrapping_add(del_c) > 0 {
            // how much of the first row can matter
            let slack_d = max_d.wrapping_sub(min_theo_d);
            let best_column = if net_inserts < 0 { -net_inserts } else { 0 };
            stop_column = best_column.wrapping_add(slack_d / ins_c.wrapping_add(del_c)).wrapping_add(1);
            if stop_column > m0 {
                stop_column = m0 + 1;
            }
        }
    }
    // one more cell for the row and column of the empty prefix
    let (m, n) = (m0 + 1, n0 + 1);
    let (mut prev, mut curr) = (vec![0i32; m as usize], vec![0i32; m as usize]);
    for i in start_column..stop_column {
        prev[i as usize] = i.wrapping_mul(del_c);
    }
    // what it costs at least to get from a column to the one where what is left of each string is as
    // long as the other's, `net_inserts` columns away
    let extra = |net_inserts: i32| {
        if net_inserts > 0 { net_inserts.wrapping_mul(ins_c) } else { net_inserts.wrapping_neg().wrapping_mul(del_c) }
    };
    for j in 1..n {
        let y = t[(j - 1) as usize];
        // the best case is a match down the diagonal, so the band grows by a column unless it is full
        if stop_column < m {
            // a negative column is outside the row: Postgres writes there all the same (see above)
            if stop_column >= 0 {
                prev[stop_column as usize] = max_d.wrapping_add(1);
            }
            stop_column += 1;
        }
        let mut i;
        if start_column == 0 {
            curr[0] = j.wrapping_mul(ins_c);
            i = 1;
        } else {
            i = start_column;
        }
        while i < stop_column {
            let k = i as usize;
            let ins = prev[k].wrapping_add(ins_c);
            let del = curr[k - 1].wrapping_add(del_c);
            let sub = prev[k - 1].wrapping_add(if s[k - 1] == y { 0 } else { sub_c });
            curr[k] = ins.min(del).min(sub);
            i += 1;
        }
        std::mem::swap(&mut prev, &mut curr);
        if max_d >= 0 {
            // the column where what is left of each string is as long as the other's
            let zp = j - (n - m);
            // slide the stop column left, and the start column right, past cells that cannot reach max_d
            while stop_column > 0 {
                let ii = stop_column - 1;
                if prev[ii as usize].wrapping_add(extra(ii - zp)) <= max_d {
                    break;
                }
                stop_column -= 1;
            }
            while start_column < stop_column {
                if prev[start_column as usize].wrapping_add(extra(start_column - zp)) <= max_d {
                    break;
                }
                // never updated again, so leave nothing in them that a later row could trip on
                prev[start_column as usize] = max_d.wrapping_add(1);
                curr[start_column as usize] = max_d.wrapping_add(1);
                start_column += 1;
            }
            // if they cross, the bound is exceeded
            if start_column >= stop_column {
                return Ok(max_d.wrapping_add(1));
            }
        }
    }
    Ok(prev[(m - 1) as usize])
}

/// `levenshtein(a, b)`: the fewest single-character insertions, deletions and substitutions that turn
/// `a` into `b`. Characters are Unicode characters; more than 255 in either (unless the other is
/// empty) is error 22023.
pub(super) fn levenshtein(a: &str, b: &str) -> Result<i32, Fail> {
    levenshtein_impl(a, b, 1, 1, 1, -1)
}

/// `levenshtein(a, b, ins, del, sub)`: the same with a cost for each kind of edit.
pub(super) fn levenshtein_with_costs(a: &str, b: &str, ins_c: i32, del_c: i32, sub_c: i32) -> Result<i32, Fail> {
    levenshtein_impl(a, b, ins_c, del_c, sub_c, -1)
}

/// `levenshtein_less_equal(a, b, max_d)`: the distance if it is at most `max_d`, else some number
/// above `max_d` (`max_d + 1` when the search gives up early). A negative `max_d` means no bound.
pub(super) fn levenshtein_less_equal(a: &str, b: &str, max_d: i32) -> Result<i32, Fail> {
    levenshtein_impl(a, b, 1, 1, 1, max_d)
}

/// `levenshtein_less_equal(a, b, ins, del, sub, max_d)`.
pub(super) fn levenshtein_less_equal_with_costs(
    a: &str,
    b: &str,
    ins_c: i32,
    del_c: i32,
    sub_c: i32,
    max_d: i32,
) -> Result<i32, Fail> {
    levenshtein_impl(a, b, ins_c, del_c, sub_c, max_d)
}

// ---- fuzzystrmatch: soundex ----

const SOUNDEX_LEN: usize = 4;

/// The soundex digit of each letter A to Z.
const SOUNDEX_TABLE: &[u8; 26] = b"01230120022455012623010202";

/// A letter's soundex digit; anything that is not a letter stands for itself.
fn soundex_code(c: u8) -> u8 {
    let c = c.to_ascii_uppercase();
    if c.is_ascii_uppercase() { SOUNDEX_TABLE[usize::from(c - b'A')] } else { c }
}

/// The four bytes `_soundex` leaves: the first letter, then the digits of the letters after it, left
/// to right, dropping vowels and repeats of a digit, padded with zeros; all zero bytes if there is no
/// letter. Only ASCII letters are letters.
fn soundex_bytes(s: &str) -> [u8; SOUNDEX_LEN] {
    let s = s.as_bytes();
    let mut out = [0u8; SOUNDEX_LEN];
    let Some(first) = s.iter().position(u8::is_ascii_alphabetic) else {
        return out;
    };
    out[0] = s[first].to_ascii_uppercase();
    let mut count = 1;
    for k in first + 1..s.len() {
        if count >= SOUNDEX_LEN {
            break;
        }
        if s[k].is_ascii_alphabetic() && soundex_code(s[k]) != soundex_code(s[k - 1]) {
            let code = soundex_code(s[k]);
            if code != b'0' {
                out[count] = code;
                count += 1;
            }
        }
    }
    while count < SOUNDEX_LEN {
        out[count] = b'0';
        count += 1;
    }
    out
}

/// `soundex(text)`: the four-character code (a letter and three digits), or the empty string if
/// there is no letter in the text.
pub(super) fn soundex(s: &str) -> String {
    let code = soundex_bytes(s);
    if code[0] == 0 { String::new() } else { code.iter().map(|&b| b as char).collect() }
}

/// `difference(a, b)`: how many of the four places of their soundex codes agree, 0 to 4.
pub(super) fn difference(a: &str, b: &str) -> i32 {
    let (x, y) = (soundex_bytes(a), soundex_bytes(b));
    x.iter().zip(&y).filter(|(p, q)| p == q).count() as i32
}

// ---- fuzzystrmatch: metaphone ----

/// The longest input and the longest output `metaphone` takes.
const MAX_METAPHONE_STRLEN: usize = 255;

/// What kind of letter each of A to Z is: bit 0 a vowel, 1 passed through unchanged, 2 forms a
/// diphthong before H, 3 makes C and G soft, 4 keeps GH from becoming F.
const METAPHONE_CODES: [u8; 26] = [1, 16, 4, 16, 9, 2, 4, 16, 9, 2, 0, 2, 2, 2, 1, 4, 0, 2, 4, 4, 1, 0, 0, 0, 8, 0];

fn mp_code(c: u8) -> u8 {
    if c.is_ascii_alphabetic() { METAPHONE_CODES[usize::from(c.to_ascii_uppercase() - b'A')] } else { 0 }
}

fn mp_vowel(c: u8) -> bool {
    mp_code(c) & 1 != 0
}

/// The phonemes of the ASCII letters of `word`, at most `max` of them (`_metaphone`). Anything that
/// is not an ASCII letter (every byte of a multibyte character too) is ignored, though it still
/// counts as a place when looking back and ahead.
fn metaphone_core(word: &[u8], max: usize) -> String {
    // the upper-cased byte at a place, 0 past the end
    let at = |i: usize| word.get(i).map_or(0, u8::to_ascii_uppercase);
    let mut out = String::new();
    let mut w = 0;
    // the first letter is handled on its own
    while !at(w).is_ascii_alphabetic() {
        if at(w) == 0 {
            return out;
        }
        w += 1;
    }
    match at(w) {
        b'A' => {
            if at(w + 1) == b'E' {
                out.push('E');
                w += 2;
            } else {
                out.push('A');
                w += 1;
            }
        }
        b'G' | b'K' | b'P' if at(w + 1) == b'N' => {
            out.push('N');
            w += 2;
        }
        b'W' => {
            if at(w + 1) == b'H' || at(w + 1) == b'R' {
                out.push(at(w + 1) as char);
                w += 2;
            } else if mp_vowel(at(w + 1)) {
                out.push('W');
                w += 2;
            }
        }
        b'X' => {
            out.push('S');
            w += 1;
        }
        c @ (b'E' | b'I' | b'O' | b'U') => {
            out.push(c as char);
            w += 1;
        }
        _ => {}
    }
    while at(w) != 0 && out.len() < max {
        let cur = at(w);
        let next = at(w + 1);
        let after_next = if next != 0 { at(w + 2) } else { 0 };
        let back = |n: usize| if w >= n { at(w - n) } else { 0 };
        let prev = back(1);
        // letters an earlier rule already used up
        let mut skip = 0;
        // non-letters are ignored, and a repeated letter counts once (except CC)
        if cur.is_ascii_alphabetic() && !(cur == prev && cur != b'C') {
            match cur {
                // B unless it follows M
                b'B' if prev != b'M' => out.push('B'),
                // X (sh) for CIA and CH (but K for CHR and SCH), S for CI, CE and CY (dropped after S), else K
                b'C' => {
                    if mp_code(next) & 8 != 0 {
                        if after_next == b'A' && next == b'I' {
                            out.push('X');
                        } else if prev != b'S' {
                            out.push('S');
                        }
                    } else if next == b'H' {
                        out.push(if after_next == b'R' || prev == b'S' { 'K' } else { 'X' });
                        skip += 1;
                    } else {
                        out.push('K');
                    }
                }
                // J for DGE, DGI and DGY, else T
                b'D' => {
                    if next == b'G' && mp_code(after_next) & 8 != 0 {
                        out.push('J');
                        skip += 1;
                    } else {
                        out.push('T');
                    }
                }
                // F for GH (silent after B, D or H three back, or H four back), dropped in GN and GNED,
                // J for GE, GI and GY unless after G, else K
                b'G' => {
                    if next == b'H' {
                        if !(mp_code(back(3)) & 16 != 0 || back(4) == b'H') {
                            out.push('F');
                            skip += 1;
                        }
                    } else if next == b'N' {
                        if !after_next.is_ascii_alphabetic() || (after_next == b'E' && at(w + 3) == b'D') {
                            // dropped
                        } else {
                            out.push('K');
                        }
                    } else if mp_code(next) & 8 != 0 && prev != b'G' {
                        out.push('J');
                    } else {
                        out.push('K');
                    }
                }
                // H before a vowel unless after C, G, P, S or T
                b'H' if mp_vowel(next) && mp_code(prev) & 4 == 0 => out.push('H'),
                // K unless after C
                b'K' if prev != b'C' => out.push('K'),
                b'P' => out.push(if next == b'H' { 'F' } else { 'P' }),
                b'Q' => out.push('K'),
                // X (sh) for SIO, SIA, SH and SCHW, else S
                b'S' => {
                    if next == b'I' && (after_next == b'O' || after_next == b'A') {
                        out.push('X');
                    } else if next == b'H' {
                        out.push('X');
                        skip += 1;
                    } else if next == b'C' && at(w + 2) == b'H' && at(w + 3) == b'W' {
                        out.push('X');
                        skip += 2;
                    } else {
                        out.push('S');
                    }
                }
                // X (sh) for TIA and TIO, 0 (th) for TH, else T
                b'T' => {
                    if next == b'I' && (after_next == b'O' || after_next == b'A') {
                        out.push('X');
                    } else if next == b'H' {
                        out.push('0');
                        skip += 1;
                    } else {
                        out.push('T');
                    }
                }
                b'V' => out.push('F'),
                b'W' if mp_vowel(next) => out.push('W'),
                b'X' => {
                    out.push('K');
                    if out.len() < max {
                        out.push('S');
                    }
                }
                b'Y' if mp_vowel(next) => out.push('Y'),
                b'Z' => out.push('S'),
                b'F' | b'J' | b'L' | b'M' | b'N' | b'R' => out.push(cur as char),
                _ => {}
            }
        }
        w += 1 + skip;
    }
    out
}

/// `metaphone(text, max_output_length)`: the Metaphone code of the letters in the text, at most
/// `max_len` characters of it. The empty string gives the empty string whatever `max_len` is; else the
/// text may be at most 255 bytes and `max_len` from 1 to 255.
pub(super) fn metaphone(s: &str, max_len: i32) -> Result<String, Fail> {
    if s.is_empty() {
        return Ok(String::new());
    }
    if s.len() > MAX_METAPHONE_STRLEN {
        return fail(INVALID_PARAMETER, format!("argument exceeds the maximum length of {MAX_METAPHONE_STRLEN} bytes"));
    }
    if max_len > MAX_METAPHONE_STRLEN as i32 {
        return fail(INVALID_PARAMETER, format!("output exceeds the maximum length of {MAX_METAPHONE_STRLEN} bytes"));
    }
    if max_len <= 0 {
        return fail(ZERO_LENGTH, "output cannot be empty string");
    }
    Ok(metaphone_core(s.as_bytes(), max_len as usize))
}

/// `daitch_mokotoff(text)`: every Daitch-Mokotoff soundex code the name can have (six digits each;
/// a letter that can be sounded two ways gives two codes), or `None` (SQL NULL) when the text has no
/// letter the chart codes.
pub(super) fn daitch_mokotoff(s: &str) -> Option<Vec<String>> {
    daitch::daitch_mokotoff(s)
}

/// `dmetaphone(text)`: the primary Double Metaphone code.
pub(super) fn dmetaphone(s: &str) -> String {
    dmetaphone::double_metaphone(s).0
}

/// `dmetaphone_alt(text)`: the alternate Double Metaphone code (the same as the primary when the
/// word has only one way of sounding).
pub(super) fn dmetaphone_alt(s: &str) -> String {
    dmetaphone::double_metaphone(s).1
}

// ---- unaccent ----

/// `unaccent(text)`: every character that has a rule in PostgreSQL's default `unaccent.rules`
/// replaced by what the rule says (nothing, for a combining mark); all others kept. A result of
/// 1,073,741,820 bytes or more is refused: XX000 "invalid memory alloc request size" for 1,073,741,820 to
/// 1,073,741,822, and 54000 "out of memory" from 1,073,741,823 (only a text of hundreds of MB made of
/// characters that grow, such as `¼` into `1/4`, gets there).
pub(super) fn unaccent(s: &str) -> Result<String, Fail> {
    unaccent_within(s, MAX_ALLOC_SIZE)
}

/// `unaccent` on a server whose largest chunk is `max_alloc` bytes, so that the limits can be tried on short
/// texts.
fn unaccent_within(s: &str, max_alloc: u64) -> Result<String, Fail> {
    let rule = |c: char| {
        unaccent_rules::RULES.binary_search_by_key(&c, |&(from, _)| from).ok().map(|i| unaccent_rules::RULES[i].1)
    };
    // a text with nothing to replace comes back as it is, and Postgres never builds a result for it
    let Some(first) = s.char_indices().find_map(|(i, c)| rule(c).map(|_| i)) else {
        return Ok(s.to_string());
    };
    // the result is built in a string buffer, which Postgres will not grow to `max_alloc` bytes or more:
    // the append that would is refused
    let mut out = String::with_capacity(s.len());
    out.push_str(&s[..first]);
    for c in s[first..].chars() {
        match rule(c) {
            Some(to) => out.push_str(to),
            None => out.push(c),
        }
        if out.len() as u64 >= max_alloc {
            return fail(PROGRAM_LIMIT, "out of memory");
        }
    }
    // then the buffer is copied into a text, which needs its header as well
    palloc_up_to(out.len() as u64 + VARHDRSZ, max_alloc)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Every expected value below is what a PostgreSQL 16.13 server answered (UTF8 database, `C.UTF-8`
    // locale, pg_trgm 1.6, fuzzystrmatch 1.2, unaccent 1.1). `float4` results are the `float4` the server
    // printed, so a comparison of `f32`s is a comparison of bits.

    /// Longer than this many characters is refused by levenshtein.
    const LEV_MAX: usize = 255;

    /// An operator's cases: `(a, b, threshold, result)`; a missing threshold is the default one.
    type OpCases = &'static [(&'static str, &'static str, Option<f64>, bool)];
    type OpFn = fn(&str, &str, f64) -> Result<bool, Fail>;
    type FloatFn = fn(&str, &str) -> Result<f32, Fail>;

    /// `(text, show_trgm(text))`
    const SHOW_TRGM: &[(&str, &[&str])] = &[
        ("cat", &["  c", " ca", "at ", "cat"]),
        ("CAT", &["  c", " ca", "at ", "cat"]),
        ("Cat dog", &["  c", "  d", " ca", " do", "at ", "cat", "dog", "og "]),
        ("word", &["  w", " wo", "ord", "rd ", "wor"]),
        ("a", &["  a", " a "]),
        ("ab", &["  a", " ab", "ab "]),
        ("abc", &["  a", " ab", "abc", "bc "]),
        ("", &[]),
        (" ", &[]),
        ("  ", &[]),
        ("a b", &["  a", "  b", " a ", " b "]),
        ("hello-world", &["  h", "  w", " he", " wo", "ell", "hel", "ld ", "llo", "lo ", "orl", "rld", "wor"]),
        ("it's", &["  i", "  s", " it", " s ", "it "]),
        ("foo_bar", &["  b", "  f", " ba", " fo", "ar ", "bar", "foo", "oo "]),
        ("foo.bar", &["  b", "  f", " ba", " fo", "ar ", "bar", "foo", "oo "]),
        ("a1b2", &["  a", " a1", "1b2", "a1b", "b2 "]),
        ("123", &["  1", " 12", "123", "23 "]),
        ("12-34", &["  1", "  3", " 12", " 34", "12 ", "34 "]),
        ("cat cat cat", &["  c", " ca", "at ", "cat"]),
        ("aaaa", &["  a", " aa", "aa ", "aaa"]),
        ("abab", &["  a", " ab", "ab ", "aba", "bab"]),
        ("a!b", &["  a", "  b", " a ", " b "]),
        ("!!!", &[]),
        (
            "The Quick  Brown\tFox",
            &[
                "  b", "  f", "  q", "  t", " br", " fo", " qu", " th", "bro", "ck ", "fox", "he ", "ick", "own",
                "ox ", "qui", "row", "the", "uic", "wn ",
            ],
        ),
        ("ñandú", &["0x927af7", "0xb00338", "0x00a9f5", "0x12522c", "0x304ab5", "and"]),
        ("Élan", &["0xe69b54", "0xf21a94", "0x532122", "an ", "lan"]),
        ("München", &["0xdeb20d", "0xef7d97", "0x1a051e", "  m", "che", "en ", "hen", "nch"]),
        ("straße", &["0x9a07c3", "0xde3801", "  s", " st", "str", "tra", "0x76a40e"]),
        ("İstanbul", &["  i", " is", "anb", "bul", "ist", "nbu", "sta", "tan", "ul "]),
        ("Kelvin", &["  k", " ke", "elv", "in ", "kel", "lvi", "vin"]),
        ("ǅungla", &["0x1c2a25", "0x55aa48", "0x5f70a5", "gla", "la ", "ngl", "ung"]),
        ("ΑΒΓ αβγ", &["0x8be2f9", "0xfd1ff7", "0x0251f3", "0x3ae2d1"]),
        (
            "привет мир",
            &[
                "0x8eb714", "0xc7e732", "0xd4fb88", "0xe64e48", "0xe72710", "0xe96751", "0xeaced8", "0x193782",
                "0x233ac5", "0x34e61d", "0x6fb020",
            ],
        ),
        ("日本語", &["0x8194c0", "0x836e53", "0x1dc363", "0x1e22e9"]),
        (
            "café au lait",
            &["0xef5960", "  a", "  c", "  l", " au", " ca", " la", "0x544980", "ait", "au ", "caf", "it ", "lai"],
        ),
        ("naïve", &["0xb96f40", "0xd8acd7", "  n", " na", "0x59a8da", "ve "]),
        ("x\u{301}y", &["  x", "  y", " x ", " y "]),
        ("\u{301}", &[]),
        ("😀 smile", &["  s", " sm", "ile", "le ", "mil", "smi"]),
        ("a²b", &["  a", "  b", " a ", " b "]),
        ("ａｂｃ１２３", &["0xf8b9ba", "0x008598", "0x0c95c1", "0x151d44", "0x31bda6", "0x495983", "0x7f5ed4"]),
        ("٣٤٥", &["0x899cd4", "0x0b0f72", "0x1f7d7b", "0x598674"]),
        ("ß ẞ", &["0xc96a06", "0x7f0eee"]),
        ("σς Σ", &["0x83f4cc", "0x94c17f", "0x958ac8", "0x3db8d9"]),
    ];

    #[test]
    fn show_trgm_matches_postgres() {
        for (text, want) in SHOW_TRGM {
            assert_eq!(show_trgm(text).unwrap(), *want, "show_trgm({text:?})");
        }
    }

    /// `(a, b, similarity(a, b))`
    const SIMILARITY: &[(&str, &str, f32)] = &[
        ("word", "two words", 0.36363637f32),
        ("cat", "cats", 0.5f32),
        ("cat", "dog", 0.0f32),
        ("", "", 0.0f32),
        ("", "a", 0.0f32),
        ("a", "", 0.0f32),
        ("abc", "abc", 1.0f32),
        ("abc", "ABC", 1.0f32),
        ("hello", "hallo", 0.33333334f32),
        ("postgres", "postgresql", 0.6666667f32),
        ("night", "nacht", 0.2f32),
        ("kitten", "sitting", 0.071428575f32),
        ("Thomas", "Tomas", 0.44444445f32),
        ("foo bar", "bar foo", 1.0f32),
        ("one two", "one  two", 1.0f32),
        ("ab", "ab", 1.0f32),
        ("日本", "日本語", 0.4f32),
        ("Élan", "elan", 0.25f32),
        ("münchen", "munchen", 0.45454547f32),
        ("abcdef", "abcxyz", 0.27272728f32),
        ("The quick brown fox", "The quick brown dog", 0.6666667f32),
        ("cat", "the cat sat", 0.36363637f32),
        ("cat", "concatenate", 0.15384616f32),
        ("abc", "xyz abc xyz", 0.5f32),
        ("hello world", "hello there world", 0.6666667f32),
        ("postgre", "postgresql is great", 0.35f32),
        ("sql", "PostgreSQL", 0.15384616f32),
        ("rat", "pirate rat", 0.4f32),
        ("rat", "pirates", 0.09090909f32),
        ("word", "swordfish", 0.15384616f32),
        ("кот", "котёнок", 0.33333334f32),
        ("!!!", "abc", 0.0f32),
        ("abc abc", "abc", 1.0f32),
        ("aaa", "aaaaaaa", 1.0f32),
    ];

    /// `(a, b, word_similarity(a, b))`
    const WORD_SIMILARITY: &[(&str, &str, f32)] = &[
        ("word", "two words", 0.8f32),
        ("cat", "cats", 0.75f32),
        ("cat", "dog", 0.0f32),
        ("", "", 0.0f32),
        ("", "a", 0.0f32),
        ("a", "", 0.0f32),
        ("abc", "abc", 1.0f32),
        ("abc", "ABC", 1.0f32),
        ("hello", "hallo", 0.33333334f32),
        ("postgres", "postgresql", 0.8888889f32),
        ("night", "nacht", 0.2f32),
        ("kitten", "sitting", 0.14285715f32),
        ("Thomas", "Tomas", 0.44444445f32),
        ("foo bar", "bar foo", 1.0f32),
        ("one two", "one  two", 1.0f32),
        ("ab", "ab", 1.0f32),
        ("日本", "日本語", 0.6666667f32),
        ("Élan", "elan", 0.4f32),
        ("münchen", "munchen", 0.5f32),
        ("abcdef", "abcxyz", 0.42857143f32),
        ("The quick brown fox", "The quick brown dog", 0.8f32),
        ("cat", "the cat sat", 1.0f32),
        ("cat", "concatenate", 0.25f32),
        ("abc", "xyz abc xyz", 1.0f32),
        ("hello world", "hello there world", 0.6666667f32),
        ("postgre", "postgresql is great", 0.875f32),
        ("sql", "PostgreSQL", 0.5f32),
        ("rat", "pirate rat", 1.0f32),
        ("rat", "pirates", 0.25f32),
        ("word", "swordfish", 0.4f32),
        ("кот", "котёнок", 0.75f32),
        ("!!!", "abc", 0.0f32),
        ("abc abc", "abc", 1.0f32),
        ("aaa", "aaaaaaa", 1.0f32),
    ];

    /// `(a, b, strict_word_similarity(a, b))`
    const STRICT_WORD_SIMILARITY: &[(&str, &str, f32)] = &[
        ("word", "two words", 0.5714286f32),
        ("cat", "cats", 0.5f32),
        ("cat", "dog", 0.0f32),
        ("", "", 0.0f32),
        ("", "a", 0.0f32),
        ("a", "", 0.0f32),
        ("abc", "abc", 1.0f32),
        ("abc", "ABC", 1.0f32),
        ("hello", "hallo", 0.33333334f32),
        ("postgres", "postgresql", 0.6666667f32),
        ("night", "nacht", 0.2f32),
        ("kitten", "sitting", 0.071428575f32),
        ("Thomas", "Tomas", 0.44444445f32),
        ("foo bar", "bar foo", 1.0f32),
        ("one two", "one  two", 1.0f32),
        ("ab", "ab", 1.0f32),
        ("日本", "日本語", 0.4f32),
        ("Élan", "elan", 0.25f32),
        ("münchen", "munchen", 0.45454547f32),
        ("abcdef", "abcxyz", 0.27272728f32),
        ("The quick brown fox", "The quick brown dog", 0.8f32),
        ("cat", "the cat sat", 1.0f32),
        ("cat", "concatenate", 0.15384616f32),
        ("abc", "xyz abc xyz", 1.0f32),
        ("hello world", "hello there world", 0.6666667f32),
        ("postgre", "postgresql is great", 0.5833333f32),
        ("sql", "PostgreSQL", 0.15384616f32),
        ("rat", "pirate rat", 1.0f32),
        ("rat", "pirates", 0.09090909f32),
        ("word", "swordfish", 0.15384616f32),
        ("кот", "котёнок", 0.33333334f32),
        ("!!!", "abc", 0.0f32),
        ("abc abc", "abc", 1.0f32),
        ("aaa", "aaaaaaa", 1.0f32),
    ];

    #[test]
    fn similarity_matches_postgres() {
        for (a, b, want) in SIMILARITY {
            assert_eq!(similarity(a, b), Ok(*want), "similarity({a:?}, {b:?})");
        }
    }

    #[test]
    fn word_similarity_matches_postgres() {
        for (a, b, want) in WORD_SIMILARITY {
            assert_eq!(word_similarity(a, b), Ok(*want), "word_similarity({a:?}, {b:?})");
        }
    }

    #[test]
    fn strict_word_similarity_matches_postgres() {
        for (a, b, want) in STRICT_WORD_SIMILARITY {
            assert_eq!(strict_word_similarity(a, b), Ok(*want), "strict_word_similarity({a:?}, {b:?})");
        }
    }

    /// `(a, b, a <-> b, a <<-> b, a <->> b, a <<<-> b, a <->>> b)`
    const DISTANCES: &[(&str, &str, [f32; 5])] = &[
        ("word", "two words", [0.6363636f32, 0.19999999f32, 0.6f32, 0.4285714f32, 0.6363636f32]),
        ("cat", "cats", [0.5f32, 0.25f32, 0.39999998f32, 0.5f32, 0.5f32]),
        ("cat", "dog", [1.0f32, 1.0f32, 1.0f32, 1.0f32, 1.0f32]),
        ("abc", "abc", [0.0f32, 0.0f32, 0.0f32, 0.0f32, 0.0f32]),
        ("hello", "hallo", [0.6666666f32, 0.6666666f32, 0.6666666f32, 0.6666666f32, 0.6666666f32]),
        ("", "abc", [1.0f32, 1.0f32, 1.0f32, 1.0f32, 1.0f32]),
        ("rat", "pirate rat", [0.6f32, 0.0f32, 0.6f32, 0.0f32, 0.6f32]),
        ("postgre", "postgresql is great", [0.65f32, 0.125f32, 0.6315789f32, 0.4166667f32, 0.65f32]),
        ("foo bar", "bar foo", [0.0f32, 0.0f32, 0.0f32, 0.0f32, 0.0f32]),
        ("sql", "PostgreSQL", [0.84615386f32, 0.5f32, 0.8181818f32, 0.84615386f32, 0.84615386f32]),
    ];

    #[test]
    fn distance_operators_match_postgres() {
        for (a, b, [d, wd, wdc, sd, sdc]) in DISTANCES {
            assert_eq!(similarity_dist(a, b), Ok(*d), "{a:?} <-> {b:?}");
            assert_eq!(word_similarity_dist(a, b), Ok(*wd), "{a:?} <<-> {b:?}");
            assert_eq!(word_similarity_dist_commutator(a, b), Ok(*wdc), "{a:?} <->> {b:?}");
            assert_eq!(strict_word_similarity_dist(a, b), Ok(*sd), "{a:?} <<<-> {b:?}");
            assert_eq!(strict_word_similarity_dist_commutator(a, b), Ok(*sdc), "{a:?} <->>> {b:?}");
        }
    }

    /// `(a, b, threshold, a % b)`; a missing threshold is the default one
    const SIM_OPS: OpCases = &[
        ("word", "two words", None, true),
        ("cat", "cats", None, true),
        ("cat", "dog", None, false),
        ("abc", "abc", None, true),
        ("word", "two words", Some(0.36), true),
        ("word", "two words", Some(0.37), false),
        ("cat", "cats", Some(0.5), true),
        ("cat", "cats", Some(0.4), true),
        ("cat", "cats", Some(0.0), true),
        ("cat", "dog", Some(0.0), true),
        ("cat", "dog", Some(1.0), false),
        ("abc", "abc", Some(1.0), true),
        ("hello", "hallo", Some(0.3), true),
        ("hello", "hallo", Some(0.2857143), true),
        ("postgres", "postgresql", Some(0.5), true),
    ];

    /// `(a, b, threshold, a <% b)`; a missing threshold is the default one
    const WORD_OPS: OpCases = &[
        ("word", "two words", None, true),
        ("cat", "the cat sat", None, true),
        ("cat", "concatenate", None, false),
        ("abc", "xyz abc xyz", None, true),
        ("sql", "PostgreSQL", None, false),
        ("rat", "pirate rat", None, true),
        ("word", "swordfish", None, false),
        ("word", "swordfish", Some(0.5), false),
        ("word", "swordfish", Some(0.57), false),
        ("hello world", "hello there world", Some(0.9), false),
        ("hello world", "hello there world", Some(0.5), true),
        ("postgre", "postgresql is great", Some(0.8), true),
        ("rat", "pirates", Some(0.3), false),
        ("", "abc", Some(0.0), true),
        ("abc", "", Some(0.3), false),
    ];

    /// `(a, b, threshold, a %> b)`; a missing threshold is the default one
    const WORD_OPS_COMMUTED: OpCases = &[
        ("two words", "word", None, true),
        ("the cat sat", "cat", None, true),
        ("concatenate", "cat", None, false),
        ("xyz abc xyz", "abc", None, true),
        ("PostgreSQL", "sql", None, false),
        ("swordfish", "word", Some(0.5), false),
        ("swordfish", "word", Some(0.57), false),
        ("hello there world", "hello world", Some(0.5), true),
    ];

    /// `(a, b, threshold, a <<% b)`; a missing threshold is the default one
    const STRICT_OPS: OpCases = &[
        ("word", "two words", None, true),
        ("cat", "the cat sat", None, true),
        ("cat", "concatenate", None, false),
        ("abc", "xyz abc xyz", None, true),
        ("sql", "PostgreSQL", None, false),
        ("rat", "pirate rat", None, true),
        ("word", "swordfish", Some(0.3), false),
        ("word", "swordfish", Some(0.5), false),
        ("hello world", "hello there world", Some(0.9), false),
        ("hello world", "hello there world", Some(0.6), true),
        ("postgre", "postgresql is great", Some(0.4), true),
    ];

    /// `(a, b, threshold, a %>> b)`; a missing threshold is the default one
    const STRICT_OPS_COMMUTED: OpCases = &[
        ("two words", "word", None, true),
        ("the cat sat", "cat", None, true),
        ("concatenate", "cat", None, false),
        ("xyz abc xyz", "abc", None, true),
        ("swordfish", "word", Some(0.3), false),
        ("swordfish", "word", Some(0.5), false),
    ];

    #[test]
    fn boolean_operators_match_postgres() {
        let ops: [(&str, OpCases, OpFn, f64); 5] = [
            ("%", SIM_OPS, similarity_op, SIMILARITY_THRESHOLD),
            ("<%", WORD_OPS, word_similarity_op, WORD_SIMILARITY_THRESHOLD),
            ("%>", WORD_OPS_COMMUTED, word_similarity_commutator_op, WORD_SIMILARITY_THRESHOLD),
            ("<<%", STRICT_OPS, strict_word_similarity_op, STRICT_WORD_SIMILARITY_THRESHOLD),
            ("%>>", STRICT_OPS_COMMUTED, strict_word_similarity_commutator_op, STRICT_WORD_SIMILARITY_THRESHOLD),
        ];
        for (name, cases, op, default) in ops {
            for (a, b, threshold, want) in cases {
                assert_eq!(op(a, b, threshold.unwrap_or(default)), Ok(*want), "{a:?} {name} {b:?} at {threshold:?}");
            }
        }
    }

    /// Postgres keeps the thresholds as doubles set from `float4` constants, so a similarity that is
    /// exactly 0.3 as a `float4` passes the default 0.3 and a `SET` to 0.3 alike, while one just under
    /// 0.3 as a double would pass neither.
    #[test]
    fn default_thresholds_are_the_float4_constants() {
        assert_eq!(SIMILARITY_THRESHOLD, f64::from(0.3f32));
        assert_eq!(WORD_SIMILARITY_THRESHOLD, f64::from(0.6f32));
        assert_eq!(STRICT_WORD_SIMILARITY_THRESHOLD, f64::from(0.5f32));
        assert_ne!(SIMILARITY_THRESHOLD, 0.3f64);
    }

    /// `(a, b, levenshtein(a, b))`
    const LEVENSHTEIN: &[(&str, &str, i32)] = &[
        ("kitten", "sitting", 3),
        ("", "", 0),
        ("", "abc", 3),
        ("abc", "", 3),
        ("abc", "abc", 0),
        ("abc", "abd", 1),
        ("abc", "ab", 1),
        ("ab", "abc", 1),
        ("flaw", "lawn", 2),
        ("Saturday", "Sunday", 3),
        ("intention", "execution", 5),
        ("naïve", "naive", 1),
        ("日本語", "日本", 1),
        ("日本語", "中文字", 3),
        ("😀a", "a😀", 2),
        ("ab", "ba", 2),
        ("hello", "world", 4),
        ("a", "b", 1),
        ("Levenshtein", "Frankenstein", 6),
        ("postgres", "postgresql", 2),
    ];

    /// `(a, b, insert, delete, substitute, levenshtein(a, b, insert, delete, substitute))`
    const LEVENSHTEIN_COSTS: &[(&str, &str, [i32; 3], i32)] = &[
        ("kitten", "sitting", [2, 1, 1], 4),
        ("kitten", "sitting", [1, 2, 1], 3),
        ("kitten", "sitting", [1, 1, 2], 5),
        ("kitten", "sitting", [1, 1, 3], 5),
        ("flaw", "lawn", [0, 1, 1], 1),
        ("flaw", "lawn", [1, 0, 1], 1),
        ("Saturday", "Sunday", [3, 3, 1], 7),
        ("intention", "execution", [5, 1, 10], 24),
        ("abc", "abd", [-1, 1, 1], 0),
        ("abc", "abd", [1, -1, 1], 0),
        ("abc", "abd", [2147483647, 1, 1], -2147483647),
        ("kitten", "sitting", [2147483647, 2147483647, 2147483647], -12),
        ("abba", "babbabb", [715827884, 3, 715827892], -1431655757),
    ];

    /// `(a, b, max_d, levenshtein_less_equal(a, b, max_d))`: above `max_d` the value is whatever the search left
    const LEVENSHTEIN_LE: &[(&str, &str, i32, i32)] = &[
        ("kitten", "sitting", 3, 3),
        ("kitten", "sitting", 2, 3),
        ("kitten", "sitting", 1, 2),
        ("kitten", "sitting", 0, 1),
        ("kitten", "sitting", -1, 3),
        ("kitten", "sitting", 100, 3),
        ("abc", "abc", 0, 0),
        ("abc", "abd", 0, 1),
        ("abc", "abd", 1, 1),
        ("", "abcdef", 2, 6),
        ("abcdef", "", 2, 6),
        ("abcdef", "ab", 1, 2),
        ("ab", "abcdef", 3, 4),
        ("Saturday", "Sunday", 2, 3),
        ("Saturday", "Sunday", 3, 3),
        ("intention", "execution", 4, 5),
        ("intention", "execution", 5, 5),
        ("hello", "world", 3, 4),
        ("日本語", "中文字", 2, 3),
        ("a", "b", 0, 1),
    ];

    /// `(a, b, [insert, delete, substitute], max_d, levenshtein_less_equal(a, b, insert, delete, substitute, max_d))`
    const LEVENSHTEIN_LE_COSTS: &[(&str, &str, [i32; 3], i32, i32)] = &[
        ("kitten", "sitting", [2, 1, 1], 3, 4),
        ("kitten", "sitting", [2, 1, 1], 4, 4),
        ("kitten", "sitting", [2, 1, 1], 5, 4),
        ("kitten", "sitting", [1, 2, 1], 2, 3),
        ("kitten", "sitting", [1, 1, 5], 4, 5),
        ("kitten", "sitting", [1, 1, 5], 5, 5),
        ("abcdef", "abc", [1, 3, 1], 4, 5),
        ("abcdef", "abc", [1, 3, 1], 9, 9),
        ("abc", "abcdef", [3, 1, 1], 8, 9),
        ("abc", "abcdef", [3, 1, 1], 9, 9),
        ("flaw", "lawn", [2, 2, 2], 3, 4),
        ("flaw", "lawn", [2, 2, 2], 4, 4),
        ("abc", "xyz", [1, 1, 10], 6, 6),
        ("abc", "xyz", [1, 1, 10], 7, 6),
        ("hello", "world", [0, 0, 1], 2, 0),
        // Costs so large that the first row's stop column wraps negative (-1 to -3 here): Postgres keeps it, the
        // band is empty and the answer is `max_d + 1`, while the same strings and costs without the bound give a
        // wrapped distance (-1431655757 for the first, in the table above).
        ("abba", "babbabb", [715827884, 3, 715827892], 5, 6),
        ("aba", "baaaaaa", [536870912, 536869912, 2103821310], 2, 3),
        ("aab", "bbbab", [1073741824, -683094506, -8], 9, 10),
        ("aa", "ababca", [536870913, -1944, -1130], 4, 5),
        ("a", "bbcbb", [536870914, -1393, -1820], 12, 13),
        ("bcba", "ababbacb", [536870913, -1496, -248], 7, 8),
        ("caac", "cbccacba", [536870912, 1740, 715827884], 5, 6),
        ("bbbcb", "bbbaaabb", [715827886, 64, 1431655747], 11, 12),
        ("cac", "abbacc", [715827884, 50, 1073741841], 5, 6),
        ("aaéa", "日aaaaaé日", [536870913, -1302, -678], 8, 9),
        ("日a日é", "日éa日a日aa", [536870913, 546, -766], 10, 11),
        ("aa日", "aaéa日éé", [536870912, 227, -1422], 4, 5),
        ("日", "日aéa日", [536870913, 13, -2147483624], 12, 13),
        ("日aa", "é日aéaéa", [536870912, 295, -860], 11, 12),
        ("日aéé", "日a日é日éaé", [536870915, 1301, -1282], 12, 13),
        ("日日日a", "日éa日aéa", [715827884, 26, 2147483640], 7, 8),
        ("a日ééé", "日aéa日日日日", [715827884, 87, 536870942], 12, 13),
        // Another thing: with costs of a billion the band vanishes without the cell that is returned ever being
        // written, and Postgres gives what an earlier allocation left there. In a fresh backend that is 0; later in
        // a session it can be any number (-589934592 after the first of these, for one).
        ("abcdef", "ab", [1000000000, 1000000000, 1000000000], 0, 0),
        ("dcaadca", "dcacadda", [0, 2147483647, 1485790717], 6, 0),
        // (`levenshtein_less_equal('bb', 'b', -2147483647, -2147483648, -1073741824, 11)` has a stop column of
        // -2147483635, and the Postgres server dies of a segfault on it, so there is no answer to copy; this
        // gives `max_d + 1`, 12, as it does for any other empty band.)
    ];

    #[test]
    fn levenshtein_matches_postgres() {
        for (a, b, want) in LEVENSHTEIN {
            assert_eq!(levenshtein(a, b), Ok(*want), "levenshtein({a:?}, {b:?})");
        }
        for (a, b, [ins, del, sub], want) in LEVENSHTEIN_COSTS {
            assert_eq!(
                levenshtein_with_costs(a, b, *ins, *del, *sub),
                Ok(*want),
                "levenshtein({a:?}, {b:?}, {ins}, {del}, {sub})"
            );
        }
        for (a, b, max_d, want) in LEVENSHTEIN_LE {
            assert_eq!(
                levenshtein_less_equal(a, b, *max_d),
                Ok(*want),
                "levenshtein_less_equal({a:?}, {b:?}, {max_d})"
            );
        }
        for (a, b, [ins, del, sub], max_d, want) in LEVENSHTEIN_LE_COSTS {
            assert_eq!(
                levenshtein_less_equal_with_costs(a, b, *ins, *del, *sub, *max_d),
                Ok(*want),
                "levenshtein_less_equal({a:?}, {b:?}, {ins}, {del}, {sub}, {max_d})"
            );
        }
    }

    #[test]
    fn levenshtein_length_limit_matches_postgres() {
        let too_long: Fail = ("22023", format!("levenshtein argument exceeds maximum length of {LEV_MAX} characters"));
        // 255 characters are fine
        assert_eq!(levenshtein(&"a".repeat(255), &"a".repeat(255)), Ok(0));
        assert_eq!(levenshtein(&"é".repeat(255), &"e".repeat(255)), Ok(255));
        // 256 are not, in either argument, or of more than one byte each, or under the bounded variants
        assert_eq!(levenshtein(&"a".repeat(256), "a"), Err(too_long.clone()));
        assert_eq!(levenshtein("a", &"a".repeat(256)), Err(too_long.clone()));
        assert_eq!(levenshtein(&"日".repeat(256), "日"), Err(too_long.clone()));
        assert_eq!(levenshtein_less_equal(&"a".repeat(256), "b", 1), Err(too_long.clone()));
        assert_eq!(levenshtein_with_costs(&"日".repeat(256), "日", 1, 1, 1), Err(too_long.clone()));
        assert_eq!(levenshtein_less_equal_with_costs("b", &"a".repeat(300), 1, 1, 1, 5), Err(too_long));
        // an empty string is exempt: the other one's length is the answer, however long
        assert_eq!(levenshtein("", &"a".repeat(300)), Ok(300));
        assert_eq!(levenshtein(&"a".repeat(300), ""), Ok(300));
    }

    /// What Postgres 16.13 answered for texts too long for its allocator, tried with `repeat(' ', n)` (blanks have no
    /// trigrams, so the server fails or succeeds on the length alone), with `repeat('a ', n)` for the trigram count,
    /// and with every function and operator: the errors are raised before anything is built, so these cost the text
    /// and no more.
    #[test]
    fn trigram_memory_limits_match_postgres() {
        let out_of_memory: Fail = ("54000", "out of memory".to_string());
        let too_big = |size: u64| -> Fail { ("XX000", format!("invalid memory alloc request size {size}")) };

        // where each check gives way: `similarity` and `show_trgm` work up to 238,609,291 bytes, fail in
        // `palloc` at 238,609,292 and 238,609,293 and in `protect_out_of_mem` from 238,609,294
        assert_eq!(generate_trgm_alloc(0), Ok(()));
        assert_eq!(generate_trgm_alloc(238_609_291), Ok(()));
        assert_eq!(generate_trgm_alloc(238_609_292), Err(too_big(1_073_741_828)));
        assert_eq!(generate_trgm_alloc(238_609_293), Err(too_big(1_073_741_828)));
        assert_eq!(generate_trgm_alloc(238_609_294), Err(out_of_memory.clone()));
        assert_eq!(generate_trgm_alloc(1_000_000_000), Err(out_of_memory.clone()));
        // word similarity adds the two lengths: `word_similarity('a', repeat(' ', 238609292))` is 0 and
        // `word_similarity('aa', repeat(' ', 238609292))` is 54000
        assert_eq!(protect_out_of_mem(238_609_293), Ok(()));
        assert_eq!(protect_out_of_mem(238_609_294), Err(out_of_memory.clone()));
        // and then fails from 134,217,728 trigrams in both texts
        assert_eq!(positional_trgm_alloc(134_217_727), Ok(()));
        assert_eq!(positional_trgm_alloc(134_217_728), Err(too_big(1_073_741_824)));
        assert_eq!(positional_trgm_alloc(134_217_729), Err(too_big(1_073_741_832)));
        // the count that the last check goes by is the number of trigrams there are
        for (text, _) in SHOW_TRGM {
            assert_eq!(trgm_count(text), trgm_only(text).0.len(), "trgm_count({text:?})");
        }

        // the functions, on texts of those lengths; the first argument's error comes first
        let blanks = " ".repeat(238_609_294);
        for (n, want) in [
            (238_609_292, too_big(1_073_741_828)),
            (238_609_293, too_big(1_073_741_828)),
            (238_609_294, out_of_memory.clone()),
        ] {
            let t = &blanks[..n];
            for (a, b) in [(t, "a"), ("a", t)] {
                assert_eq!(similarity(a, b), Err(want.clone()), "similarity of {n} bytes");
                assert_eq!(similarity_dist(a, b), Err(want.clone()), "<-> of {n} bytes");
                assert_eq!(similarity_op(a, b, SIMILARITY_THRESHOLD), Err(want.clone()), "% of {n} bytes");
            }
            assert_eq!(show_trgm(t), Err(want.clone()), "show_trgm of {n} bytes");
        }
        assert_eq!(similarity(&blanks[..238_609_292], &blanks), Err(too_big(1_073_741_828)));
        assert_eq!(similarity(&blanks, &blanks[..238_609_292]), Err(out_of_memory.clone()));

        let (pattern, text) = ("aa", &blanks[..238_609_292]);
        let floats: [FloatFn; 6] = [
            word_similarity,
            strict_word_similarity,
            word_similarity_dist,
            word_similarity_dist_commutator,
            strict_word_similarity_dist,
            strict_word_similarity_dist_commutator,
        ];
        for f in floats {
            assert_eq!(f(pattern, text), Err(out_of_memory.clone()));
            assert_eq!(f(text, pattern), Err(out_of_memory.clone()));
        }
        let bools: [(OpFn, f64); 4] = [
            (word_similarity_op, WORD_SIMILARITY_THRESHOLD),
            (word_similarity_commutator_op, WORD_SIMILARITY_THRESHOLD),
            (strict_word_similarity_op, STRICT_WORD_SIMILARITY_THRESHOLD),
            (strict_word_similarity_commutator_op, STRICT_WORD_SIMILARITY_THRESHOLD),
        ];
        for (op, threshold) in bools {
            assert_eq!(op(pattern, text, threshold), Err(out_of_memory.clone()));
            assert_eq!(op(text, pattern, threshold), Err(out_of_memory.clone()));
        }
        drop(blanks);

        // `repeat('a ', 67108861) || 'bbb'` has 134,217,726 trigrams: one too many with the two of 'z', whichever
        // way round the texts are and for the strict kind (without the last b Postgres answers 0, as above at
        // the boundary of `positional_trgm_alloc`)
        let text = "a ".repeat(67_108_861) + "bbb";
        assert_eq!(word_similarity("z", &text), Err(too_big(1_073_741_824)));
        assert_eq!(
            strict_word_similarity_op(&text, "z", STRICT_WORD_SIMILARITY_THRESHOLD),
            Err(too_big(1_073_741_824))
        );
    }

    /// `unaccent` of a result of 1 GB or more, from `unaccent(repeat('a', x) || repeat(chr(188), k))` on 16.13
    /// (`¼` turns into `1/4`, so the result is x + 3k bytes): 1,073,741,819 bytes work, 1,073,741,820 to
    /// 1,073,741,822 give XX000 "invalid memory alloc request size" (the result and its header) and 1,073,741,823 or
    /// more 54000 "out of memory" (the buffer cannot grow), however the last bytes are spelt. A text that size
    /// cannot be made here, so the same boundaries are tried at `max_alloc` of 100, which stand in the same
    /// places relative to it.
    #[test]
    fn unaccent_size_limit_matches_postgres() {
        assert_eq!(MAX_ALLOC_SIZE, 1_073_741_823);
        let (out_of_memory, too_big) = (("54000", "out of memory".to_string()), |size: usize| {
            ("XX000", format!("invalid memory alloc request size {size}"))
        });
        for o in 40..=130usize {
            let text = "a".repeat(o - 30) + &"¼".repeat(10);
            let want = match o {
                ..=96 => Ok("a".repeat(o - 30) + &"1/4".repeat(10)),
                97..=99 => Err(too_big(o + 4)),
                _ => Err(out_of_memory.clone()),
            };
            assert_eq!(unaccent_within(&text, 100), want, "a result of {o} bytes at 100");
        }
        // the same when the last bytes are not from a rule: the buffer is what fills up (`¼` and n b's: 3 + n bytes)
        let with_bs = |n: usize| "¼".to_string() + &"b".repeat(n);
        assert_eq!(unaccent_within(&with_bs(93), 100), Ok("1/4".to_string() + &"b".repeat(93)));
        assert_eq!(unaccent_within(&with_bs(94), 100), Err(too_big(101)));
        assert_eq!(unaccent_within(&with_bs(96), 100), Err(too_big(103)));
        assert_eq!(unaccent_within(&with_bs(97), 100), Err(out_of_memory.clone()));
        assert_eq!(unaccent_within(&with_bs(200), 100), Err(out_of_memory.clone()));
        // a text with nothing to replace is given back as it is, however long: no result is made for it
        assert_eq!(unaccent_within(&"a".repeat(500), 100), Ok("a".repeat(500)));
        assert_eq!(unaccent("¼"), Ok("1/4".to_string()));
    }

    /// `(text, soundex(text))`
    const SOUNDEX: &[(&str, &str)] = &[
        ("Robert", "R163"),
        ("Rupert", "R163"),
        ("Rubin", "R150"),
        ("Ashcraft", "A226"),
        ("Ashcroft", "A226"),
        ("Tymczak", "T522"),
        ("Pfister", "P236"),
        ("Honeyman", "H555"),
        ("Lloyd", "L300"),
        ("Lee", "L000"),
        ("Jackson", "J250"),
        ("Washington", "W252"),
        ("Gutierrez", "G362"),
        ("O'Brien", "O165"),
        ("Smith", "S530"),
        ("Smythe", "S530"),
        ("a", "A000"),
        ("A", "A000"),
        ("hello world", "H464"),
        ("", ""),
        ("   ", ""),
        ("123", ""),
        ("  robert", "R163"),
        ("42 Robert", "R163"),
        ("é", ""),
        ("éclair", "C460"),
        ("Zzzz", "Z000"),
        ("Hhh", "H000"),
        ("Bbbb", "B000"),
        ("Ab12cd", "A130"),
        ("Wyatt", "W300"),
        ("Euler", "E460"),
        ("Gauss", "G200"),
        ("Hilbert", "H416"),
        ("Knuth", "K530"),
        ("Lukasiewicz", "L222"),
        ("naïve", "N100"),
        ("Schwarzenegger", "S625"),
    ];

    /// `(a, b, difference(a, b))`
    const DIFFERENCE: &[(&str, &str, i32)] = &[
        ("Robert", "Rupert", 4),
        ("Anne", "Ann", 4),
        ("hello", "world", 0),
        ("", "", 4),
        ("", "abc", 0),
        ("abc", "", 0),
        ("Smith", "Smythe", 4),
        ("Ashcraft", "Ashcroft", 4),
        ("Jackson", "Jaxon", 4),
        ("Washington", "Lincoln", 0),
        ("a", "b", 3),
        ("123", "456", 4),
        ("é", "e", 0),
        ("Kathryn", "Catherine", 3),
        ("Stephen", "Steven", 4),
    ];

    #[test]
    fn soundex_matches_postgres() {
        for (text, want) in SOUNDEX {
            assert_eq!(soundex(text), *want, "soundex({text:?})");
        }
        for (a, b, want) in DIFFERENCE {
            assert_eq!(difference(a, b), *want, "difference({a:?}, {b:?})");
        }
    }

    /// `(text, max_output_length, metaphone(text, max_output_length))`; an `Err` is "SQLSTATE message"
    const METAPHONE: &[(&str, i32, Result<&str, &str>)] = &[
        ("Thompson", 10, Ok("0MPSN")),
        ("Thompson", 2, Ok("0M")),
        ("Knight", 4, Ok("NFT")),
        ("Wright", 4, Ok("RFT")),
        ("Xavier", 4, Ok("SFR")),
        ("Schwarzenegger", 10, Ok("XWRSNKR")),
        ("character", 6, Ok("XRKTR")),
        ("physics", 6, Ok("FSKS")),
        ("Aeon", 4, Ok("EN")),
        ("Gnome", 4, Ok("NM")),
        ("Which", 4, Ok("HX")),
        ("Whirl", 4, Ok("HRL")),
        ("judge", 4, Ok("JJ")),
        ("laugh", 4, Ok("LF")),
        ("night", 4, Ok("NFT")),
        ("thumb", 4, Ok("0M")),
        ("science", 6, Ok("SNS")),
        ("cheese", 4, Ok("XS")),
        ("ocean", 4, Ok("OSN")),
        ("school", 6, Ok("SKL")),
        ("Christ", 6, Ok("KRST")),
        ("Wasserman", 10, Ok("WSRMN")),
        ("x", 4, Ok("S")),
        ("xx", 4, Ok("S")),
        ("box", 1, Ok("B")),
        ("box", 2, Ok("BK")),
        ("box", 3, Ok("BKS")),
        ("ghost", 4, Ok("FST")),
        ("hugh", 4, Ok("HF")),
        ("bough", 4, Ok("B")),
        ("ough", 4, Ok("OF")),
        ("gnat", 4, Ok("NT")),
        ("Pneumonia", 8, Ok("NMN")),
        ("Xylophone", 8, Ok("SLFN")),
        ("knead", 3, Ok("NT")),
        ("Ashcraft", 255, Ok("AXKRFT")),
        ("Tymczak", 255, Ok("TMKSK")),
        ("Hello, World!", 10, Ok("HLWRLT")),
        ("a b c", 10, Ok("ABK")),
        ("123 abc", 10, Ok("ABK")),
        ("é", 4, Ok("")),
        ("éclair", 10, Ok("KLR")),
        ("naïve", 10, Ok("NF")),
        ("", 4, Ok("")),
        ("", 0, Ok("")),
        ("", 1000, Ok("")),
        ("!!!", 4, Ok("")),
        ("abc", 0, Err("2200F output cannot be empty string")),
        ("abc", -1, Err("2200F output cannot be empty string")),
        ("abc", 256, Err("22023 output exceeds the maximum length of 255 bytes")),
        ("abc", 255, Ok("ABK")),
        ("abc", 1000, Err("22023 output exceeds the maximum length of 255 bytes")),
        ("abc", 2147483647, Err("22023 output exceeds the maximum length of 255 bytes")),
        ("abc", -2147483648, Err("2200F output cannot be empty string")),
    ];

    #[test]
    fn metaphone_matches_postgres() {
        for (text, max_len, want) in METAPHONE {
            let got = metaphone(text, *max_len).map_err(|(code, message)| format!("{code} {message}"));
            assert_eq!(got, want.map(String::from).map_err(String::from), "metaphone({text:?}, {max_len})");
        }
    }

    /// The input may be 255 bytes, not characters.
    #[test]
    fn metaphone_length_limit_is_in_bytes() {
        let err = |r: Result<String, Fail>| r.map_err(|(code, message)| format!("{code} {message}"));
        assert_eq!(
            err(metaphone(&"a".repeat(256), 4)),
            Err("22023 argument exceeds the maximum length of 255 bytes".to_string())
        );
        assert_eq!(err(metaphone(&"a".repeat(255), 4)), Ok("A".to_string()));
        assert_eq!(
            err(metaphone(&"é".repeat(128), 4)),
            Err("22023 argument exceeds the maximum length of 255 bytes".to_string())
        );
        assert_eq!(err(metaphone(&"é".repeat(127), 4)), Ok("".to_string()));
    }

    /// `(name, daitch_mokotoff(name))`; `None` is SQL NULL
    const DAITCH_MOKOTOFF: &[(&str, Option<&[&str]>)] = &[
        ("Schwarzenegger", Some(&["479465", "474659"])),
        ("Peterson", Some(&["739460", "734600"])),
        ("Moskowitz", Some(&["645740"])),
        ("Jackson", Some(&["154600", "145460", "454600", "445460"])),
        ("Mokotoff", Some(&["653700"])),
        ("Auerbach", Some(&["097500", "097400"])),
        ("Haber", Some(&["579000"])),
        ("Friedman", Some(&["793660"])),
        ("Ashkenazi", Some(&["045640"])),
        ("Szczepanski", Some(&["276450"])),
        ("Łukasiewicz", Some(&["054740"])),
        ("Wójcik", Some(&["755000", "745000"])),
        ("Zhukov", Some(&["457000"])),
        ("Daitch", Some(&["340000"])),
        ("Bouey", Some(&["710000"])),
        ("Stern", Some(&["296000"])),
        ("Sternberg", Some(&["296795"])),
        ("Rosenberg", Some(&["946795"])),
        ("Rozenberg", Some(&["946795"])),
        ("Horowitz", Some(&["597400"])),
        ("Hurwitz", Some(&["597400"])),
        ("Czerny", Some(&["496000"])),
        ("Tchaikovsky", Some(&["457450"])),
        ("Cohen", Some(&["556000", "456000"])),
        ("Kohn", Some(&["560000"])),
        ("Cheney", Some(&["560000", "460000"])),
        ("a", Some(&["000000"])),
        ("ab", Some(&["070000"])),
        ("Ä", Some(&["000000"])),
        ("Ąę", Some(&["600000", "000000"])),
        ("Ţara", Some(&["390000", "490000"])),
        ("ÉÀ", Some(&["000000"])),
        ("über", Some(&["079000"])),
        ("", None),
        ("   ", None),
        ("123", None),
        ("日本", None),
        ("O'Brien", Some(&["079600"])),
        ("Mc-Donald", Some(&["653683", "643683"])),
        ("CHARACTER", Some(&["595390", "594390", "495390", "494390"])),
        ("Success", Some(&["454000", "454400", "445400", "444000"])),
        ("Schtsch", Some(&["200000"])),
        ("Szczecin", Some(&["256000", "246000"])),
        ("Jaeger", Some(&["159000", "459000"])),
        ("Ñuñez", Some(&["664000"])),
        ("Øster", Some(&["043900"])),
        ("Ÿ", None),
        ("ß", Some(&["400000"])),
        (
            "ccbcacc",
            Some(&[
                "547554", "547545", "547454", "547445", "457554", "457545", "457454", "457445", "575500", "575540",
                "575450", "575400", "574500", "574540", "574450", "574400", "547550", "547540", "547450", "547440",
                "457550", "457540", "457450", "457440", "475500", "475540", "475450", "475400", "474500", "474540",
                "474450", "474400",
            ]),
        ),
    ];

    #[test]
    fn daitch_mokotoff_matches_postgres() {
        for (name, want) in DAITCH_MOKOTOFF {
            let got = daitch_mokotoff(name);
            let got: Option<Vec<&str>> = got.as_ref().map(|codes| codes.iter().map(String::as_str).collect());
            assert_eq!(got.as_deref(), *want, "daitch_mokotoff({name:?})");
        }
    }

    /// `(word, dmetaphone(word), dmetaphone_alt(word))`
    const DMETAPHONE: &[(&str, &str, &str)] = &[
        ("Thompson", "TMPS", "TMPS"),
        ("Schmidt", "XMT", "SMT"),
        ("Smith", "SM0", "XMT"),
        ("Smythe", "SM0", "XMT"),
        ("Xavier", "SF", "SFR"),
        ("Jose", "HS", "HS"),
        ("San Jacinto", "SNHS", "SNHS"),
        ("Michael", "MKL", "MXL"),
        ("Caesar", "SSR", "SSR"),
        ("Cheryl", "XRL", "XRL"),
        ("Czerny", "SRN", "XRN"),
        ("Filipowicz", "FLPT", "FLPF"),
        ("Wasserman", "ASRM", "FSRM"),
        ("Arnow", "ARN", "ARNF"),
        ("Gnome", "NM", "NM"),
        ("Knight", "NT", "NT"),
        ("Aguilar", "AKLR", "AKLR"),
        ("Bach", "PK", "PK"),
        ("Ghislane", "JLN", "JLN"),
        ("Tchaikovsky", "XKFS", "XKFS"),
        ("Breaux", "PR", "PR"),
        ("Zhao", "J", "J"),
        ("Edge", "AJ", "AJ"),
        ("Edgar", "ATKR", "ATKR"),
        ("Ocean", "ASN", "ASN"),
        ("Laugh", "LF", "LF"),
        ("Vanderbilt", "FNTR", "FNTR"),
        ("McHugh", "MK", "MK"),
        ("Bellocchio", "PLX", "PLX"),
        ("Accident", "AKST", "AKST"),
        ("Succeed", "SKST", "SKST"),
        ("Focaccia", "FKX", "FKX"),
        ("Chianti", "KNT", "KNT"),
        ("Chemistry", "KMST", "KMST"),
        ("Orchestra", "ARKS", "ARKS"),
        ("Architect", "ARKT", "ARKT"),
        ("Wachtler", "AKTL", "FKTL"),
        ("Dumb", "TM", "TM"),
        ("Island", "ALNT", "ALNT"),
        ("Carlisle", "KRLL", "KRLL"),
        ("Sugar", "XKR", "SKR"),
        ("School", "SKL", "SKL"),
        ("Schneider", "XNTR", "SNTR"),
        ("Yankelovich", "ANKL", "ANKL"),
        ("Bajador", "PJTR", "PHTR"),
        ("Rogier", "RJ", "RJR"),
        ("Hochmeier", "HKMR", "HKMR"),
        ("Resnais", "RSN", "RSNS"),
        ("Artois", "ART", "ARTS"),
        ("Gallegos", "KLKS", "KKS"),
        ("Cabrillo", "KPRL", "KPR"),
        ("Tagliaro", "TKLR", "TLR"),
        ("Biaggi", "PJ", "PK"),
        ("Campbell", "KMPL", "KMPL"),
        ("Raspberry", "RSPR", "RSPR"),
        ("Thomas", "TMS", "TMS"),
        ("Thames", "TMS", "TMS"),
        ("Hugh", "H", "H"),
        ("Cough", "KF", "KF"),
        ("Broughton", "PRTN", "PRTN"),
        ("Pierce", "PRS", "PRS"),
        ("a", "A", "A"),
        ("", "", ""),
        ("   ", "", ""),
        ("123", "", ""),
        ("Çelik", "LK", "LK"),
        ("Ñandú", "NT", "NT"),
        ("naïve", "NF", "NF"),
        ("привет", "NN", "NN"),
        ("ǀ", "S", "S"),
        ("ÇÇ", "", ""),
        ("Wicz", "ATS", "FFX"),
        ("Aaaaaaa", "A", "A"),
        ("Z", "S", "S"),
        ("ZZ", "S", "S"),
        ("Hjalmar", "JLMR", "JLMR"),
        ("Witz", "ATS", "FFX"),
    ];

    #[test]
    fn dmetaphone_matches_postgres() {
        for (word, primary, alternate) in DMETAPHONE {
            assert_eq!(dmetaphone(word), *primary, "dmetaphone({word:?})");
            assert_eq!(dmetaphone_alt(word), *alternate, "dmetaphone_alt({word:?})");
        }
    }

    /// `(text, unaccent(text))`
    const UNACCENT: &[(&str, &str)] = &[
        ("Hôtel", "Hotel"),
        ("Crème brûlée", "Creme brulee"),
        ("Ærøskøbing", "AEroskobing"),
        ("Œuvre", "OEuvre"),
        ("ß", "ss"),
        ("Łódź", "Lodz"),
        ("Ångström", "Angstrom"),
        ("ﬁne", "fine"),
        ("Ⅻ", "XII"),
        ("½", "1/2"),
        ("©2024", "(C)2024"),
        ("“quoted”", "\"quoted\""),
        ("‘single’", "'single'"),
        ("e\u{301}", "e"),
        ("cafe\u{301}", "cafe"),
        ("日本語", "日本語"),
        ("", ""),
        (" ", " "),
        ("plain ASCII 123", "plain ASCII 123"),
        ("İstanbul", "Istanbul"),
        ("ñandú", "nandu"),
        ("Ǆ", "DZ"),
        ("ǅ", "Dz"),
        ("ŉ", "'n"),
        ("Þorn", "THorn"),
        ("ð", "d"),
        ("Ǉ", "LJ"),
        ("‐–—", "---"),
        ("…", "..."),
        ("ℌ", "x"),
        ("①②③", "①②③"),
        ("Ａｂｃ", "Abc"),
        ("ǰ", "j"),
        ("ṩ", "s"),
        ("Ẽ", "E"),
        ("Ỹ", "Y"),
        ("ỹ", "y"),
        ("ᵫ", "ue"),
        ("ꜩ", "ꜩ"),
        ("ʼn", "'n"),
        ("\u{300}\u{301}x", "x"),
        ("ÀÁÂÃÄÅ", "AAAAAA"),
        ("Ǎǎ", "Aa"),
        ("ÿ", "y"),
        ("Ÿ", "Y"),
        ("Σίσυφος", "Σισυφος"),
        ("Привет", "Привет"),
        ("😀", "😀"),
        ("a‍b", "a‍b"),
    ];

    #[test]
    fn unaccent_matches_postgres() {
        for (text, want) in UNACCENT {
            assert_eq!(unaccent(text).unwrap(), *want, "unaccent({text:?})");
        }
    }

    /// The binary searches rely on sorted tables.
    #[test]
    fn tables_are_sorted() {
        assert!(unaccent_rules::RULES.windows(2).all(|w| w[0].0 < w[1].0));
        assert!(ctype::WORD_RANGES.windows(2).all(|w| w[0].0 <= w[0].1 && w[0].1 + 1 < w[1].0));
        assert!(ctype::LOWER_RUNS.windows(2).all(|w| w[0].0 <= w[0].1 && w[0].1 < w[1].0));
        assert_eq!(unaccent_rules::RULES.len(), 1650);
    }

    /// Characters the locale tables decide, each one the oracle's own answer: `iswalnum` is true of
    /// letters of every script, of a combining iota and of ASCII digits, but not of superscripts,
    /// other marks or symbols; `towlower` is one character for one, so `İ` becomes `i`, the Kelvin
    /// sign `k` and `ẞ` `ß`.
    #[test]
    fn locale_tables_answer_like_the_oracle() {
        for c in [
            'a', 'Z', '7', 'é', 'Ω', 'ж', '日', 'ا', '\u{345}', '٣', '𝟘', 'ª', 'µ', 'ǅ', 'ⅷ', 'ⓐ', 'ａ', '１',
            '\u{903}', 'ऄ', '\u{948}', '\u{30fc}', '\u{3005}', '〇', 'Ⅰ',
        ] {
            assert!(is_word_char(c), "{c:?}");
        }
        for c in ['_', ' ', '-', '²', '\u{301}', '😀', '€', '\u{2003}', '\u{a0}', '\u{309b}', '·'] {
            assert!(!is_word_char(c), "{c:?}");
        }
        for (from, to) in [
            ('İ', 'i'),
            ('K', 'k'),
            ('ẞ', 'ß'),
            ('Σ', 'σ'),
            ('ς', 'ς'),
            ('ǅ', 'ǆ'),
            ('A', 'a'),
            ('日', '日'),
            ('Å', 'å'),
            ('Ω', 'ω'),
            ('Ÿ', 'ÿ'),
            ('Ǆ', 'ǆ'),
            ('Ⅻ', 'ⅻ'),
            ('Ⓐ', 'ⓐ'),
            ('Ａ', 'ａ'),
            ('Ꙁ', 'ꙁ'),
            ('ᾈ', 'ᾀ'),
            ('Ά', 'ά'),
            ('Ü', 'ü'),
            ('ß', 'ß'),
            ('ÿ', 'ÿ'),
            ('ı', 'ı'),
        ] {
            assert_eq!(lower_char(from), to, "lower_char({from:?})");
        }
    }
}
