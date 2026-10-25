//! src/backend/utils/mb/conv.c: the conversions the conversion functions build on. Between UTF8 and an encoding by
//! a radix tree over the character's bytes (`utf_to_local`, `local_to_utf`, Postgres's UtfToLocal and LocalToUtf),
//! between two single-byte encodings by a table (`local2local`), and to and from MULE_INTERNAL (`latin2mic`,
//! `mic2latin` and their `_with_table` forms). Each writes into `dest` and gives how many bytes of `src` it
//! converted: all of them, or with `no_error` those before the first it couldn't (without, that one is an error).

use super::wchar::{mule_mblen, utf_mblen, utf8_islegal, verifymbchar};
use super::*;

/// A radix tree's table: Postgres's chars16 or chars32, as little-endian bytes (maps/*.bin)
pub(crate) enum Table {
    U16(&'static [u8]),
    U32(&'static [u8]),
}

impl Table {
    fn get(&self, i: usize) -> u32 {
        match self {
            Table::U16(b) => u16::from_le_bytes([b[2 * i], b[2 * i + 1]]) as u32,
            Table::U32(b) => u32::from_le_bytes([b[4 * i], b[4 * i + 1], b[4 * i + 2], b[4 * i + 3]]),
        }
    }
}

/// pg_mb_radix_tree: for characters of 1 to 4 bytes, where in the table the lookup starts (b*root) and the range
/// each byte may be in
pub(crate) struct RadixTree {
    pub table: Table,
    pub b1root: u32,
    pub b1_lower: u8,
    pub b1_upper: u8,
    pub b2root: u32,
    pub b2_1_lower: u8,
    pub b2_1_upper: u8,
    pub b2_2_lower: u8,
    pub b2_2_upper: u8,
    pub b3root: u32,
    pub b3_1_lower: u8,
    pub b3_1_upper: u8,
    pub b3_2_lower: u8,
    pub b3_2_upper: u8,
    pub b3_3_lower: u8,
    pub b3_3_upper: u8,
    pub b4root: u32,
    pub b4_1_lower: u8,
    pub b4_1_upper: u8,
    pub b4_2_lower: u8,
    pub b4_2_upper: u8,
    pub b4_3_lower: u8,
    pub b4_3_upper: u8,
    pub b4_4_lower: u8,
    pub b4_4_upper: u8,
}

/// pg_utf_to_local_combined: two UTF8 characters that are one local character
pub(crate) struct UtfToLocalCombined {
    pub utf1: u32,
    pub utf2: u32,
    pub code: u32,
}

/// pg_local_to_utf_combined: a local character that is two UTF8 characters
pub(crate) struct LocalToUtfCombined {
    pub code: u32,
    pub utf1: u32,
    pub utf2: u32,
}

/// utf_local_conversion_func: an algorithmic conversion of one character, 0 when it has none
pub(crate) type ConvFunc = fn(u32) -> u32;

const HIGHBIT: u8 = 0x80;

fn is_highbit_set(c: u8) -> bool {
    c & HIGHBIT != 0
}

/// local2local: a single-byte encoding to another, both supersets of ASCII; `tab` gives the other's byte for each
/// from 0x80 up (0 for none)
pub(crate) fn local2local(
    src: &[u8],
    dest: &mut Vec<u8>,
    src_encoding: Enc,
    dest_encoding: Enc,
    tab: &[u8; 128],
    no_error: bool,
) -> Result<usize> {
    let mut i = 0;
    while i < src.len() {
        let c1 = src[i];
        if c1 == 0 {
            if no_error {
                break;
            }
            return Err(report_invalid_encoding(src_encoding, &src[i..]));
        }
        if !is_highbit_set(c1) {
            dest.push(c1);
        } else {
            let c2 = tab[(c1 - HIGHBIT) as usize];
            if c2 != 0 {
                dest.push(c2);
            } else {
                if no_error {
                    break;
                }
                return Err(report_untranslatable_char(src_encoding, dest_encoding, &src[i..]));
            }
        }
        i += 1;
    }
    Ok(i)
}

/// latin2mic: a single-byte encoding whose codes are MULE_INTERNAL's own after the charset byte `lc`
pub(crate) fn latin2mic(src: &[u8], dest: &mut Vec<u8>, lc: u8, encoding: Enc, no_error: bool) -> Result<usize> {
    let mut i = 0;
    while i < src.len() {
        let c1 = src[i];
        if c1 == 0 {
            if no_error {
                break;
            }
            return Err(report_invalid_encoding(encoding, &src[i..]));
        }
        if is_highbit_set(c1) {
            dest.push(lc);
        }
        dest.push(c1);
        i += 1;
    }
    Ok(i)
}

/// mic2latin: back from MULE_INTERNAL to such an encoding
pub(crate) fn mic2latin(src: &[u8], dest: &mut Vec<u8>, lc: u8, encoding: Enc, no_error: bool) -> Result<usize> {
    let mut i = 0;
    while i < src.len() {
        let c1 = src[i];
        let len = src.len() - i;
        if c1 == 0 {
            if no_error {
                break;
            }
            return Err(report_invalid_encoding(PG_MULE_INTERNAL, &src[i..]));
        }
        if !is_highbit_set(c1) {
            // easy for ASCII
            dest.push(c1);
            i += 1;
        } else {
            let l = mule_mblen(&src[i..]);
            if len < l {
                if no_error {
                    break;
                }
                return Err(report_invalid_encoding(PG_MULE_INTERNAL, &src[i..]));
            }
            if l != 2 || c1 != lc || !is_highbit_set(src[i + 1]) {
                if no_error {
                    break;
                }
                return Err(report_untranslatable_char(PG_MULE_INTERNAL, encoding, &src[i..]));
            }
            dest.push(src[i + 1]);
            i += 2;
        }
    }
    Ok(i)
}

/// latin2mic_with_table: a single-byte encoding to MULE_INTERNAL, `lc` and the byte `tab` gives for each from 0x80
/// up (0 for none)
pub(crate) fn latin2mic_with_table(
    src: &[u8],
    dest: &mut Vec<u8>,
    lc: u8,
    encoding: Enc,
    tab: &[u8; 128],
    no_error: bool,
) -> Result<usize> {
    let mut i = 0;
    while i < src.len() {
        let c1 = src[i];
        if c1 == 0 {
            if no_error {
                break;
            }
            return Err(report_invalid_encoding(encoding, &src[i..]));
        }
        if !is_highbit_set(c1) {
            dest.push(c1);
        } else {
            let c2 = tab[(c1 - HIGHBIT) as usize];
            if c2 != 0 {
                dest.push(lc);
                dest.push(c2);
            } else {
                if no_error {
                    break;
                }
                return Err(report_untranslatable_char(encoding, PG_MULE_INTERNAL, &src[i..]));
            }
        }
        i += 1;
    }
    Ok(i)
}

/// mic2latin_with_table: MULE_INTERNAL to a single-byte encoding; `tab` gives the local byte for each second byte
/// from 0x80 up (0 for none)
pub(crate) fn mic2latin_with_table(
    src: &[u8],
    dest: &mut Vec<u8>,
    lc: u8,
    encoding: Enc,
    tab: &[u8; 128],
    no_error: bool,
) -> Result<usize> {
    let mut i = 0;
    while i < src.len() {
        let c1 = src[i];
        let len = src.len() - i;
        if c1 == 0 {
            if no_error {
                break;
            }
            return Err(report_invalid_encoding(PG_MULE_INTERNAL, &src[i..]));
        }
        if !is_highbit_set(c1) {
            dest.push(c1);
            i += 1;
        } else {
            let l = mule_mblen(&src[i..]);
            if len < l {
                if no_error {
                    break;
                }
                return Err(report_invalid_encoding(PG_MULE_INTERNAL, &src[i..]));
            }
            let c2 =
                if l == 2 && c1 == lc && is_highbit_set(src[i + 1]) { tab[(src[i + 1] - HIGHBIT) as usize] } else { 0 };
            if c2 == 0 {
                if no_error {
                    break;
                }
                return Err(report_untranslatable_char(PG_MULE_INTERNAL, encoding, &src[i..]));
            }
            dest.push(c2);
            i += 2;
        }
    }
    Ok(i)
}

/// store_coded_char: a character's code as its bytes, high to low, leaving out its zero ones
fn store_coded_char(dest: &mut Vec<u8>, code: u32) {
    if code & 0xff00_0000 != 0 {
        dest.push((code >> 24) as u8);
    }
    if code & 0x00ff_0000 != 0 {
        dest.push((code >> 16) as u8);
    }
    if code & 0x0000_ff00 != 0 {
        dest.push((code >> 8) as u8);
    }
    if code & 0x0000_00ff != 0 {
        dest.push(code as u8);
    }
}

/// pg_mb_radix_conv: the character of `l` bytes (`b1`..`b4`, the last `l` of them used) through the tree, 0 for
/// none
fn radix_conv(rt: &RadixTree, l: usize, b1: u8, b2: u8, b3: u8, b4: u8) -> u32 {
    let t = &rt.table;
    let get = |i: u32| t.get(i as usize);
    match l {
        4 => {
            if b1 < rt.b4_1_lower
                || b1 > rt.b4_1_upper
                || b2 < rt.b4_2_lower
                || b2 > rt.b4_2_upper
                || b3 < rt.b4_3_lower
                || b3 > rt.b4_3_upper
                || b4 < rt.b4_4_lower
                || b4 > rt.b4_4_upper
            {
                return 0;
            }
            let mut idx = rt.b4root;
            idx = get(b1 as u32 + idx - rt.b4_1_lower as u32);
            idx = get(b2 as u32 + idx - rt.b4_2_lower as u32);
            idx = get(b3 as u32 + idx - rt.b4_3_lower as u32);
            get(b4 as u32 + idx - rt.b4_4_lower as u32)
        }
        3 => {
            if b2 < rt.b3_1_lower
                || b2 > rt.b3_1_upper
                || b3 < rt.b3_2_lower
                || b3 > rt.b3_2_upper
                || b4 < rt.b3_3_lower
                || b4 > rt.b3_3_upper
            {
                return 0;
            }
            let mut idx = rt.b3root;
            idx = get(b2 as u32 + idx - rt.b3_1_lower as u32);
            idx = get(b3 as u32 + idx - rt.b3_2_lower as u32);
            get(b4 as u32 + idx - rt.b3_3_lower as u32)
        }
        2 => {
            if b3 < rt.b2_1_lower || b3 > rt.b2_1_upper || b4 < rt.b2_2_lower || b4 > rt.b2_2_upper {
                return 0;
            }
            let idx = get(b3 as u32 + rt.b2root - rt.b2_1_lower as u32);
            get(b4 as u32 + idx - rt.b2_2_lower as u32)
        }
        1 => {
            if b4 < rt.b1_lower || b4 > rt.b1_upper {
                return 0;
            }
            get(b4 as u32 + rt.b1root - rt.b1_lower as u32)
        }
        _ => 0, // shouldn't happen
    }
}

/// The code of a character of `l` bytes from `s`, as UtfToLocal and LocalToUtf collect it (b1..b4, high to low)
fn collect(s: &[u8], l: usize) -> (u8, u8, u8, u8) {
    match l {
        1 => (0, 0, 0, s[0]),
        2 => (0, 0, s[0], s[1]),
        3 => (0, s[0], s[1], s[2]),
        _ => (s[0], s[1], s[2], s[3]),
    }
}

fn code_of(b1: u8, b2: u8, b3: u8, b4: u8) -> u32 {
    (b1 as u32) << 24 | (b2 as u32) << 16 | (b3 as u32) << 8 | b4 as u32
}

/// UtfToLocal: UTF8 to `encoding`. For each character the combined map first (when there is one, with the next
/// character), then the tree, then `conv_func`; an untranslatable character if none has it.
pub(crate) fn utf_to_local(
    src: &[u8],
    dest: &mut Vec<u8>,
    map: Option<&RadixTree>,
    cmap: &[UtfToLocalCombined],
    conv_func: Option<ConvFunc>,
    encoding: Enc,
    no_error: bool,
) -> Result<usize> {
    let mut i = 0;
    // "break" cases all represent errors
    'chars: while i < src.len() {
        let len = src.len() - i;
        if src[i] == 0 {
            break;
        }
        let l = utf_mblen(&src[i..]);
        if len < l || !utf8_islegal(&src[i..i + l]) {
            break;
        }
        if l == 1 {
            // ASCII case is easy, assume it's one-to-one conversion
            dest.push(src[i]);
            i += 1;
            continue;
        }
        let (b1, b2, b3, b4) = collect(&src[i..], l);
        let iutf = code_of(b1, b2, b3, b4);

        // first, try the combined map if possible
        if !cmap.is_empty() && len > l {
            let next = i + l;
            let rest = src.len() - next;
            let l2 = utf_mblen(&src[next..]);
            if rest < l2 {
                // need more data to decide if this is a combined char. The C reports it at the first character
                // but with the length left after it (its `len` was already taken down), so the message shows at
                // most that many of the first character's bytes.
                if !no_error {
                    return Err(report_invalid_encoding(PG_UTF8, &src[i..i + rest]));
                }
                break;
            }
            if !utf8_islegal(&src[next..next + l2]) {
                if !no_error {
                    return Err(report_invalid_encoding(PG_UTF8, &src[next..]));
                }
                break;
            }
            // an ASCII character can't be in the combined map
            if l2 > 1 {
                let (c1, c2, c3, c4) = collect(&src[next..], l2);
                let iutf2 = code_of(c1, c2, c3, c4);
                let found = cmap.binary_search_by(|e| (e.utf1, e.utf2).cmp(&(iutf, iutf2)));
                if let Ok(k) = found {
                    store_coded_char(dest, cmap[k].code);
                    i = next + l2;
                    continue 'chars;
                }
            }
            // fail, so the second character is next
        }

        // now the ordinary map
        if let Some(map) = map {
            let converted = radix_conv(map, l, b1, b2, b3, b4);
            if converted != 0 {
                store_coded_char(dest, converted);
                i += l;
                continue;
            }
        }
        // if there's a conversion function, try that
        if let Some(f) = conv_func {
            let converted = f(iutf);
            if converted != 0 {
                store_coded_char(dest, converted);
                i += l;
                continue;
            }
        }
        // failed to translate this character
        if no_error {
            break;
        }
        return Err(report_untranslatable_char(PG_UTF8, encoding, &src[i..]));
    }
    // if we broke out of the loop early, the input is invalid
    if i < src.len() && !no_error {
        return Err(report_invalid_encoding(PG_UTF8, &src[i..]));
    }
    Ok(i)
}

/// LocalToUtf: `encoding` to UTF8. For each character the tree first, then the combined map (when there is one),
/// then `conv_func`; an untranslatable character if none has it.
pub(crate) fn local_to_utf(
    src: &[u8],
    dest: &mut Vec<u8>,
    map: Option<&RadixTree>,
    cmap: &[LocalToUtfCombined],
    conv_func: Option<ConvFunc>,
    encoding: Enc,
    no_error: bool,
) -> Result<usize> {
    let mut i = 0;
    while i < src.len() {
        if src[i] == 0 {
            break;
        }
        if !is_highbit_set(src[i]) {
            // ASCII case is easy, assume it's one-to-one conversion
            dest.push(src[i]);
            i += 1;
            continue;
        }
        let Some(l) = verifymbchar(encoding, &src[i..]) else {
            break;
        };
        let (b1, b2, b3, b4) = collect(&src[i..], l);
        let iiso = code_of(b1, b2, b3, b4);
        if let Some(map) = map {
            let converted = radix_conv(map, l, b1, b2, b3, b4);
            if converted != 0 {
                store_coded_char(dest, converted);
                i += l;
                continue;
            }
            // if there's a combined character map, try that
            if let Ok(k) = cmap.binary_search_by(|e| e.code.cmp(&iiso)) {
                store_coded_char(dest, cmap[k].utf1);
                store_coded_char(dest, cmap[k].utf2);
                i += l;
                continue;
            }
        }
        // if there's a conversion function, try that
        if let Some(f) = conv_func {
            let converted = f(iiso);
            if converted != 0 {
                store_coded_char(dest, converted);
                i += l;
                continue;
            }
        }
        // failed to translate this character
        if no_error {
            break;
        }
        return Err(report_untranslatable_char(encoding, PG_UTF8, &src[i..]));
    }
    // if we broke out of the loop early, the input is invalid
    if i < src.len() && !no_error {
        return Err(report_invalid_encoding(encoding, &src[i..]));
    }
    Ok(i)
}
