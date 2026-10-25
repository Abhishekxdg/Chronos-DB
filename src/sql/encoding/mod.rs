//! Character encodings and the conversions between them, as Postgres 16 has them: a port of src/common/wchar.c
//! (each encoding's character length and verifier: `wchar`), src/common/encnames.c (names and aliases: here),
//! src/backend/utils/mb/conv.c (the table-driven conversions: `conv`), the conversion functions of
//! src/backend/utils/mb/conversion_procs (`procs`), their maps (src/backend/utils/mb/Unicode/*.map: `maps`, made by
//! tools/pg_unicode_maps.py) and the built-in conversions (src/include/catalog/pg_conversion.dat: `conversions`).
//! Each Rust function names the C function it ports. The C is Postgres's, under the PostgreSQL License (see NOTICE).
//!
//! Text in Chronos is UTF8; an encoding matters where bytes come in or go out in another one: `convert`,
//! `convert_to` and `convert_from`, and the conversion and verification functions the regression suite calls
//! (regress.c's `test_enc_conversion`).

mod conv;
mod conversions;
mod maps;
mod procs;
mod wchar;

pub(crate) use conversions::CONVERSIONS;
pub(crate) use wchar::{max_length, verifymbstr};

use super::{SqlError, err};

type Result<T> = std::result::Result<T, SqlError>;

/// An encoding: its number in Postgres's `pg_enc` (src/include/mb/pg_wchar.h), which pg_conversion and
/// pg_database show.
pub(crate) type Enc = usize;

pub(crate) const PG_SQL_ASCII: Enc = 0;
pub(crate) const PG_EUC_JP: Enc = 1;
pub(crate) const PG_EUC_CN: Enc = 2;
pub(crate) const PG_EUC_KR: Enc = 3;
pub(crate) const PG_EUC_TW: Enc = 4;
pub(crate) const PG_EUC_JIS_2004: Enc = 5;
pub(crate) const PG_UTF8: Enc = 6;
pub(crate) const PG_MULE_INTERNAL: Enc = 7;
pub(crate) const PG_LATIN1: Enc = 8;
pub(crate) const PG_LATIN2: Enc = 9;
pub(crate) const PG_LATIN3: Enc = 10;
pub(crate) const PG_LATIN4: Enc = 11;
pub(crate) const PG_LATIN5: Enc = 12;
pub(crate) const PG_LATIN6: Enc = 13;
pub(crate) const PG_LATIN7: Enc = 14;
pub(crate) const PG_LATIN8: Enc = 15;
pub(crate) const PG_LATIN9: Enc = 16;
pub(crate) const PG_LATIN10: Enc = 17;
pub(crate) const PG_WIN1256: Enc = 18;
pub(crate) const PG_WIN1258: Enc = 19;
pub(crate) const PG_WIN866: Enc = 20;
pub(crate) const PG_WIN874: Enc = 21;
pub(crate) const PG_KOI8R: Enc = 22;
pub(crate) const PG_WIN1251: Enc = 23;
pub(crate) const PG_WIN1252: Enc = 24;
pub(crate) const PG_ISO_8859_5: Enc = 25;
pub(crate) const PG_ISO_8859_6: Enc = 26;
pub(crate) const PG_ISO_8859_7: Enc = 27;
pub(crate) const PG_ISO_8859_8: Enc = 28;
pub(crate) const PG_WIN1250: Enc = 29;
pub(crate) const PG_WIN1253: Enc = 30;
pub(crate) const PG_WIN1254: Enc = 31;
pub(crate) const PG_WIN1255: Enc = 32;
pub(crate) const PG_WIN1257: Enc = 33;
pub(crate) const PG_KOI8U: Enc = 34;
pub(crate) const PG_SJIS: Enc = 35;
pub(crate) const PG_BIG5: Enc = 36;
pub(crate) const PG_GBK: Enc = 37;
pub(crate) const PG_UHC: Enc = 38;
pub(crate) const PG_GB18030: Enc = 39;
pub(crate) const PG_JOHAB: Enc = 40;
pub(crate) const PG_SHIFT_JIS_2004: Enc = 41;
/// `_PG_LAST_ENCODING_`: how many there are
pub(crate) const PG_LAST_ENCODING: Enc = 42;

