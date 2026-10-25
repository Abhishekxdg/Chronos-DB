//! src/backend/utils/mb/conversion_procs: the conversion functions, by their SQL names (pg_proc's prosrc, which
//! pg_conversion's conproc names). `call` runs one as Postgres's fmgr would: it checks the encodings it was called
//! with (CHECK_ENCODING_CONVERSION_ARGS), converts `src` into `dest` and gives how many bytes of `src` it converted.
//! The functions whose C is a single call into conv.c are here; the others are in a file named for their C file.

mod big5;
mod euc2004_sjis2004;
mod euc_cn_and_mic;
mod euc_jp_and_sjis;
mod euc_kr_and_mic;
mod euc_tw_and_big5;
mod tables;
mod utf8;

use super::conv::{latin2mic, latin2mic_with_table, local2local, mic2latin, mic2latin_with_table};
use super::*;
use tables::*;

/// The MULE_INTERNAL charset bytes (LC_*, src/include/mb/pg_wchar.h) the conversions use
pub(super) const LC_ISO8859_1: u8 = 0x81;
pub(super) const LC_ISO8859_2: u8 = 0x82;
pub(super) const LC_ISO8859_3: u8 = 0x83;
pub(super) const LC_ISO8859_4: u8 = 0x84;
pub(super) const LC_JISX0201K: u8 = 0x89;
pub(super) const LC_KOI8_R: u8 = 0x8b;
pub(super) const LC_GB2312_80: u8 = 0x91;
pub(super) const LC_JISX0208: u8 = 0x92;
pub(super) const LC_KS5601: u8 = 0x93;
pub(super) const LC_JISX0212: u8 = 0x94;
pub(super) const LC_CNS11643_1: u8 = 0x95;
pub(super) const LC_CNS11643_2: u8 = 0x96;
pub(super) const LC_CNS11643_3: u8 = 0xf6;
pub(super) const LC_CNS11643_4: u8 = 0xf7;
pub(super) const LC_CNS11643_7: u8 = 0xfa;
pub(super) const LCPRV2_B: u8 = 0x9d;

pub(super) const SS2: u8 = 0x8e;
pub(super) const SS3: u8 = 0x8f;

pub(super) fn is_highbit_set(c: u8) -> bool {
    c & 0x80 != 0
}

/// check_encoding_conversion_args: the encodings a conversion function was called with are the ones it converts
/// (`expected_*` None: any). Postgres's elog: not user-facing.
fn check_args(src: Enc, dest: Enc, expected_src: Option<Enc>, expected_dest: Option<Enc>) -> Result<()> {
    if src >= PG_LAST_ENCODING {
        return err("XX000", format!("invalid source encoding ID: {src}"));
    }
    if let Some(e) = expected_src
        && src != e
    {
        return err("XX000", format!("expected source encoding \"{}\", but got \"{}\"", NAMES[e], NAMES[src]));
    }
    if dest >= PG_LAST_ENCODING {
        return err("XX000", format!("invalid destination encoding ID: {dest}"));
    }
    if let Some(e) = expected_dest
        && dest != e
    {
        return err("XX000", format!("expected destination encoding \"{}\", but got \"{}\"", NAMES[e], NAMES[dest]));
    }
    Ok(())
}

