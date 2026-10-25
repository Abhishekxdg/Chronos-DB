//! src/common/wchar.c: each encoding's character length (`mblen`), and its verifiers (`verifychar`, `verifystr`)
//! with the C's exact limits. A string is a slice: the C's `len` is its length, and where the C reads a byte past
//! `len` (its strings end in a zero byte), `at` gives 0.

use super::*;

/// The byte at `i`, or the zero byte that ends a C string
fn at(s: &[u8], i: usize) -> u8 {
    s.get(i).copied().unwrap_or(0)
}

const SS2: u8 = 0x8e;
const SS3: u8 = 0x8f;

/// NONUTF8_INVALID_BYTE0 and _BYTE1: a two-byte character no multibyte encoding but UTF8 accepts
const NONUTF8_INVALID_BYTE0: u8 = 0x8d;
const NONUTF8_INVALID_BYTE1: u8 = b' ';

fn is_highbit_set(c: u8) -> bool {
    c & 0x80 != 0
}

/// IS_EUC_RANGE_VALID
fn is_euc_range_valid(c: u8) -> bool {
    (0xa1..=0xfe).contains(&c)
}

/// ISSJISHEAD and ISSJISTAIL
fn is_sjis_head(c: u8) -> bool {
    (0x81..=0x9f).contains(&c) || (0xe0..=0xfc).contains(&c)
}
fn is_sjis_tail(c: u8) -> bool {
    (0x40..=0x7e).contains(&c) || (0x80..=0xfc).contains(&c)
}

/// IS_LC1, IS_LCPRV1, IS_LC2, IS_LCPRV2: the MULE_INTERNAL leading bytes
pub(super) fn is_lc1(c: u8) -> bool {
    (0x81..=0x8d).contains(&c)
}
pub(super) fn is_lcprv1(c: u8) -> bool {
    c == 0x9a || c == 0x9b
}
pub(super) fn is_lc2(c: u8) -> bool {
    (0x90..=0x99).contains(&c)
}
pub(super) fn is_lcprv2(c: u8) -> bool {
    c == 0x9c || c == 0x9d
}

/// pg_euc_mblen
fn euc_mblen(s: &[u8]) -> usize {
    match at(s, 0) {
        SS2 => 2,
        SS3 => 3,
        c if is_highbit_set(c) => 2,
        _ => 1,
    }
}

/// pg_euccn_mblen
fn euccn_mblen(s: &[u8]) -> usize {
    match at(s, 0) {
        SS2 | SS3 => 3,
        c if is_highbit_set(c) => 2,
        _ => 1,
    }
}

/// pg_euctw_mblen
fn euctw_mblen(s: &[u8]) -> usize {
    match at(s, 0) {
        SS2 => 4,
        SS3 => 3,
        c if is_highbit_set(c) => 2,
        _ => 1,
    }
}

/// pg_utf_mblen
pub(super) fn utf_mblen(s: &[u8]) -> usize {
    let c = at(s, 0);
    if c & 0x80 == 0 {
        1
    } else if c & 0xe0 == 0xc0 {
        2
    } else if c & 0xf0 == 0xe0 {
        3
    } else if c & 0xf8 == 0xf0 {
        4
    } else {
        1
    }
}

/// pg_mule_mblen
pub(super) fn mule_mblen(s: &[u8]) -> usize {
    let c = at(s, 0);
    if is_lc1(c) {
        2
    } else if is_lcprv1(c) || is_lc2(c) {
        3
    } else if is_lcprv2(c) {
        4
    } else {
        1 // assume ASCII
    }
}

/// pg_sjis_mblen
fn sjis_mblen(s: &[u8]) -> usize {
    match at(s, 0) {
        0xa1..=0xdf => 1, // 1 byte kana
        c if is_highbit_set(c) => 2,
        _ => 1,
    }
}

/// pg_big5_mblen, pg_gbk_mblen and pg_uhc_mblen
fn highbit2_mblen(s: &[u8]) -> usize {
    if is_highbit_set(at(s, 0)) { 2 } else { 1 }
}

/// pg_gb18030_mblen: looks at the second byte too
fn gb18030_mblen(s: &[u8]) -> usize {
    if !is_highbit_set(at(s, 0)) {
        1
    } else if (0x30..=0x39).contains(&at(s, 1)) {
        4
    } else {
        2
    }
}

/// pg_encoding_mblen: the length of the character `s` starts with, by its first byte (GB18030: its first two)
pub(super) fn mblen(encoding: Enc, s: &[u8]) -> usize {
    match encoding {
        PG_EUC_JP | PG_EUC_KR | PG_EUC_JIS_2004 | PG_JOHAB => euc_mblen(s),
        PG_EUC_CN => euccn_mblen(s),
        PG_EUC_TW => euctw_mblen(s),
        PG_UTF8 => utf_mblen(s),
        PG_MULE_INTERNAL => mule_mblen(s),
        PG_SJIS | PG_SHIFT_JIS_2004 => sjis_mblen(s),
        PG_BIG5 | PG_GBK | PG_UHC => highbit2_mblen(s),
        PG_GB18030 => gb18030_mblen(s),
        _ => 1, // SQL_ASCII and the single-byte encodings
    }
}

