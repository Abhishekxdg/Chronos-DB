//! The conversions between UTF8 and the other encodings (conversion_procs/utf8_and_*): each a call of UtfToLocal
//! or LocalToUtf with its maps, but for LATIN1 (utf8_and_iso8859_1.c, by arithmetic) and GB18030
//! (utf8_and_gb18030.c, whose four-byte ranges are converted by arithmetic after the map).

use super::super::conv::{RadixTree, local_to_utf, utf_to_local};
use super::super::maps::*;
use super::super::wchar::{utf_mblen, utf8_islegal};
use super::*;

/// The functions here, by name
pub(super) const NAMES: [&str; 34] = [
    "big5_to_utf8",
    "utf8_to_big5",
    "utf8_to_koi8r",
    "koi8r_to_utf8",
    "utf8_to_koi8u",
    "koi8u_to_utf8",
    "euc_jis_2004_to_utf8",
    "utf8_to_euc_jis_2004",
    "euc_cn_to_utf8",
    "utf8_to_euc_cn",
    "euc_jp_to_utf8",
    "utf8_to_euc_jp",
    "euc_kr_to_utf8",
    "utf8_to_euc_kr",
    "euc_tw_to_utf8",
    "utf8_to_euc_tw",
    "gb18030_to_utf8",
    "utf8_to_gb18030",
    "gbk_to_utf8",
    "utf8_to_gbk",
    "iso8859_to_utf8",
    "utf8_to_iso8859",
    "iso8859_1_to_utf8",
    "utf8_to_iso8859_1",
    "johab_to_utf8",
    "utf8_to_johab",
    "sjis_to_utf8",
    "utf8_to_sjis",
    "shift_jis_2004_to_utf8",
    "utf8_to_shift_jis_2004",
    "uhc_to_utf8",
    "utf8_to_uhc",
    "win_to_utf8",
    "utf8_to_win",
];

/// utf8_and_iso8859.c's maps: each ISO 8859 encoding's tree to UTF8 and from it
fn iso8859_maps(encoding: Enc) -> Option<(&'static RadixTree, &'static RadixTree)> {
    Some(match encoding {
        PG_LATIN2 => (&iso8859_2_to_unicode_tree, &iso8859_2_from_unicode_tree),
        PG_LATIN3 => (&iso8859_3_to_unicode_tree, &iso8859_3_from_unicode_tree),
        PG_LATIN4 => (&iso8859_4_to_unicode_tree, &iso8859_4_from_unicode_tree),
        PG_LATIN5 => (&iso8859_9_to_unicode_tree, &iso8859_9_from_unicode_tree),
        PG_LATIN6 => (&iso8859_10_to_unicode_tree, &iso8859_10_from_unicode_tree),
        PG_LATIN7 => (&iso8859_13_to_unicode_tree, &iso8859_13_from_unicode_tree),
        PG_LATIN8 => (&iso8859_14_to_unicode_tree, &iso8859_14_from_unicode_tree),
        PG_LATIN9 => (&iso8859_15_to_unicode_tree, &iso8859_15_from_unicode_tree),
        PG_LATIN10 => (&iso8859_16_to_unicode_tree, &iso8859_16_from_unicode_tree),
        PG_ISO_8859_5 => (&iso8859_5_to_unicode_tree, &iso8859_5_from_unicode_tree),
        PG_ISO_8859_6 => (&iso8859_6_to_unicode_tree, &iso8859_6_from_unicode_tree),
        PG_ISO_8859_7 => (&iso8859_7_to_unicode_tree, &iso8859_7_from_unicode_tree),
        PG_ISO_8859_8 => (&iso8859_8_to_unicode_tree, &iso8859_8_from_unicode_tree),
        _ => return None,
    })
}