/// pg_enc2name_tbl: each encoding's name, in `pg_enc` order
pub(crate) const NAMES: [&str; PG_LAST_ENCODING] = [
    "SQL_ASCII",
    "EUC_JP",
    "EUC_CN",
    "EUC_KR",
    "EUC_TW",
    "EUC_JIS_2004",
    "UTF8",
    "MULE_INTERNAL",
    "LATIN1",
    "LATIN2",
    "LATIN3",
    "LATIN4",
    "LATIN5",
    "LATIN6",
    "LATIN7",
    "LATIN8",
    "LATIN9",
    "LATIN10",
    "WIN1256",
    "WIN1258",
    "WIN866",
    "WIN874",
    "KOI8R",
    "WIN1251",
    "WIN1252",
    "ISO_8859_5",
    "ISO_8859_6",
    "ISO_8859_7",
    "ISO_8859_8",
    "WIN1250",
    "WIN1253",
    "WIN1254",
    "WIN1255",
    "WIN1257",
    "KOI8U",
    "SJIS",
    "BIG5",
    "GBK",
    "UHC",
    "GB18030",
    "JOHAB",
    "SHIFT_JIS_2004",
];

/// pg_encname_tbl: every name an encoding is known by, lowercase with only its letters and digits (as
/// clean_encoding_name leaves a name)
const ALIASES: &[(&str, Enc)] = &[
    ("abc", PG_WIN1258),
    ("alt", PG_WIN866),
    ("big5", PG_BIG5),
    ("euccn", PG_EUC_CN),
    ("eucjis2004", PG_EUC_JIS_2004),
    ("eucjp", PG_EUC_JP),
    ("euckr", PG_EUC_KR),
    ("euctw", PG_EUC_TW),
    ("gb18030", PG_GB18030),
    ("gbk", PG_GBK),
    ("iso88591", PG_LATIN1),
    ("iso885910", PG_LATIN6),
    ("iso885913", PG_LATIN7),
    ("iso885914", PG_LATIN8),
    ("iso885915", PG_LATIN9),
    ("iso885916", PG_LATIN10),
    ("iso88592", PG_LATIN2),
    ("iso88593", PG_LATIN3),
    ("iso88594", PG_LATIN4),
    ("iso88595", PG_ISO_8859_5),
    ("iso88596", PG_ISO_8859_6),
    ("iso88597", PG_ISO_8859_7),
    ("iso88598", PG_ISO_8859_8),
    ("iso88599", PG_LATIN5),
    ("johab", PG_JOHAB),
    ("koi8", PG_KOI8R),
    ("koi8r", PG_KOI8R),
    ("koi8u", PG_KOI8U),
    ("latin1", PG_LATIN1),
    ("latin10", PG_LATIN10),
    ("latin2", PG_LATIN2),
    ("latin3", PG_LATIN3),
    ("latin4", PG_LATIN4),
    ("latin5", PG_LATIN5),
    ("latin6", PG_LATIN6),
    ("latin7", PG_LATIN7),
    ("latin8", PG_LATIN8),
    ("latin9", PG_LATIN9),
    ("mskanji", PG_SJIS),
    ("muleinternal", PG_MULE_INTERNAL),
    ("shiftjis", PG_SJIS),
    ("shiftjis2004", PG_SHIFT_JIS_2004),
    ("sjis", PG_SJIS),
    ("sqlascii", PG_SQL_ASCII),
    ("tcvn", PG_WIN1258),
    ("tcvn5712", PG_WIN1258),
    ("uhc", PG_UHC),
    ("unicode", PG_UTF8),
    ("utf8", PG_UTF8),
    ("vscii", PG_WIN1258),
    ("win", PG_WIN1251),
    ("win1250", PG_WIN1250),
    ("win1251", PG_WIN1251),
    ("win1252", PG_WIN1252),
    ("win1253", PG_WIN1253),
    ("win1254", PG_WIN1254),
    ("win1255", PG_WIN1255),
    ("win1256", PG_WIN1256),
    ("win1257", PG_WIN1257),
    ("win1258", PG_WIN1258),
    ("win866", PG_WIN866),
    ("win874", PG_WIN874),
    ("win932", PG_SJIS),
    ("win936", PG_GBK),
    ("win949", PG_UHC),
    ("win950", PG_BIG5),
    ("windows1250", PG_WIN1250),
    ("windows1251", PG_WIN1251),
    ("windows1252", PG_WIN1252),
    ("windows1253", PG_WIN1253),
    ("windows1254", PG_WIN1254),
    ("windows1255", PG_WIN1255),
    ("windows1256", PG_WIN1256),
    ("windows1257", PG_WIN1257),
    ("windows1258", PG_WIN1258),
    ("windows866", PG_WIN866),
    ("windows874", PG_WIN874),
    ("windows932", PG_SJIS),
    ("windows936", PG_GBK),
    ("windows949", PG_UHC),
    ("windows950", PG_BIG5),
];

