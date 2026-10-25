//! conversion_procs/euc_cn_and_mic/euc_cn_and_mic.c: EUC_CN and MULE_INTERNAL (GB2312).

use super::*;

/// euc_cn2mic: EUC_CN to MULE_INTERNAL
pub(super) fn euc_cn2mic(src: &[u8], p: &mut Vec<u8>, no_error: bool) -> Result<usize> {
    let mut i = 0;
    while i < src.len() {
        let len = src.len() - i;
        let c1 = src[i];
        if is_highbit_set(c1) {
            if len < 2 || !is_highbit_set(src[i + 1]) {
                if no_error {
                    break;
                }
                return Err(report_invalid_encoding(PG_EUC_CN, &src[i..]));
            }
            p.extend([LC_GB2312_80, c1, src[i + 1]]);
            i += 2;
        } else {
            // should be ASCII
            if c1 == 0 {
                if no_error {
                    break;
                }
                return Err(report_invalid_encoding(PG_EUC_CN, &src[i..]));
            }
            p.push(c1);
            i += 1;
        }
    }
    Ok(i)
}

/// mic2euc_cn: MULE_INTERNAL to EUC_CN
pub(super) fn mic2euc_cn(src: &[u8], p: &mut Vec<u8>, no_error: bool) -> Result<usize> {
    let mut i = 0;
    while i < src.len() {
        let len = src.len() - i;
        let c1 = src[i];
        if is_highbit_set(c1) {
            if c1 != LC_GB2312_80 {
                if no_error {
                    break;
                }
                return Err(report_untranslatable_char(PG_MULE_INTERNAL, PG_EUC_CN, &src[i..]));
            }
            if len < 3 || !is_highbit_set(src[i + 1]) || !is_highbit_set(src[i + 2]) {
                if no_error {
                    break;
                }
                return Err(report_invalid_encoding(PG_MULE_INTERNAL, &src[i..]));
            }
            p.extend([src[i + 1], src[i + 2]]);
            i += 3;
        } else {
            // should be ASCII
            if c1 == 0 {
                if no_error {
                    break;
                }
                return Err(report_invalid_encoding(PG_MULE_INTERNAL, &src[i..]));
            }
            p.push(c1);
            i += 1;
        }
    }
    Ok(i)
}
