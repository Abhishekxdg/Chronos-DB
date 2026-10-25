//! conversion_procs/euc_jp_and_sjis/euc_jp_and_sjis.c: EUC_JP, SJIS and MULE_INTERNAL, with sjis.map's IBM kanji.
//! The C's int arithmetic is i32 here; a byte it writes is the int truncated, as its `*p++ = ...` does.

use super::super::wchar::verifymbchar;
use super::*;

const PGSJISALTCODE: i32 = 0x81ac;
const PGEUCALTCODE: i32 = 0xa2ae;

fn is_sjis_head(c: i32) -> bool {
    (0x81..=0x9f).contains(&c) || (0xe0..=0xfc).contains(&c)
}
fn is_sjis_tail(c: i32) -> bool {
    (0x40..=0x7e).contains(&c) || (0x80..=0xfc).contains(&c)
}

fn put(p: &mut Vec<u8>, v: i32) {
    p.push(v as u8);
}

fn b(x: bool) -> i32 {
    x as i32
}

/// The bytes a JIS X0208 or X0212 character takes from its SJIS bytes (sjis2mic's and sjis2euc_jp's arithmetic)
fn kanji(p: &mut Vec<u8>, c1: i32, c2: i32, base: i32) {
    put(p, ((c1 & 0x3f) << 1) + base + b(c2 > 0x9e));
    put(p, c2 + if c2 > 0x9e { 2 } else { 0x60 } + b(c2 < 0x80));
}

/// The SJIS bytes of an EUC/MIC kanji pair, UDC (`udc` the offset added to the first byte) or not
fn to_sjis(p: &mut Vec<u8>, mut c1: i32, c2: i32, udc: Option<i32>) {
    match udc {
        Some(off) => {
            c1 -= 0x54;
            put(p, ((c1 - 0xa1) >> 1) + if c1 < 0xdf { 0x81 } else { 0xc1 } + off);
        }
        None => put(p, ((c1 - 0xa1) >> 1) + if c1 < 0xdf { 0x81 } else { 0xc1 }),
    }
    put(p, c2 - if c1 & 1 != 0 { if c2 < 0xe0 { 0x61 } else { 0x60 } } else { 2 });
}

/// The SJIS code ibmkanji gives a JIS X0212 code (its EUC's low two bytes), or PGSJISALTCODE
fn ibm_to_sjis(p: &mut Vec<u8>, k: i32) {
    for e in ibmkanji.iter() {
        let k2 = (e.euc & 0xffff) as i32;
        if k2 == 0xffff {
            put(p, PGSJISALTCODE >> 8);
            put(p, PGSJISALTCODE & 0xff);
            return;
        }
        if k2 == k {
            put(p, e.sjis as i32 >> 8);
            put(p, e.sjis as i32 & 0xff);
            return;
        }
    }
}