/// pg_char_to_encoding: the encoding a name means (any case, any punctuation: clean_encoding_name keeps only its
/// letters and digits), or None. A name of NAMEDATALEN (64) bytes or more names none.
pub(crate) fn char_to_encoding(name: &str) -> Option<Enc> {
    if name.is_empty() || name.len() >= 64 {
        return None;
    }
    let key: String = name.chars().filter(char::is_ascii_alphanumeric).map(|c| c.to_ascii_lowercase()).collect();
    ALIASES.binary_search_by(|(alias, _)| alias.cmp(&key.as_str())).ok().map(|i| ALIASES[i].1)
}

/// pg_encoding_to_char: an encoding's name ("" for a number that is none)
pub(crate) fn encoding_to_char(encoding: Enc) -> &'static str {
    NAMES.get(encoding).copied().unwrap_or("")
}

/// The first bytes of `s` as the reports below write them: `0x` and two hex digits each, spaces between, at most
/// `mblen` of them and never more than 8 (report_invalid_encoding_int's buffer)
fn bytes_of(s: &[u8], mblen: usize) -> String {
    let n = mblen.min(s.len()).min(8);
    s[..n].iter().map(|b| format!("0x{b:02x}")).collect::<Vec<_>>().join(" ")
}

/// report_invalid_encoding: `s` (the rest of the string, starting at the bad character) isn't valid in `encoding`
pub(crate) fn report_invalid_encoding(encoding: Enc, s: &[u8]) -> SqlError {
    let l = wchar::mblen_or_incomplete(encoding, s);
    SqlError {
        code: "22021",
        message: format!("invalid byte sequence for encoding \"{}\": {}", NAMES[encoding], bytes_of(s, l)),
        info: None,
    }
}

/// report_untranslatable_char: `s` (the rest of the string, starting at the character) has no equivalent in
/// `dest_encoding`
pub(crate) fn report_untranslatable_char(src_encoding: Enc, dest_encoding: Enc, s: &[u8]) -> SqlError {
    let l = wchar::mblen_or_incomplete(src_encoding, s);
    SqlError {
        code: "22P05",
        message: format!(
            "character with byte sequence {} in encoding \"{}\" has no equivalent in encoding \"{}\"",
            bytes_of(s, l),
            NAMES[src_encoding],
            NAMES[dest_encoding]
        ),
        info: None,
    }
}

/// FindDefaultConversionProc, over the built-in conversions: the function that converts `from` to `to` by
/// default, or None
pub(crate) fn default_conversion_proc(from: Enc, to: Enc) -> Option<&'static str> {
    CONVERSIONS.iter().find(|c| c.from == from && c.to == to).map(|c| c.proc)
}

/// Calls the conversion function `proc` (one of `CONVERSIONS`' or a user's conversion naming one) as
/// pg_do_encoding_conversion_buf does: the converted bytes, and how many of `src` it converted. With `no_error`, an
/// invalid or untranslatable character ends the conversion there instead of failing it.
pub(crate) fn convert_with(proc: &str, from: Enc, to: Enc, src: &[u8], no_error: bool) -> Result<(usize, Vec<u8>)> {
    let mut dest = Vec::with_capacity(src.len() * MAX_CONVERSION_GROWTH);
    let converted = procs::call(proc, from, to, src, &mut dest, no_error)?;
    Ok((converted, dest))
}

/// Whether `proc` names a conversion function (what CREATE CONVERSION's FROM may name)
pub(crate) fn proc_exists(proc: &str) -> bool {
    procs::exists(proc)
}

/// MAX_CONVERSION_GROWTH: the most bytes a conversion writes for one byte it reads
pub(crate) const MAX_CONVERSION_GROWTH: usize = 4;

/// pg_verify_mbstr: an error unless all of `s` is valid in `encoding`
pub(crate) fn verify_mbstr(encoding: Enc, s: &[u8]) -> Result<()> {
    let ok = verifymbstr(encoding, s);
    if ok != s.len() {
        return Err(report_invalid_encoding(encoding, &s[ok..]));
    }
    Ok(())
}

