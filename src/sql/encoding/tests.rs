//! The port against the C's edge cases. conversion.sql runs every conversion family through regress.c's
//! test_enc_conversion; these pin the pieces it shares and the cases its comments name.

use super::*;

fn conv(src: &[u8], from: &str, to: &str) -> std::result::Result<Vec<u8>, String> {
    pg_convert(src, from, to).map_err(|e| e.message)
}

#[test]
fn names_and_aliases() {
    assert_eq!(char_to_encoding("UTF-8"), Some(PG_UTF8));
    assert_eq!(char_to_encoding("unicode"), Some(PG_UTF8));
    assert_eq!(char_to_encoding("ISO_8859_1"), Some(PG_LATIN1));
    assert_eq!(char_to_encoding("iso8859-9"), Some(PG_LATIN5));
    assert_eq!(char_to_encoding("windows-1251"), Some(PG_WIN1251));
    assert_eq!(char_to_encoding("MS_Kanji"), Some(PG_SJIS));
    assert_eq!(char_to_encoding("nope"), None);
    assert_eq!(char_to_encoding(""), None);
    assert_eq!(encoding_to_char(PG_SHIFT_JIS_2004), "SHIFT_JIS_2004");
    assert_eq!(encoding_to_char(99), "");
    // the aliases are sorted, as pg_char_to_encoding's binary search needs
    assert!(ALIASES.windows(2).all(|w| w[0].0 < w[1].0));
}

#[test]
fn every_conversion_function_runs_on_empty_input() {
    // as CREATE CONVERSION calls each: an empty string converts to nothing, without error
    for c in CONVERSIONS.iter() {
        assert_eq!(convert_with(c.proc, c.from, c.to, b"", false).unwrap(), (0, vec![]), "{}", c.proc);
    }
    // and refuses encodings it isn't for
    let e = convert_with("koi8r_to_mic", PG_LATIN1, PG_MULE_INTERNAL, b"", false).unwrap_err();
    assert_eq!(e.message, "expected source encoding \"KOI8R\", but got \"LATIN1\"");
}

#[test]
fn convert_round_trips() {
    let s = "héllo, мир, 日本語";
    for enc in ["LATIN1", "KOI8R", "EUC_JP", "SJIS", "GB18030", "UTF8"] {
        let Ok(b) = conv(s.as_bytes(), "UTF8", enc) else { continue };
        assert_eq!(conv(&b, enc, "UTF8").unwrap(), s.as_bytes(), "{enc}");
    }
    let jp = "日本語";
    for enc in ["EUC_JP", "SJIS", "EUC_JIS_2004", "SHIFT_JIS_2004", "GB18030", "BIG5", "UHC", "EUC_KR", "GBK"] {
        let b = conv(jp.as_bytes(), "UTF8", enc).unwrap_or_else(|e| panic!("{enc}: {e}"));
        assert_eq!(conv(&b, enc, "UTF8").unwrap(), jp.as_bytes(), "{enc}");
    }
    // between two local encodings, through their own conversions
    let e = conv("日本語".as_bytes(), "UTF8", "EUC_JP").unwrap();
    let s = conv(&e, "EUC_JP", "SJIS").unwrap();
    assert_eq!(conv(&s, "SJIS", "EUC_JP").unwrap(), e);
    assert_eq!(conv(&e, "EUC_JP", "MULE_INTERNAL").and_then(|m| conv(&m, "MULE_INTERNAL", "EUC_JP")).unwrap(), e);
}

#[test]
fn errors_name_the_bytes() {
    assert_eq!(
        conv("ä€".as_bytes(), "UTF8", "LATIN1").unwrap_err(),
        "character with byte sequence 0xe2 0x82 0xac in encoding \"UTF8\" has no equivalent in encoding \"LATIN1\""
    );
    assert_eq!(conv(b"a\xc3", "UTF8", "LATIN1").unwrap_err(), "invalid byte sequence for encoding \"UTF8\": 0xc3");
    assert_eq!(conv(b"a\x00b", "LATIN1", "UTF8").unwrap_err(), "invalid byte sequence for encoding \"LATIN1\": 0x00");
    // GB18030's four-byte lead with one byte left: incomplete, so just the byte (pg_encoding_mblen_or_incomplete)
    assert_eq!(conv(b"\x81", "GB18030", "UTF8").unwrap_err(), "invalid byte sequence for encoding \"GB18030\": 0x81");
    assert_eq!(conv(b"x", "nope", "UTF8").unwrap_err(), "invalid source encoding name \"nope\"");
    // SQL_ASCII: anything goes to it; from it, the bytes must be valid in the target
    assert_eq!(conv(b"\xff", "UTF8", "SQL_ASCII").unwrap_err(), "invalid byte sequence for encoding \"UTF8\": 0xff");
    assert_eq!(conv(b"\xff", "SQL_ASCII", "LATIN1").unwrap(), b"\xff");
    assert_eq!(conv(b"\xff", "SQL_ASCII", "UTF8").unwrap_err(), "invalid byte sequence for encoding \"UTF8\": 0xff");
}

#[test]
fn gb18030_four_byte_ranges() {
    // U+0452 is the first code point of the first range conv_utf8_to_18030 computes; U+10000 the first of the last
    assert_eq!(conv("\u{452}".as_bytes(), "UTF8", "GB18030").unwrap(), b"\x81\x30\xd3\x30");
    assert_eq!(conv("\u{10000}".as_bytes(), "UTF8", "GB18030").unwrap(), b"\x90\x30\x81\x30");
    assert_eq!(conv(b"\x90\x30\x81\x30", "GB18030", "UTF8").unwrap(), "\u{10000}".as_bytes());
}
