//! conversion_procs/euc_tw_and_big5/big5.c: Big5 to CNS 11643-1992 and back, by ranges of codes that map in order
//! (`BinarySearchRange`) and a few single codes (`b1c4`, `b2c3`).

use super::*;

/// BinarySearchRange: the code `code` maps to through `array`'s ranges (`high` its last range searched), or 0.
/// Which way it converts is told by the code: from 0xa140 up it is Big5.
fn binary_search_range(array: &[Codes], mut high: i32, code: u16) -> u16 {
    let mut low: i32 = 0;
    let mut mid = high >> 1;
    while low <= high {
        let m = mid as usize;
        if array[m].code <= code && array[m + 1].code > code {
            if array[m].peer == 0 {
                return 0;
            }
            let code = code as i32;
            let (acode, peer) = (array[m].code as i32, array[m].peer as i32);
            if code >= 0xa140 {
                // big5 to cns: big5's second byte runs 0x40-0x7e then 0xa1-0xfe (radix 0x9d), with a bias of -0x22
                // between the two regions
                let tmp = ((code & 0xff00) - (acode & 0xff00)) >> 8;
                let high = code & 0x00ff;
                let low = acode & 0x00ff;
                let distance = tmp * 0x9d + high - low
                    + if high >= 0xa1 {
                        if low >= 0xa1 { 0 } else { -0x22 }
                    } else if low >= 0xa1 {
                        0x22
                    } else {
                        0
                    };
                // the distance as a code point: the low byte is 0x21 plus mod 0x5e
                let tmp = (peer & 0x00ff) + distance - 0x21;
                let tmp = (peer & 0xff00) + ((tmp / 0x5e) << 8) + 0x21 + tmp % 0x5e;
                return tmp as u16;
            } else {
                // cns to big5: ISO charsets have radix 0x5e and no bias
                let tmp = ((code & 0xff00) - (acode & 0xff00)) >> 8;
                let distance = tmp * 0x5e + ((code & 0x00ff) - (acode & 0x00ff));
                let low = peer & 0x00ff;
                let tmp = low + distance - if low >= 0xa1 { 0x62 } else { 0x40 };
                let low = tmp % 0x9d;
                let tmp = (peer & 0xff00) + ((tmp / 0x9d) << 8) + if low > 0x3e { 0x62 } else { 0x40 } + low;
                return tmp as u16;
            }
        } else if array[m].code > code {
            high = mid - 1;
        } else {
            low = mid + 1;
        }
        mid = (low + high) >> 1;
    }
    0
}

/// BIG5toCNS: a Big5 code's CNS 11643 code and plane (its LC_CNS11643_* byte); plane 0 and '?' for none
pub(super) fn big5_to_cns(big5: u16) -> (u16, u8) {
    let mut lc = 0;
    let cns;
    if big5 < 0xc940 {
        // level 1
        if let Some(e) = b1c4.iter().find(|e| e[0] == big5) {
            return (e[1] | 0x8080, LC_CNS11643_4);
        }
        cns = binary_search_range(&big5Level1ToCnsPlane1, 23, big5);
        if cns > 0 {
            lc = LC_CNS11643_1;
        }
    } else if big5 == 0xc94a {
        // level 2
        return (0x4442 | 0x8080, LC_CNS11643_1);
    } else {
        // level 2
        if let Some(e) = b2c3.iter().find(|e| e[0] == big5) {
            return (e[1] | 0x8080, LC_CNS11643_3);
        }
        cns = binary_search_range(&big5Level2ToCnsPlane2, 46, big5);
        if cns > 0 {
            lc = LC_CNS11643_2;
        }
    }
    if cns == 0 {
        // no mapping Big5 to CNS 11643-1992
        return (b'?' as u16, 0);
    }
    (cns | 0x8080, lc)
}

/// CNStoBIG5: a CNS 11643 code of plane `lc` as Big5, 0 for none
pub(super) fn cns_to_big5(cns: u16, lc: u8) -> u16 {
    let cns = cns & 0x7f7f;
    match lc {
        LC_CNS11643_1 => binary_search_range(&cnsPlane1ToBig5Level1, 24, cns),
        LC_CNS11643_2 => binary_search_range(&cnsPlane2ToBig5Level2, 47, cns),
        LC_CNS11643_3 => b2c3.iter().find(|e| e[1] == cns).map_or(0, |e| e[0]),
        LC_CNS11643_4 => b1c4.iter().find(|e| e[1] == cns).map_or(0, |e| e[0]),
        _ => 0,
    }
}