/// The JIS X0208 or X0212 character an SJIS pair is, written with `lead` (MIC's charset byte, or EUC's), as
/// sjis2mic and sjis2euc_jp share it. `mic`: MIC's form (its charset bytes) rather than EUC's (SS3 for X0212).
fn sjis_kanji(p: &mut Vec<u8>, mut c1: i32, mut c2: i32, mic: bool) {
    let mut k = (c1 << 8) + c2;
    if (0xed40..0xf040).contains(&k) {
        // NEC selection IBM kanji (the C's loop doesn't stop at a match)
        for e in ibmkanji.iter() {
            let k2 = e.nec as i32;
            if k2 == 0xffff {
                break;
            }
            if k2 == k {
                k = e.sjis as i32;
                c1 = (k >> 8) & 0xff;
                c2 = k & 0xff;
            }
        }
    }
    if k < 0xeb3f {
        // JIS X0208
        if mic {
            p.push(LC_JISX0208);
        }
        kanji(p, c1, c2, 0x9f);
    } else if (0xeb40..0xf040).contains(&k) || (0xfc4c..=0xfcfc).contains(&k) {
        // NEC selection IBM kanji - Other undecided justice
        if mic {
            p.push(LC_JISX0208);
        }
        put(p, PGEUCALTCODE >> 8);
        put(p, PGEUCALTCODE & 0xff);
    } else if (0xf040..0xf540).contains(&k) {
        // UDC1 mapping to X0208 85 ku - 94 ku
        if mic {
            p.push(LC_JISX0208);
        }
        kanji(p, c1 - 0x6f, c2, 0xf3);
    } else if (0xf540..0xfa40).contains(&k) {
        // UDC2 mapping to X0212 85 ku - 94 ku
        p.push(if mic { LC_JISX0212 } else { SS3 });
        kanji(p, c1 - 0x74, c2, 0xf3);
    } else if k >= 0xfa40 {
        // mapping IBM kanji to X0208 and X0212 (the C's loop doesn't stop at a match)
        for e in ibmkanji.iter() {
            let k2 = e.sjis as i32;
            if k2 == 0xffff {
                break;
            }
            if k2 == k {
                k = e.euc as i32;
                if k >= 0x8f0000 {
                    p.push(if mic { LC_JISX0212 } else { SS3 });
                    put(p, 0x80 | ((k & 0xff00) >> 8));
                } else {
                    if mic {
                        p.push(LC_JISX0208);
                    }
                    put(p, 0x80 | (k >> 8));
                }
                put(p, 0x80 | (k & 0xff));
            }
        }
    }
    // k == 0xeb3f: nothing, as in the C
}

/// sjis2mic: SJIS to MULE_INTERNAL
pub(super) fn sjis2mic(src: &[u8], p: &mut Vec<u8>, no_error: bool) -> Result<usize> {
    let mut i = 0;
    while i < src.len() {
        let len = src.len() - i;
        let c1 = src[i] as i32;
        if (0xa1..=0xdf).contains(&c1) {
            // JIS X0201 (1 byte kana)
            p.push(LC_JISX0201K);
            put(p, c1);
            i += 1;
        } else if c1 & 0x80 != 0 {
            // JIS X0208, X0212, user defined extended characters
            if len < 2 || !is_sjis_head(c1) || !is_sjis_tail(src[i + 1] as i32) {
                if no_error {
                    break;
                }
                return Err(report_invalid_encoding(PG_SJIS, &src[i..]));
            }
            sjis_kanji(p, c1, src[i + 1] as i32, true);
            i += 2;
        } else {
            // should be ASCII
            if c1 == 0 {
                if no_error {
                    break;
                }
                return Err(report_invalid_encoding(PG_SJIS, &src[i..]));
            }
            put(p, c1);
            i += 1;
        }
    }
    Ok(i)
}

/// The loop the MIC and EUC_JP functions share: ASCII as it is, a zero byte invalid, each other character
/// verified in `encoding` and handed to `each` (its bytes); `each` gives false for a character it can't convert
fn each_char(
    src: &[u8],
    p: &mut Vec<u8>,
    encoding: Enc,
    dest_encoding: Enc,
    no_error: bool,
    mut each: impl FnMut(&mut Vec<u8>, &[u8]) -> bool,
) -> Result<usize> {
    let mut i = 0;
    while i < src.len() {
        let c1 = src[i];
        if c1 & 0x80 == 0 {
            if c1 == 0 {
                if no_error {
                    break;
                }
                return Err(report_invalid_encoding(encoding, &src[i..]));
            }
            p.push(c1);
            i += 1;
            continue;
        }
        let Some(l) = verifymbchar(encoding, &src[i..]) else {
            if no_error {
                break;
            }
            return Err(report_invalid_encoding(encoding, &src[i..]));
        };
        if !each(p, &src[i..i + l]) {
            if no_error {
                break;
            }
            return Err(report_untranslatable_char(encoding, dest_encoding, &src[i..]));
        }
        i += l;
    }
    Ok(i)
}