/// utf8_and_win.c's maps
fn win_maps(encoding: Enc) -> Option<(&'static RadixTree, &'static RadixTree)> {
    Some(match encoding {
        PG_WIN866 => (&win866_to_unicode_tree, &win866_from_unicode_tree),
        PG_WIN874 => (&win874_to_unicode_tree, &win874_from_unicode_tree),
        PG_WIN1250 => (&win1250_to_unicode_tree, &win1250_from_unicode_tree),
        PG_WIN1251 => (&win1251_to_unicode_tree, &win1251_from_unicode_tree),
        PG_WIN1252 => (&win1252_to_unicode_tree, &win1252_from_unicode_tree),
        PG_WIN1253 => (&win1253_to_unicode_tree, &win1253_from_unicode_tree),
        PG_WIN1254 => (&win1254_to_unicode_tree, &win1254_from_unicode_tree),
        PG_WIN1255 => (&win1255_to_unicode_tree, &win1255_from_unicode_tree),
        PG_WIN1256 => (&win1256_to_unicode_tree, &win1256_from_unicode_tree),
        PG_WIN1257 => (&win1257_to_unicode_tree, &win1257_from_unicode_tree),
        PG_WIN1258 => (&win1258_to_unicode_tree, &win1258_from_unicode_tree),
        _ => return None,
    })
}