/// Runs the conversion function named `proc` on `src` (in `from`) into `dest` (in `to`), as Postgres calls it: the
/// number of bytes of `src` it converted. An error for a name that is no conversion function.
pub(crate) fn call(proc: &str, from: Enc, to: Enc, src: &[u8], dest: &mut Vec<u8>, no_error: bool) -> Result<usize> {
    if let Some(n) = utf8::call(proc, from, to, src, dest, no_error)? {
        return Ok(n);
    }
    // (name, its source and destination encodings, the call its C makes)
    macro_rules! conv {
        ($s:expr, $d:expr, $e:expr) => {{
            check_args(from, to, Some($s), Some($d))?;
            $e
        }};
    }
    let n = match proc {
        // cyrillic_and_mic.c
        "koi8r_to_mic" => conv!(PG_KOI8R, PG_MULE_INTERNAL, latin2mic(src, dest, LC_KOI8_R, PG_KOI8R, no_error)),
        "mic_to_koi8r" => conv!(PG_MULE_INTERNAL, PG_KOI8R, mic2latin(src, dest, LC_KOI8_R, PG_KOI8R, no_error)),
        "iso_to_mic" => conv!(
            PG_ISO_8859_5,
            PG_MULE_INTERNAL,
            latin2mic_with_table(src, dest, LC_KOI8_R, PG_ISO_8859_5, &iso2koi, no_error)
        ),
        "mic_to_iso" => conv!(
            PG_MULE_INTERNAL,
            PG_ISO_8859_5,
            mic2latin_with_table(src, dest, LC_KOI8_R, PG_ISO_8859_5, &koi2iso, no_error)
        ),
        "win1251_to_mic" => conv!(
            PG_WIN1251,
            PG_MULE_INTERNAL,
            latin2mic_with_table(src, dest, LC_KOI8_R, PG_WIN1251, &win12512koi, no_error)
        ),
        "mic_to_win1251" => conv!(
            PG_MULE_INTERNAL,
            PG_WIN1251,
            mic2latin_with_table(src, dest, LC_KOI8_R, PG_WIN1251, &koi2win1251, no_error)
        ),
        "win866_to_mic" => conv!(
            PG_WIN866,
            PG_MULE_INTERNAL,
            latin2mic_with_table(src, dest, LC_KOI8_R, PG_WIN866, &win8662koi, no_error)
        ),
        "mic_to_win866" => conv!(
            PG_MULE_INTERNAL,
            PG_WIN866,
            mic2latin_with_table(src, dest, LC_KOI8_R, PG_WIN866, &koi2win866, no_error)
        ),
        "koi8r_to_win1251" => {
            conv!(PG_KOI8R, PG_WIN1251, local2local(src, dest, PG_KOI8R, PG_WIN1251, &koi2win1251, no_error))
        }
        "win1251_to_koi8r" => {
            conv!(PG_WIN1251, PG_KOI8R, local2local(src, dest, PG_WIN1251, PG_KOI8R, &win12512koi, no_error))
        }
        "koi8r_to_win866" => {
            conv!(PG_KOI8R, PG_WIN866, local2local(src, dest, PG_KOI8R, PG_WIN866, &koi2win866, no_error))
        }
        "win866_to_koi8r" => {
            conv!(PG_WIN866, PG_KOI8R, local2local(src, dest, PG_WIN866, PG_KOI8R, &win8662koi, no_error))
        }
        "win866_to_win1251" => {
            conv!(PG_WIN866, PG_WIN1251, local2local(src, dest, PG_WIN866, PG_WIN1251, &win8662win1251, no_error))
        }
        "win1251_to_win866" => {
            conv!(PG_WIN1251, PG_WIN866, local2local(src, dest, PG_WIN1251, PG_WIN866, &win12512win866, no_error))
        }
        "iso_to_koi8r" => {
            conv!(PG_ISO_8859_5, PG_KOI8R, local2local(src, dest, PG_ISO_8859_5, PG_KOI8R, &iso2koi, no_error))
        }
        "koi8r_to_iso" => {
            conv!(PG_KOI8R, PG_ISO_8859_5, local2local(src, dest, PG_KOI8R, PG_ISO_8859_5, &koi2iso, no_error))
        }
        "iso_to_win1251" => {
            conv!(PG_ISO_8859_5, PG_WIN1251, local2local(src, dest, PG_ISO_8859_5, PG_WIN1251, &iso2win1251, no_error))
        }
        "win1251_to_iso" => {
            conv!(PG_WIN1251, PG_ISO_8859_5, local2local(src, dest, PG_WIN1251, PG_ISO_8859_5, &win12512iso, no_error))
        }
        "iso_to_win866" => {
            conv!(PG_ISO_8859_5, PG_WIN866, local2local(src, dest, PG_ISO_8859_5, PG_WIN866, &iso2win866, no_error))
        }
        "win866_to_iso" => {
            conv!(PG_WIN866, PG_ISO_8859_5, local2local(src, dest, PG_WIN866, PG_ISO_8859_5, &win8662iso, no_error))
        }
        // euc2004_sjis2004.c
        "euc_jis_2004_to_shift_jis_2004" => conv!(
            PG_EUC_JIS_2004,
            PG_SHIFT_JIS_2004,
            euc2004_sjis2004::euc_jis_20042shift_jis_2004(src, dest, no_error)
        ),
        "shift_jis_2004_to_euc_jis_2004" => conv!(
            PG_SHIFT_JIS_2004,
            PG_EUC_JIS_2004,
            euc2004_sjis2004::shift_jis_20042euc_jis_2004(src, dest, no_error)
        ),
        // euc_cn_and_mic.c
        "euc_cn_to_mic" => conv!(PG_EUC_CN, PG_MULE_INTERNAL, euc_cn_and_mic::euc_cn2mic(src, dest, no_error)),
        "mic_to_euc_cn" => conv!(PG_MULE_INTERNAL, PG_EUC_CN, euc_cn_and_mic::mic2euc_cn(src, dest, no_error)),
        // euc_jp_and_sjis.c
        "euc_jp_to_sjis" => conv!(PG_EUC_JP, PG_SJIS, euc_jp_and_sjis::euc_jp2sjis(src, dest, no_error)),
        "sjis_to_euc_jp" => conv!(PG_SJIS, PG_EUC_JP, euc_jp_and_sjis::sjis2euc_jp(src, dest, no_error)),
        "euc_jp_to_mic" => conv!(PG_EUC_JP, PG_MULE_INTERNAL, euc_jp_and_sjis::euc_jp2mic(src, dest, no_error)),
        "mic_to_euc_jp" => conv!(PG_MULE_INTERNAL, PG_EUC_JP, euc_jp_and_sjis::mic2euc_jp(src, dest, no_error)),
        "sjis_to_mic" => conv!(PG_SJIS, PG_MULE_INTERNAL, euc_jp_and_sjis::sjis2mic(src, dest, no_error)),
        "mic_to_sjis" => conv!(PG_MULE_INTERNAL, PG_SJIS, euc_jp_and_sjis::mic2sjis(src, dest, no_error)),
        // euc_kr_and_mic.c
        "euc_kr_to_mic" => conv!(PG_EUC_KR, PG_MULE_INTERNAL, euc_kr_and_mic::euc_kr2mic(src, dest, no_error)),
        "mic_to_euc_kr" => conv!(PG_MULE_INTERNAL, PG_EUC_KR, euc_kr_and_mic::mic2euc_kr(src, dest, no_error)),
        // euc_tw_and_big5.c
        "euc_tw_to_big5" => conv!(PG_EUC_TW, PG_BIG5, euc_tw_and_big5::euc_tw2big5(src, dest, no_error)),
        "big5_to_euc_tw" => conv!(PG_BIG5, PG_EUC_TW, euc_tw_and_big5::big52euc_tw(src, dest, no_error)),
        "euc_tw_to_mic" => conv!(PG_EUC_TW, PG_MULE_INTERNAL, euc_tw_and_big5::euc_tw2mic(src, dest, no_error)),
        "mic_to_euc_tw" => conv!(PG_MULE_INTERNAL, PG_EUC_TW, euc_tw_and_big5::mic2euc_tw(src, dest, no_error)),
        "big5_to_mic" => conv!(PG_BIG5, PG_MULE_INTERNAL, euc_tw_and_big5::big52mic(src, dest, no_error)),
        "mic_to_big5" => conv!(PG_MULE_INTERNAL, PG_BIG5, euc_tw_and_big5::mic2big5(src, dest, no_error)),
        // latin2_and_win1250.c
        "latin2_to_mic" => {
            conv!(PG_LATIN2, PG_MULE_INTERNAL, latin2mic(src, dest, LC_ISO8859_2, PG_LATIN2, no_error))
        }
        "mic_to_latin2" => {
            conv!(PG_MULE_INTERNAL, PG_LATIN2, mic2latin(src, dest, LC_ISO8859_2, PG_LATIN2, no_error))
        }
        "win1250_to_mic" => conv!(
            PG_WIN1250,
            PG_MULE_INTERNAL,
            latin2mic_with_table(src, dest, LC_ISO8859_2, PG_WIN1250, &win1250_2_iso88592, no_error)
        ),
        "mic_to_win1250" => conv!(
            PG_MULE_INTERNAL,
            PG_WIN1250,
            mic2latin_with_table(src, dest, LC_ISO8859_2, PG_WIN1250, &iso88592_2_win1250, no_error)
        ),
        "latin2_to_win1250" => {
            conv!(PG_LATIN2, PG_WIN1250, local2local(src, dest, PG_LATIN2, PG_WIN1250, &iso88592_2_win1250, no_error))
        }
        "win1250_to_latin2" => {
            conv!(PG_WIN1250, PG_LATIN2, local2local(src, dest, PG_WIN1250, PG_LATIN2, &win1250_2_iso88592, no_error))
        }
        // latin_and_mic.c
        "latin1_to_mic" => {
            conv!(PG_LATIN1, PG_MULE_INTERNAL, latin2mic(src, dest, LC_ISO8859_1, PG_LATIN1, no_error))
        }
        "mic_to_latin1" => {
            conv!(PG_MULE_INTERNAL, PG_LATIN1, mic2latin(src, dest, LC_ISO8859_1, PG_LATIN1, no_error))
        }
        "latin3_to_mic" => {
            conv!(PG_LATIN3, PG_MULE_INTERNAL, latin2mic(src, dest, LC_ISO8859_3, PG_LATIN3, no_error))
        }
        "mic_to_latin3" => {
            conv!(PG_MULE_INTERNAL, PG_LATIN3, mic2latin(src, dest, LC_ISO8859_3, PG_LATIN3, no_error))
        }
        "latin4_to_mic" => {
            conv!(PG_LATIN4, PG_MULE_INTERNAL, latin2mic(src, dest, LC_ISO8859_4, PG_LATIN4, no_error))
        }
        "mic_to_latin4" => {
            conv!(PG_MULE_INTERNAL, PG_LATIN4, mic2latin(src, dest, LC_ISO8859_4, PG_LATIN4, no_error))
        }
        _ => {
            return err(
                "42883",
                format!("function {proc}(integer, integer, cstring, internal, integer, boolean) does not exist"),
            );
        }
    }?;
    Ok(n)
}

/// Whether `proc` is a conversion function here (CREATE CONVERSION's FROM names one)
pub(crate) fn exists(proc: &str) -> bool {
    utf8::NAMES.contains(&proc) || CONVERSIONS.iter().any(|c| c.proc == proc)
}
