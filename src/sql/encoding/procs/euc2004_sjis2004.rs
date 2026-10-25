//! conversion_procs/euc2004_sjis2004/euc2004_sjis2004.c: EUC_JIS_2004 and SHIFT_JIS_2004 (JIS X 0213), by
//! arithmetic on ku and ten. The C's int arithmetic is i32 here; a byte it writes is the int truncated.

use super::super::wchar::verifymbchar;
use super::*;

fn put(p: &mut Vec<u8>, v: i32) {
    p.push(v as u8);
}

/// The second SHIFT_JIS_2004 byte for `ten` of an odd or even ku, or None (a ten out of range)
fn ten_byte(ku: i32, ten: i32) -> Option<i32> {
    if ku % 2 != 0 {
        if (1..=63).contains(&ten) {
            Some(ten + 0x3f)
        } else if (64..=94).contains(&ten) {
            Some(ten + 0x40)
        } else {
            None
        }
    } else {
        Some(ten + 0x9e)
    }
}

/// euc_jis_20042shift_jis_2004: EUC_JIS_2004 to SHIFT_JIS_2004
pub(super) fn euc_jis_20042shift_jis_2004(src: &[u8], p: &mut Vec<u8>, no_error: bool) -> Result<usize> {
    let mut i = 0;
    while i < src.len() {
        let euc = &src[i..];
        let c1 = euc[0] as i32;
        if c1 & 0x80 == 0 {
            // ASCII
            if c1 == 0 {
                if no_error {
                    break;
                }
                return Err(report_invalid_encoding(PG_EUC_JIS_2004, euc));
            }
            put(p, c1);
            i += 1;
            continue;
        }
        let Some(l) = verifymbchar(PG_EUC_JIS_2004, euc) else {
            if no_error {
                break;
            }
            return Err(report_invalid_encoding(PG_EUC_JIS_2004, euc));
        };
        if c1 == SS2 as i32 && l == 2 {
            // JIS X 0201 kana
            p.push(euc[1]);
        } else if c1 == SS3 as i32 && l == 3 {
            // JIS X 0213 plane 2
            let ku = euc[1] as i32 - 0xa0;
            let ten = euc[2] as i32 - 0xa0;
            match ku {
                1 | 3 | 4 | 5 | 8 | 12 | 13 | 14 | 15 => put(p, ((ku + 0x1df) >> 1) - (ku >> 3) * 3),
                78..=94 => put(p, (ku + 0x19b) >> 1),
                _ => {
                    // the C's `break` here leaves its switch, not the loop: with noError the character goes on
                    // without its first byte
                    if !no_error {
                        return Err(report_invalid_encoding(PG_EUC_JIS_2004, euc));
                    }
                }
            }
            match ten_byte(ku, ten) {
                Some(b) => put(p, b),
                None => {
                    if no_error {
                        break;
                    }
                    return Err(report_invalid_encoding(PG_EUC_JIS_2004, euc));
                }
            }
        } else if l == 2 {
            // JIS X 0213 plane 1
            let ku = c1 - 0xa0;
            let ten = euc[1] as i32 - 0xa0;
            if (1..=62).contains(&ku) {
                put(p, (ku + 0x101) >> 1);
            } else if (63..=94).contains(&ku) {
                put(p, (ku + 0x181) >> 1);
            } else {
                if no_error {
                    break;
                }
                return Err(report_invalid_encoding(PG_EUC_JIS_2004, euc));
            }
            match ten_byte(ku, ten) {
                Some(b) => put(p, b),
                None => {
                    if no_error {
                        break;
                    }
                    return Err(report_invalid_encoding(PG_EUC_JIS_2004, euc));
                }
            }
        } else {
            if no_error {
                break;
            }
            return Err(report_invalid_encoding(PG_EUC_JIS_2004, euc));
        }
        i += l;
    }
    Ok(i)
}

/// get_ten: the ten a SHIFT_JIS_2004 second byte gives, and whether its ku is odd (1) or even (0); None for a byte
/// out of range
fn get_ten(b: i32) -> Option<(i32, i32)> {
    if (0x40..=0x7e).contains(&b) {
        Some((b - 0x3f, 1))
    } else if (0x80..=0x9e).contains(&b) {
        Some((b - 0x40, 1))
    } else if (0x9f..=0xfc).contains(&b) {
        Some((b - 0x9e, 0))
    } else {
        None
    }
}

/// shift_jis_20042euc_jis_2004: SHIFT_JIS_2004 to EUC_JIS_2004
pub(super) fn shift_jis_20042euc_jis_2004(src: &[u8], p: &mut Vec<u8>, no_error: bool) -> Result<usize> {
    let mut i = 0;
    while i < src.len() {
        let sjis = &src[i..];
        let c1 = sjis[0] as i32;
        if c1 & 0x80 == 0 {
            // ASCII
            if c1 == 0 {
                if no_error {
                    break;
                }
                return Err(report_invalid_encoding(PG_SHIFT_JIS_2004, sjis));
            }
            put(p, c1);
            i += 1;
            continue;
        }
        let l = match verifymbchar(PG_SHIFT_JIS_2004, sjis) {
            Some(l) if l <= sjis.len() => l,
            _ => {
                if no_error {
                    break;
                }
                return Err(report_invalid_encoding(PG_SHIFT_JIS_2004, sjis));
            }
        };
        if (0xa1..=0xdf).contains(&c1) && l == 1 {
            // JIS X0201 (1 byte kana)
            p.push(SS2);
            put(p, c1);
        } else if l == 2 {
            let c2 = sjis[1] as i32;
            let mut plane = 1;
            let ku;
            // JIS X 0213
            let tk = get_ten(c2);
            let Some((t, kubun)) = tk.filter(|_| (0x81..=0x9f).contains(&c1) || (0xe0..=0xfc).contains(&c1)) else {
                // a ten out of range, or a first byte that is neither plane
                if no_error {
                    break;
                }
                return Err(report_invalid_encoding(PG_SHIFT_JIS_2004, sjis));
            };
            let ten = t;
            if (0x81..=0x9f).contains(&c1) {
                // plane 1 1ku-62ku
                ku = (c1 << 1) - 0x100 - kubun;
            } else if (0xe0..=0xef).contains(&c1) {
                // plane 1 62ku-94ku
                ku = (c1 << 1) - 0x180 - kubun;
            } else if (0xf0..=0xf3).contains(&c1) {
                // plane 2 1,3,4,5,8,12,13,14,15 ku
                plane = 2;
                ku = match c1 {
                    0xf0 => {
                        if kubun == 0 {
                            8
                        } else {
                            1
                        }
                    }
                    0xf1 => {
                        if kubun == 0 {
                            4
                        } else {
                            3
                        }
                    }
                    0xf2 => {
                        if kubun == 0 {
                            12
                        } else {
                            5
                        }
                    }
                    _ => {
                        if kubun == 0 {
                            14
                        } else {
                            13
                        }
                    }
                };
            } else {
                // plane 2 78-94ku
                plane = 2;
                ku = if c1 == 0xf4 && kubun == 1 { 15 } else { (c1 << 1) - 0x19a - kubun };
            }
            if plane == 2 {
                p.push(SS3);
            }
            put(p, ku + 0xa0);
            put(p, ten + 0xa0);
        }
        i += l;
    }
    Ok(i)
}