/// The function `proc` if it is one of these, else None
pub(super) fn call(
    proc: &str,
    from: Enc,
    to: Enc,
    src: &[u8],
    dest: &mut Vec<u8>,
    no_error: bool,
) -> Result<Option<usize>> {
    // a function that is one call: (its local encoding, the tree, whether it goes to UTF8)
    let (local, tree, to_utf8): (Enc, &RadixTree, bool) = match proc {
        "big5_to_utf8" => (PG_BIG5, &big5_to_unicode_tree, true),
        "utf8_to_big5" => (PG_BIG5, &big5_from_unicode_tree, false),
        "koi8r_to_utf8" => (PG_KOI8R, &koi8r_to_unicode_tree, true),
        "utf8_to_koi8r" => (PG_KOI8R, &koi8r_from_unicode_tree, false),
        "koi8u_to_utf8" => (PG_KOI8U, &koi8u_to_unicode_tree, true),
        "utf8_to_koi8u" => (PG_KOI8U, &koi8u_from_unicode_tree, false),
        "euc_cn_to_utf8" => (PG_EUC_CN, &euc_cn_to_unicode_tree, true),
        "utf8_to_euc_cn" => (PG_EUC_CN, &euc_cn_from_unicode_tree, false),
        "euc_jp_to_utf8" => (PG_EUC_JP, &euc_jp_to_unicode_tree, true),
        "utf8_to_euc_jp" => (PG_EUC_JP, &euc_jp_from_unicode_tree, false),
        "euc_kr_to_utf8" => (PG_EUC_KR, &euc_kr_to_unicode_tree, true),
        "utf8_to_euc_kr" => (PG_EUC_KR, &euc_kr_from_unicode_tree, false),
        "euc_tw_to_utf8" => (PG_EUC_TW, &euc_tw_to_unicode_tree, true),
        "utf8_to_euc_tw" => (PG_EUC_TW, &euc_tw_from_unicode_tree, false),
        "gbk_to_utf8" => (PG_GBK, &gbk_to_unicode_tree, true),
        "utf8_to_gbk" => (PG_GBK, &gbk_from_unicode_tree, false),
        "johab_to_utf8" => (PG_JOHAB, &johab_to_unicode_tree, true),
        "utf8_to_johab" => (PG_JOHAB, &johab_from_unicode_tree, false),
        "sjis_to_utf8" => (PG_SJIS, &sjis_to_unicode_tree, true),
        "utf8_to_sjis" => (PG_SJIS, &sjis_from_unicode_tree, false),
        "uhc_to_utf8" => (PG_UHC, &uhc_to_unicode_tree, true),
        "utf8_to_uhc" => (PG_UHC, &uhc_from_unicode_tree, false),
        // utf8_and_euc2004.c and utf8_and_sjis2004.c: with their maps of combined characters
        "euc_jis_2004_to_utf8" => {
            check_args(from, to, Some(PG_EUC_JIS_2004), Some(PG_UTF8))?;
            let cmap = &LUmapEUC_JIS_2004_combined;
            let n =
                local_to_utf(src, dest, Some(&euc_jis_2004_to_unicode_tree), cmap, None, PG_EUC_JIS_2004, no_error)?;
            return Ok(Some(n));
        }
        "utf8_to_euc_jis_2004" => {
            check_args(from, to, Some(PG_UTF8), Some(PG_EUC_JIS_2004))?;
            let cmap = &ULmapEUC_JIS_2004_combined;
            let tree = &euc_jis_2004_from_unicode_tree;
            return Ok(Some(utf_to_local(src, dest, Some(tree), cmap, None, PG_EUC_JIS_2004, no_error)?));
        }
        "shift_jis_2004_to_utf8" => {
            check_args(from, to, Some(PG_SHIFT_JIS_2004), Some(PG_UTF8))?;
            let cmap = &LUmapSHIFT_JIS_2004_combined;
            let tree = &shift_jis_2004_to_unicode_tree;
            return Ok(Some(local_to_utf(src, dest, Some(tree), cmap, None, PG_SHIFT_JIS_2004, no_error)?));
        }
        "utf8_to_shift_jis_2004" => {
            check_args(from, to, Some(PG_UTF8), Some(PG_SHIFT_JIS_2004))?;
            let cmap = &ULmapSHIFT_JIS_2004_combined;
            let tree = &shift_jis_2004_from_unicode_tree;
            return Ok(Some(utf_to_local(src, dest, Some(tree), cmap, None, PG_SHIFT_JIS_2004, no_error)?));
        }
        // utf8_and_gb18030.c
        "gb18030_to_utf8" => {
            check_args(from, to, Some(PG_GB18030), Some(PG_UTF8))?;
            let tree = &gb18030_to_unicode_tree;
            return Ok(Some(local_to_utf(src, dest, Some(tree), &[], Some(conv_18030_to_utf8), PG_GB18030, no_error)?));
        }
        "utf8_to_gb18030" => {
            check_args(from, to, Some(PG_UTF8), Some(PG_GB18030))?;
            let tree = &gb18030_from_unicode_tree;
            return Ok(Some(utf_to_local(src, dest, Some(tree), &[], Some(conv_utf8_to_18030), PG_GB18030, no_error)?));
        }
        // utf8_and_iso8859.c and utf8_and_win.c: the encoding from the call picks the map
        "iso8859_to_utf8" | "win_to_utf8" => {
            check_args(from, to, None, Some(PG_UTF8))?;
            let (maps, family) =
                if proc == "win_to_utf8" { (win_maps(from), "WIN") } else { (iso8859_maps(from), "ISO 8859") };
            let Some((to_unicode, _)) = maps else {
                return err("XX000", format!("unexpected encoding ID {from} for {family} character sets"));
            };
            return Ok(Some(local_to_utf(src, dest, Some(to_unicode), &[], None, from, no_error)?));
        }
        "utf8_to_iso8859" | "utf8_to_win" => {
            check_args(from, to, Some(PG_UTF8), None)?;
            let (maps, family) =
                if proc == "utf8_to_win" { (win_maps(to), "WIN") } else { (iso8859_maps(to), "ISO 8859") };
            let Some((_, from_unicode)) = maps else {
                return err("XX000", format!("unexpected encoding ID {to} for {family} character sets"));
            };
            return Ok(Some(utf_to_local(src, dest, Some(from_unicode), &[], None, to, no_error)?));
        }
        // utf8_and_iso8859_1.c
        "iso8859_1_to_utf8" => {
            check_args(from, to, Some(PG_LATIN1), Some(PG_UTF8))?;
            return Ok(Some(iso8859_1_to_utf8(src, dest, no_error)?));
        }
        "utf8_to_iso8859_1" => {
            check_args(from, to, Some(PG_UTF8), Some(PG_LATIN1))?;
            return Ok(Some(utf8_to_iso8859_1(src, dest, no_error)?));
        }
        _ => return Ok(None),
    };
    let n = if to_utf8 {
        check_args(from, to, Some(local), Some(PG_UTF8))?;
        local_to_utf(src, dest, Some(tree), &[], None, local, no_error)?
    } else {
        check_args(from, to, Some(PG_UTF8), Some(local))?;
        utf_to_local(src, dest, Some(tree), &[], None, local, no_error)?
    };
    Ok(Some(n))
}