/// pg_encoding_mblen_or_incomplete: as `mblen`, but usize::MAX (the C's INT_MAX) when too few bytes are left to
/// tell
pub(super) fn mblen_or_incomplete(encoding: Enc, s: &[u8]) -> usize {
    if s.is_empty() || (encoding == PG_GB18030 && is_highbit_set(s[0]) && s.len() < 2) {
        return usize::MAX;
    }
    mblen(encoding, s)
}

/// pg_encoding_max_length: the most bytes a character of `encoding` takes
pub(crate) fn max_length(encoding: Enc) -> usize {
    match encoding {
        PG_EUC_JP | PG_EUC_CN | PG_EUC_KR | PG_EUC_JIS_2004 | PG_JOHAB => 3,
        PG_EUC_TW | PG_UTF8 | PG_MULE_INTERNAL | PG_GB18030 => 4,
        PG_SJIS | PG_BIG5 | PG_GBK | PG_UHC | PG_SHIFT_JIS_2004 => 2,
        _ => 1,
    }
}

/// pg_encoding_set_invalid: two bytes that start a character of two in `encoding` and are never valid
pub(super) fn set_invalid(encoding: Enc) -> [u8; 2] {
    [if encoding == PG_UTF8 { 0xc0 } else { NONUTF8_INVALID_BYTE0 }, NONUTF8_INVALID_BYTE1]
}

/// pg_eucjp_verifychar (EUC_JP and EUC_JIS_2004): the length of the valid character `s` starts with, or None
fn eucjp_verifychar(s: &[u8]) -> Option<usize> {
    let len = s.len();
    let c1 = at(s, 0);
    match c1 {
        SS2 => {
            // JIS X 0201
            if 2 > len {
                return None;
            }
            let c2 = at(s, 1);
            if !(0xa1..=0xdf).contains(&c2) {
                return None;
            }
            Some(2)
        }
        SS3 => {
            // JIS X 0212
            if 3 > len || !is_euc_range_valid(at(s, 1)) || !is_euc_range_valid(at(s, 2)) {
                return None;
            }
            Some(3)
        }
        _ if is_highbit_set(c1) => {
            // JIS X 0208
            if 2 > len || !is_euc_range_valid(c1) || !is_euc_range_valid(at(s, 1)) {
                return None;
            }
            Some(2)
        }
        _ => Some(1), // must be ASCII
    }
}

/// pg_euckr_verifychar (EUC_KR and EUC_CN)
fn euckr_verifychar(s: &[u8]) -> Option<usize> {
    let c1 = at(s, 0);
    if is_highbit_set(c1) {
        if 2 > s.len() || !is_euc_range_valid(c1) || !is_euc_range_valid(at(s, 1)) {
            return None;
        }
        Some(2)
    } else {
        Some(1) // must be ASCII
    }
}

/// pg_euctw_verifychar
fn euctw_verifychar(s: &[u8]) -> Option<usize> {
    let c1 = at(s, 0);
    match c1 {
        SS2 => {
            // CNS 11643 Plane 1-7
            if 4 > s.len()
                || !(0xa1..=0xa7).contains(&at(s, 1))
                || !is_euc_range_valid(at(s, 2))
                || !is_euc_range_valid(at(s, 3))
            {
                return None;
            }
            Some(4)
        }
        SS3 => None, // unused
        _ if is_highbit_set(c1) => {
            // CNS 11643 Plane 1 (no range check on c1)
            if 2 > s.len() || !is_euc_range_valid(at(s, 1)) {
                return None;
            }
            Some(2)
        }
        _ => Some(1), // must be ASCII
    }
}

/// pg_johab_verifychar
fn johab_verifychar(s: &[u8]) -> Option<usize> {
    let mbl = euc_mblen(s);
    if s.len() < mbl {
        return None;
    }
    if !is_highbit_set(at(s, 0)) {
        return Some(mbl);
    }
    if (1..mbl).any(|i| !is_euc_range_valid(at(s, i))) {
        return None;
    }
    Some(mbl)
}

/// pg_mule_verifychar
fn mule_verifychar(s: &[u8]) -> Option<usize> {
    let mbl = mule_mblen(s);
    if s.len() < mbl {
        return None;
    }
    if (1..mbl).any(|i| !is_highbit_set(at(s, i))) {
        return None;
    }
    Some(mbl)
}

/// pg_sjis_verifychar (SJIS and SHIFT_JIS_2004)
fn sjis_verifychar(s: &[u8]) -> Option<usize> {
    let mbl = sjis_mblen(s);
    if s.len() < mbl {
        return None;
    }
    if mbl == 1 {
        return Some(mbl); // pg_sjis_mblen already verified it
    }
    if !is_sjis_head(at(s, 0)) || !is_sjis_tail(at(s, 1)) {
        return None;
    }
    Some(mbl)
}