/// pg_do_encoding_conversion: `src` in `from` as `to` by the pair's default conversion. An empty string, one
/// encoding to itself and anything to SQL_ASCII are left as they are; from SQL_ASCII the bytes are only checked
/// against `to`.
pub(crate) fn do_encoding_conversion(src: &[u8], from: Enc, to: Enc) -> Result<Vec<u8>> {
    if src.is_empty() || from == to || to == PG_SQL_ASCII {
        return Ok(src.to_vec());
    }
    if from == PG_SQL_ASCII {
        verify_mbstr(to, src)?;
        return Ok(src.to_vec());
    }
    let Some(proc) = default_conversion_proc(from, to) else {
        return err(
            "42883",
            format!("default conversion function for encoding \"{}\" to \"{}\" does not exist", NAMES[from], NAMES[to]),
        );
    };
    Ok(convert_with(proc, from, to, src, false)?.1)
}

/// pg_convert, `convert(bytes, from, to)`: `src`, which must be valid in the encoding named `from`, converted to
/// the one named `to`. `convert_to` is it from the database's encoding (UTF8), `convert_from` to it.
pub(crate) fn pg_convert(src: &[u8], from: &str, to: &str) -> Result<Vec<u8>> {
    let Some(from_enc) = char_to_encoding(from) else {
        return err("22023", format!("invalid source encoding name \"{from}\""));
    };
    let Some(to_enc) = char_to_encoding(to) else {
        return err("22023", format!("invalid destination encoding name \"{to}\""));
    };
    verify_mbstr(from_enc, src)?;
    do_encoding_conversion(src, from_enc, to_enc)
}

/// regress.c's test_enc_conversion(string, src_encoding, dest_encoding, noError): `string` converted (or, between
/// one encoding and itself, verified), and how many of its bytes were good. With `no_error` a bad character ends the
/// result there; without, it fails as the conversion does.
pub(crate) fn test_enc_conversion(src: &[u8], from: &str, to: &str, no_error: bool) -> Result<(i32, Vec<u8>)> {
    let Some(from_enc) = char_to_encoding(from) else {
        return err("22023", format!("invalid source encoding name \"{from}\""));
    };
    let Some(to_enc) = char_to_encoding(to) else {
        return err("22023", format!("invalid destination encoding name \"{to}\""));
    };
    if from_enc == to_enc {
        let ok = verifymbstr(from_enc, src);
        if ok == src.len() {
            return Ok((ok as i32, src.to_vec()));
        }
        if !no_error {
            return Err(report_invalid_encoding(from_enc, &src[ok..]));
        }
        return Ok((ok as i32, src[..ok].to_vec()));
    }
    let Some(proc) = default_conversion_proc(from_enc, to_enc) else {
        return err(
            "42883",
            format!(
                "default conversion function for encoding \"{}\" to \"{}\" does not exist",
                NAMES[from_enc], NAMES[to_enc]
            ),
        );
    };
    let (converted, dest) = convert_with(proc, from_enc, to_enc, src, no_error)?;
    Ok((converted as i32, dest))
}

/// regress.c's test_enc_setup: checks pg_encoding_set_invalid's two bytes in each multibyte encoding (a character of
/// two bytes, none of it valid, also with spaces after); a warning for each that isn't (none, in a correct port)
pub(crate) fn test_enc_setup() -> Vec<String> {
    let mut warnings = Vec::new();
    for (i, name) in NAMES.iter().enumerate() {
        if max_length(i) == 1 {
            continue;
        }
        let buf = wchar::set_invalid(i);
        let len = buf.iter().position(|&b| b == 0).unwrap_or(2);
        if len != 2 {
            warnings.push(format!("official invalid string for encoding \"{}\" has length {len}", name));
        }
        let mblen = wchar::mblen(i, &buf);
        if mblen != 2 {
            warnings.push(format!("official invalid string for encoding \"{}\" has mblen {mblen}", name));
        }
        let valid = verifymbstr(i, &buf[..len]);
        if valid != 0 {
            warnings
                .push(format!("official invalid string for encoding \"{}\" has valid prefix of length {valid}", name));
        }
        let valid = verifymbstr(i, &buf[..1]);
        if valid != 0 {
            warnings.push(format!(
                "first byte of official invalid string for encoding \"{}\" has valid prefix of length {valid}",
                name
            ));
        }
        let mut bigbuf = [b' '; 16];
        bigbuf[..2].copy_from_slice(&buf);
        let valid = verifymbstr(i, &bigbuf);
        if valid != 0 {
            warnings.push(format!(
                "trailing data changed official invalid string for encoding \"{}\" to have valid prefix of length {valid}",
                name
            ));
        }
    }
    warnings
}

#[cfg(test)]
mod tests;