/// iso8859_1_to_utf8: each byte is its code point
fn iso8859_1_to_utf8(src: &[u8], dest: &mut Vec<u8>, no_error: bool) -> Result<usize> {
    let mut i = 0;
    while i < src.len() {
        let c = src[i];
        if c == 0 {
            if no_error {
                break;
            }
            return Err(report_invalid_encoding(PG_LATIN1, &src[i..]));
        }
        if !is_highbit_set(c) {
            dest.push(c);
        } else {
            dest.push((c >> 6) | 0xc0);
            dest.push((c & 0x3f) | 0x80);
        }
        i += 1;
    }
    Ok(i)
}

/// utf8_to_iso8859_1: a code point up to U+00FF is its byte
fn utf8_to_iso8859_1(src: &[u8], dest: &mut Vec<u8>, no_error: bool) -> Result<usize> {
    let mut i = 0;
    while i < src.len() {
        let len = src.len() - i;
        let c = src[i];
        if c == 0 {
            if no_error {
                break;
            }
            return Err(report_invalid_encoding(PG_UTF8, &src[i..]));
        }
        if !is_highbit_set(c) {
            dest.push(c);
            i += 1;
        } else {
            let l = utf_mblen(&src[i..]);
            if l > len || !utf8_islegal(&src[i..i + l]) {
                if no_error {
                    break;
                }
                return Err(report_invalid_encoding(PG_UTF8, &src[i..]));
            }
            if l != 2 {
                if no_error {
                    break;
                }
                return Err(report_untranslatable_char(PG_UTF8, PG_LATIN1, &src[i..]));
            }
            let code = ((c as u16 & 0x1f) << 6) | (src[i + 1] as u16 & 0x3f);
            if (0x80..=0xff).contains(&code) {
                dest.push(code as u8);
                i += 2;
            } else {
                if no_error {
                    break;
                }
                return Err(report_untranslatable_char(PG_UTF8, PG_LATIN1, &src[i..]));
            }
        }
    }
    Ok(i)
}

/// gb_linear: a 4-byte GB18030 code as its place in their linear space
fn gb_linear(gb: u32) -> u32 {
    let b0 = (gb & 0xff00_0000) >> 24;
    let b1 = (gb & 0x00ff_0000) >> 16;
    let b2 = (gb & 0x0000_ff00) >> 8;
    let b3 = gb & 0x0000_00ff;
    (b0 * 12600 + b1 * 1260 + b2 * 10 + b3).wrapping_sub(0x81 * 12600 + 0x30 * 1260 + 0x81 * 10 + 0x30)
}

/// gb_unlinear: back
fn gb_unlinear(lin: u32) -> u32 {
    let r0 = 0x81 + lin / 12600;
    let r1 = 0x30 + (lin / 1260) % 10;
    let r2 = 0x81 + (lin / 10) % 126;
    let r3 = 0x30 + lin % 10;
    (r0 << 24) | (r1 << 16) | (r2 << 8) | r3
}