/// pg_big5_verifychar, pg_gbk_verifychar and pg_uhc_verifychar: any non-zero bytes, except the one pair
/// NONUTF8_INVALID_BYTE0 and _BYTE1
fn highbit2_verifychar(s: &[u8]) -> Option<usize> {
    let mbl = highbit2_mblen(s);
    if s.len() < mbl {
        return None;
    }
    if mbl == 2 && at(s, 0) == NONUTF8_INVALID_BYTE0 && at(s, 1) == NONUTF8_INVALID_BYTE1 {
        return None;
    }
    if (1..mbl).any(|i| at(s, i) == 0) {
        return None;
    }
    Some(mbl)
}

/// pg_gb18030_verifychar
fn gb18030_verifychar(s: &[u8]) -> Option<usize> {
    let len = s.len();
    let c = at(s, 0);
    if !is_highbit_set(c) {
        Some(1) // ASCII
    } else if len >= 4 && (0x30..=0x39).contains(&at(s, 1)) {
        // should be 4-byte: validate the remaining bytes
        if (0x81..=0xfe).contains(&c) && (0x81..=0xfe).contains(&at(s, 2)) && (0x30..=0x39).contains(&at(s, 3)) {
            Some(4)
        } else {
            None
        }
    } else if len >= 2 && (0x81..=0xfe).contains(&c) {
        // should be 2-byte
        let c2 = at(s, 1);
        if (0x40..=0x7e).contains(&c2) || (0x80..=0xfe).contains(&c2) { Some(2) } else { None }
    } else {
        None
    }
}

/// pg_utf8_verifychar
fn utf8_verifychar(s: &[u8]) -> Option<usize> {
    let c = at(s, 0);
    let l = if c & 0x80 == 0 {
        if c == 0 {
            return None;
        }
        return Some(1);
    } else if c & 0xe0 == 0xc0 {
        2
    } else if c & 0xf0 == 0xe0 {
        3
    } else if c & 0xf8 == 0xf0 {
        4
    } else {
        1
    };
    if l > s.len() || !utf8_islegal(&s[..l]) {
        return None;
    }
    Some(l)
}

/// pg_utf8_islegal: whether `source` (one character, its whole length) is legal UTF8: no overlong form, no
/// surrogate, nothing past U+10FFFF
pub(super) fn utf8_islegal(source: &[u8]) -> bool {
    let length = source.len();
    if !(1..=4).contains(&length) {
        return false;
    }
    if length == 4 && !(0x80..=0xbf).contains(&source[3]) {
        return false;
    }
    if length >= 3 && !(0x80..=0xbf).contains(&source[2]) {
        return false;
    }
    if length >= 2 {
        let a = source[1];
        let ok = match source[0] {
            0xe0 => (0xa0..=0xbf).contains(&a),
            0xed => (0x80..=0x9f).contains(&a),
            0xf0 => (0x90..=0xbf).contains(&a),
            0xf4 => (0x80..=0x8f).contains(&a),
            _ => (0x80..=0xbf).contains(&a),
        };
        if !ok {
            return false;
        }
    }
    let a = source[0];
    !((0x80..0xc2).contains(&a) || a > 0xf4)
}

/// pg_encoding_verifymbchar: the length of the valid character `s` starts with, or None. (In the single-byte
/// encodings and SQL_ASCII, any byte is a character of one.)
pub(super) fn verifymbchar(encoding: Enc, s: &[u8]) -> Option<usize> {
    match encoding {
        PG_EUC_JP | PG_EUC_JIS_2004 => eucjp_verifychar(s),
        PG_EUC_KR | PG_EUC_CN => euckr_verifychar(s),
        PG_EUC_TW => euctw_verifychar(s),
        PG_JOHAB => johab_verifychar(s),
        PG_UTF8 => utf8_verifychar(s),
        PG_MULE_INTERNAL => mule_verifychar(s),
        PG_SJIS | PG_SHIFT_JIS_2004 => sjis_verifychar(s),
        PG_BIG5 | PG_GBK | PG_UHC => highbit2_verifychar(s),
        PG_GB18030 => gb18030_verifychar(s),
        _ => Some(1),
    }
}

/// pg_encoding_verifymbstr: how many bytes at the start of `s` form a valid string in `encoding` (all of it, when
/// it is). A zero byte is never valid. The C's UTF8 verifier takes a faster path over long strings that gives the
/// same answer; this one takes the character-by-character path always.
pub(crate) fn verifymbstr(encoding: Enc, s: &[u8]) -> usize {
    if max_length(encoding) == 1 {
        // pg_ascii_verifystr and pg_latin1_verifystr: up to the first zero byte
        return s.iter().position(|&b| b == 0).unwrap_or(s.len());
    }
    let mut i = 0;
    while i < s.len() {
        let l = if !is_highbit_set(s[i]) {
            // fast path for ASCII-subset characters
            if s[i] == 0 {
                break;
            }
            1
        } else {
            match verifymbchar(encoding, &s[i..]) {
                Some(l) => l,
                None => break,
            }
        };
        i += l;
    }
    i
}
