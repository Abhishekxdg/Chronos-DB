//! conversion_procs/euc_tw_and_big5/euc_tw_and_big5.c: EUC_TW, Big5 and MULE_INTERNAL (CNS 11643-1992), through
//! big5.c's mapping.

use super::super::wchar::verifymbchar;
use super::big5::{big5_to_cns, cns_to_big5};
use super::*;

/// The loop these functions share: ASCII as it is, a zero byte invalid, each other character verified in
/// `encoding` and handed to `each`; `each` gives false for a character it can't convert
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
        if !is_highbit_set(c1) {
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

fn push16(p: &mut Vec<u8>, v: u16) {
    p.extend([(v >> 8) as u8, v as u8]);
}

/// euc_tw2big5: EUC_TW to Big5
pub(super) fn euc_tw2big5(src: &[u8], p: &mut Vec<u8>, no_error: bool) -> Result<usize> {
    each_char(src, p, PG_EUC_TW, PG_BIG5, no_error, |p, euc| {
        let (lc, cns) = if euc[0] == SS2 {
            // plane No.
            let lc = match euc[1] {
                0xa1 => LC_CNS11643_1,
                0xa2 => LC_CNS11643_2,
                c => c.wrapping_sub(0xa3).wrapping_add(LC_CNS11643_3),
            };
            (lc, (euc[2] as u16) << 8 | euc[3] as u16)
        } else {
            // CNS11643-1
            (LC_CNS11643_1, (euc[0] as u16) << 8 | euc[1] as u16)
        };
        let big5 = cns_to_big5(cns, lc);
        if big5 == 0 {
            return false;
        }
        push16(p, big5);
        true
    })
}

/// big52euc_tw: Big5 to EUC_TW
pub(super) fn big52euc_tw(src: &[u8], p: &mut Vec<u8>, no_error: bool) -> Result<usize> {
    each_char(src, p, PG_BIG5, PG_EUC_TW, no_error, |p, big5| {
        let (cns, lc) = big5_to_cns((big5[0] as u16) << 8 | big5[1] as u16);
        if lc == LC_CNS11643_1 {
            push16(p, cns);
        } else if lc == LC_CNS11643_2 {
            p.extend([SS2, 0xa2]);
            push16(p, cns);
        } else if (LC_CNS11643_3..=LC_CNS11643_7).contains(&lc) {
            p.extend([SS2, lc - LC_CNS11643_3 + 0xa3]);
            push16(p, cns);
        } else {
            return false;
        }
        true
    })
}

/// euc_tw2mic: EUC_TW to MULE_INTERNAL (every verified character converts)
pub(super) fn euc_tw2mic(src: &[u8], p: &mut Vec<u8>, no_error: bool) -> Result<usize> {
    each_char(src, p, PG_EUC_TW, PG_MULE_INTERNAL, no_error, |p, euc| {
        if euc[0] == SS2 {
            // plane No.
            match euc[1] {
                0xa1 => p.push(LC_CNS11643_1),
                0xa2 => p.push(LC_CNS11643_2),
                c => p.extend([LCPRV2_B, c.wrapping_sub(0xa3).wrapping_add(LC_CNS11643_3)]),
            }
            p.extend([euc[2], euc[3]]);
        } else {
            // CNS11643-1
            p.extend([LC_CNS11643_1, euc[0], euc[1]]);
        }
        true
    })
}

/// mic2euc_tw: MULE_INTERNAL to EUC_TW
pub(super) fn mic2euc_tw(src: &[u8], p: &mut Vec<u8>, no_error: bool) -> Result<usize> {
    each_char(src, p, PG_MULE_INTERNAL, PG_EUC_TW, no_error, |p, mic| {
        let c1 = mic[0];
        if c1 == LC_CNS11643_1 {
            p.extend([mic[1], mic[2]]);
        } else if c1 == LC_CNS11643_2 {
            p.extend([SS2, 0xa2, mic[1], mic[2]]);
        } else if c1 == LCPRV2_B && (LC_CNS11643_3..=LC_CNS11643_7).contains(&mic[1]) {
            p.extend([SS2, mic[1] - LC_CNS11643_3 + 0xa3, mic[2], mic[3]]);
        } else {
            return false;
        }
        true
    })
}

/// big52mic: Big5 to MULE_INTERNAL
pub(super) fn big52mic(src: &[u8], p: &mut Vec<u8>, no_error: bool) -> Result<usize> {
    each_char(src, p, PG_BIG5, PG_MULE_INTERNAL, no_error, |p, big5| {
        let (cns, lc) = big5_to_cns((big5[0] as u16) << 8 | big5[1] as u16);
        if lc == 0 {
            return false;
        }
        if lc == LC_CNS11643_3 || lc == LC_CNS11643_4 {
            p.push(LCPRV2_B);
        }
        p.push(lc); // plane No.
        push16(p, cns);
        true
    })
}

/// mic2big5: MULE_INTERNAL to Big5
pub(super) fn mic2big5(src: &[u8], p: &mut Vec<u8>, no_error: bool) -> Result<usize> {
    each_char(src, p, PG_MULE_INTERNAL, PG_BIG5, no_error, |p, mic| {
        let c1 = mic[0];
        if c1 != LC_CNS11643_1 && c1 != LC_CNS11643_2 && c1 != LCPRV2_B {
            return false;
        }
        let (plane, cns) = if c1 == LCPRV2_B {
            (mic[1], (mic[2] as u16) << 8 | mic[3] as u16) // get plane no.
        } else {
            (c1, (mic[1] as u16) << 8 | mic[2] as u16)
        };
        let big5 = cns_to_big5(cns, plane);
        if big5 == 0 {
            return false;
        }
        push16(p, big5);
        true
    })
}