/// unicode_to_utf8word: a code point as its UTF8 bytes in one word
fn unicode_to_utf8word(c: u32) -> u32 {
    if c <= 0x7f {
        c
    } else if c <= 0x7ff {
        ((0xc0 | ((c >> 6) & 0x1f)) << 8) | (0x80 | (c & 0x3f))
    } else if c <= 0xffff {
        ((0xe0 | ((c >> 12) & 0x0f)) << 16) | ((0x80 | ((c >> 6) & 0x3f)) << 8) | (0x80 | (c & 0x3f))
    } else {
        ((0xf0 | ((c >> 18) & 0x07)) << 24)
            | ((0x80 | ((c >> 12) & 0x3f)) << 16)
            | ((0x80 | ((c >> 6) & 0x3f)) << 8)
            | (0x80 | (c & 0x3f))
    }
}

/// utf8word_to_unicode: back
fn utf8word_to_unicode(c: u32) -> u32 {
    if c <= 0x7f {
        c
    } else if c <= 0xffff {
        (((c >> 8) & 0x1f) << 6) | (c & 0x3f)
    } else if c <= 0xff_ffff {
        (((c >> 16) & 0x0f) << 12) | (((c >> 8) & 0x3f) << 6) | (c & 0x3f)
    } else {
        (((c >> 24) & 0x07) << 18) | (((c >> 16) & 0x3f) << 12) | (((c >> 8) & 0x3f) << 6) | (c & 0x3f)
    }
}

/// conv_18030_to_utf8: the ranges of 4-byte GB18030 codes gb-18030-2000.xml maps, as UTF8 (0 for none)
fn conv_18030_to_utf8(code: u32) -> u32 {
    // (first code point, first code, last code)
    const RANGES: [(u32, u32, u32); 13] = [
        (0x0452, 0x8130_D330, 0x8136_A531),
        (0x2643, 0x8137_A839, 0x8138_FD38),
        (0x361B, 0x8230_A633, 0x8230_F237),
        (0x3CE1, 0x8231_D438, 0x8232_AF32),
        (0x4160, 0x8232_C937, 0x8232_F837),
        (0x44D7, 0x8233_A339, 0x8233_C931),
        (0x478E, 0x8233_E838, 0x8234_9638),
        (0x49B8, 0x8234_A131, 0x8234_E733),
        (0x9FA6, 0x8235_8F33, 0x8336_C738),
        (0xE865, 0x8336_D030, 0x8430_8534),
        (0xFA2A, 0x8430_9C38, 0x8431_8537),
        (0xFFE6, 0x8431_A234, 0x8431_A439),
        (0x10000, 0x9030_8130, 0xE332_9A35),
    ];
    for (minunicode, mincode, maxcode) in RANGES {
        if code >= mincode && code <= maxcode {
            return unicode_to_utf8word(gb_linear(code) - gb_linear(mincode) + minunicode);
        }
    }
    0 // no mapping exists
}

/// conv_utf8_to_18030: the code points in those ranges, as their 4-byte GB18030 codes (0 for none)
fn conv_utf8_to_18030(code: u32) -> u32 {
    let ucs = utf8word_to_unicode(code);
    // (first code point, last code point, first code)
    const RANGES: [(u32, u32, u32); 13] = [
        (0x0452, 0x200F, 0x8130_D330),
        (0x2643, 0x2E80, 0x8137_A839),
        (0x361B, 0x3917, 0x8230_A633),
        (0x3CE1, 0x4055, 0x8231_D438),
        (0x4160, 0x4336, 0x8232_C937),
        (0x44D7, 0x464B, 0x8233_A339),
        (0x478E, 0x4946, 0x8233_E838),
        (0x49B8, 0x4C76, 0x8234_A131),
        (0x9FA6, 0xD7FF, 0x8235_8F33),
        (0xE865, 0xF92B, 0x8336_D030),
        (0xFA2A, 0xFE2F, 0x8430_9C38),
        (0xFFE6, 0xFFFF, 0x8431_A234),
        (0x10000, 0x10FFFF, 0x9030_8130),
    ];
    for (minunicode, maxunicode, mincode) in RANGES {
        if ucs >= minunicode && ucs <= maxunicode {
            return gb_unlinear(ucs - minunicode + gb_linear(mincode));
        }
    }
    0 // no mapping exists
}