/// mic2sjis: MULE_INTERNAL to SJIS
pub(super) fn mic2sjis(src: &[u8], p: &mut Vec<u8>, no_error: bool) -> Result<usize> {
    each_char(src, p, PG_MULE_INTERNAL, PG_SJIS, no_error, |p, mic| {
        let c1 = mic[0];
        if c1 == LC_JISX0201K {
            p.push(mic[1]);
        } else if c1 == LC_JISX0208 {
            let (c1, c2) = (mic[1] as i32, mic[2] as i32);
            let k = (c1 << 8) | (c2 & 0xff);
            to_sjis(p, c1, c2, (k >= 0xf5a1).then_some(0x6f)); // UDC1
        } else if c1 == LC_JISX0212 {
            let (c1, c2) = (mic[1] as i32, mic[2] as i32);
            let k = (c1 << 8) | c2;
            if k >= 0xf5a1 {
                to_sjis(p, c1, c2, Some(0x74)); // UDC2
            } else {
                ibm_to_sjis(p, k); // IBM kanji
            }
        } else {
            return false;
        }
        true
    })
}

/// euc_jp2mic: EUC_JP to MULE_INTERNAL
pub(super) fn euc_jp2mic(src: &[u8], p: &mut Vec<u8>, no_error: bool) -> Result<usize> {
    each_char(src, p, PG_EUC_JP, PG_MULE_INTERNAL, no_error, |p, euc| {
        match euc[0] {
            SS2 => p.extend([LC_JISX0201K, euc[1]]),        // 1 byte kana
            SS3 => p.extend([LC_JISX0212, euc[1], euc[2]]), // JIS X0212 kanji
            c1 => p.extend([LC_JISX0208, c1, euc[1]]),      // kanji
        }
        true
    })
}

/// mic2euc_jp: MULE_INTERNAL to EUC_JP
pub(super) fn mic2euc_jp(src: &[u8], p: &mut Vec<u8>, no_error: bool) -> Result<usize> {
    each_char(src, p, PG_MULE_INTERNAL, PG_EUC_JP, no_error, |p, mic| {
        match mic[0] {
            LC_JISX0201K => p.extend([SS2, mic[1]]),
            LC_JISX0212 => p.extend([SS3, mic[1], mic[2]]),
            LC_JISX0208 => p.extend([mic[1], mic[2]]),
            _ => return false,
        }
        true
    })
}

/// euc_jp2sjis: EUC_JP to SJIS (every verified character converts)
pub(super) fn euc_jp2sjis(src: &[u8], p: &mut Vec<u8>, no_error: bool) -> Result<usize> {
    each_char(src, p, PG_EUC_JP, PG_SJIS, no_error, |p, euc| {
        let c1 = euc[0];
        if c1 == SS2 {
            p.push(euc[1]); // hankaku kana
        } else if c1 == SS3 {
            // JIS X0212 kanji
            let (c1, c2) = (euc[1] as i32, euc[2] as i32);
            let k = (c1 << 8) | c2;
            if k >= 0xf5a1 {
                to_sjis(p, c1, c2, Some(0x74)); // UDC2
            } else {
                ibm_to_sjis(p, k); // IBM kanji
            }
        } else {
            // JIS X0208 kanji
            let (c1, c2) = (c1 as i32, euc[1] as i32);
            let k = (c1 << 8) | (c2 & 0xff);
            to_sjis(p, c1, c2, (k >= 0xf5a1).then_some(0x6f)); // UDC1
        }
        true
    })
}

/// sjis2euc_jp: SJIS to EUC_JP (every verified character converts)
pub(super) fn sjis2euc_jp(src: &[u8], p: &mut Vec<u8>, no_error: bool) -> Result<usize> {
    each_char(src, p, PG_SJIS, PG_EUC_JP, no_error, |p, sjis| {
        let c1 = sjis[0] as i32;
        if (0xa1..=0xdf).contains(&c1) {
            // JIS X0201 (1 byte kana)
            p.push(SS2);
            put(p, c1);
        } else {
            sjis_kanji(p, c1, sjis[1] as i32, false);
        }
        true
    })
}
