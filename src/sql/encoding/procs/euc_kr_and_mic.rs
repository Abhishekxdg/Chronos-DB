//! conversion_procs/euc_kr_and_mic/euc_kr_and_mic.c: EUC_KR and MULE_INTERNAL (KS X 1001).

use super::super::wchar::verifymbchar;
use super::*;

/// euc_kr2mic: EUC_KR to MULE_INTERNAL
pub(super) fn euc_kr2mic(src: &[u8], p: &mut Vec<u8>, no_error: bool) -> Result<usize> {
    let mut i = 0;
    while i < src.len() {
        let c1 = src[i];
        if is_highbit_set(c1) {
            if verifymbchar(PG_EUC_KR, &src[i..]) != Some(2) {
                if no_error {
                    break;
                }
                return Err(report_invalid_encoding(PG_EUC_KR, &src[i..]));
            }
            p.extend([LC_KS5601, c1, src[i + 1]]);
            i += 2;
        } else {
            // should be ASCII
            if c1 == 0 {
                if no_error {
                    break;
                }
                return Err(report_invalid_encoding(PG_EUC_KR, &src[i..]));
            }
            p.push(c1);
            i += 1;
        }
    }
    Ok(i)
}

/// mic2euc_kr: MULE_INTERNAL to EUC_KR
pub(super) fn mic2euc_kr(src: &[u8], p: &mut Vec<u8>, no_error: bool) -> Result<usize> {
    let mut i = 0;
    while i < src.len() {
        let c1 = src[i];
        if !is_highbit_set(c1) {
            // ASCII
            if c1 == 0 {
                if no_error {
                    break;
                }
                return Err(report_invalid_encoding(PG_MULE_INTERNAL, &src[i..]));
            }
            p.push(c1);
            i += 1;
            continue;
        }
        let Some(l) = verifymbchar(PG_MULE_INTERNAL, &src[i..]) else {
            if no_error {
                break;
            }
            return Err(report_invalid_encoding(PG_MULE_INTERNAL, &src[i..]));
        };
        if c1 == LC_KS5601 {
            p.extend([src[i + 1], src[i + 2]]);
        } else {
            if no_error {
                break;
            }
            return Err(report_untranslatable_char(PG_MULE_INTERNAL, PG_EUC_KR, &src[i..]));
        }
        i += l;
    }
    Ok(i)
}
