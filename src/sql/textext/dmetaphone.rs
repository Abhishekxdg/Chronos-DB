//! `dmetaphone(text)` and `dmetaphone_alt(text)`: Double Metaphone as contrib/fuzzystrmatch has it
//! (Lawrence Philips's algorithm through Text::DoubleMetaphone). It reads the word as bytes,
//! upper-cased (ASCII only), and gives two codes of up to four characters: the primary and the
//! alternate, which differ where a name can be said two ways. A multibyte character is several bytes
//! that no rule knows, so it is skipped (two bytes that happen to be the C with cedilla and N with
//! tilde of ISO 8859-1, 0xC7 and 0xD1, are taken for those, as they are in Postgres).

/// The two codes being built.
#[derive(Default)]
struct Codes {
    primary: Vec<u8>,
    secondary: Vec<u8>,
}

impl Codes {
    fn add(&mut self, primary: &str, secondary: &str) {
        self.primary.extend_from_slice(primary.as_bytes());
        self.secondary.extend_from_slice(secondary.as_bytes());
    }
}

/// `strncmp(a, b, n) == 0`: the first `n` bytes agree, a string ending where the other does.
fn strncmp_eq(a: &[u8], b: &[u8], n: usize) -> bool {
    for i in 0..n {
        let (x, y) = (a.get(i).copied().unwrap_or(0), b.get(i).copied().unwrap_or(0));
        if x != y {
            return false;
        }
        if x == 0 {
            return true;
        }
    }
    true
}

/// The primary and the alternate code of a word (`DoubleMetaphone`). The word is upper-cased and
/// padded with five blanks so that a rule can look past its end; `length` and `last` are the word's
/// own length and last place.
pub(super) fn double_metaphone(word: &str) -> (String, String) {
    let bytes = word.as_bytes();
    let length = bytes.len() as isize;
    let last = length - 1;
    let mut orig: Vec<u8> = bytes.iter().map(u8::to_ascii_uppercase).collect();
    orig.extend_from_slice(b"     ");
    let padded = orig.len() as isize;

    // the byte at a place, 0 outside the word and its padding
    let get = |pos: isize| -> u8 { if pos < 0 || pos >= padded { 0 } else { orig[pos as usize] } };
    let is_vowel = |pos: isize| matches!(get(pos), b'A' | b'E' | b'I' | b'O' | b'U' | b'Y');
    // whether the text at `start` begins with any of `tests`, comparing `len` bytes of each
    let string_at = |start: isize, len: usize, tests: &[&str]| -> bool {
        start >= 0 && start < padded && tests.iter().any(|t| strncmp_eq(&orig[start as usize..], t.as_bytes(), len))
    };
    // a Slavic or Germanic name has a W or K or CZ somewhere in it
    let slavo_germanic = orig.iter().any(|&b| b == b'W' || b == b'K') || orig.windows(2).any(|w| w == b"CZ");

    let mut codes = Codes::default();
    let mut current: isize = 0;

    // skip the first letter of these at the start of a word
    if string_at(0, 2, &["GN", "KN", "PN", "WR", "PS"]) {
        current += 1;
    }
    // an initial X is pronounced Z, as in Xavier (and Z is S)
    if get(0) == b'X' {
        codes.add("S", "S");
        current += 1;
    }

    while codes.primary.len() < 4 || codes.secondary.len() < 4 {
        if current >= length {
            break;
        }
        match get(current) {
            b'A' | b'E' | b'I' | b'O' | b'U' | b'Y' => {
                // every vowel at the start becomes A
                if current == 0 {
                    codes.add("A", "A");
                }
                current += 1;
            }
            b'B' => {
                // "-mb", as in dumb, is taken care of where the M is
                codes.add("P", "P");
                current += if get(current + 1) == b'B' { 2 } else { 1 };
            }
            // C with cedilla
            0xC7 => {
                codes.add("S", "S");
                current += 1;
            }
            b'C' => {
                // various Germanic
                if current > 1
                    && !is_vowel(current - 2)
                    && string_at(current - 1, 3, &["ACH"])
                    && get(current + 2) != b'I'
                    && (get(current + 2) != b'E' || string_at(current - 2, 6, &["BACHER", "MACHER"]))
                {
                    codes.add("K", "K");
                    current += 2;
                    continue;
                }
                // the special case caesar
                if current == 0 && string_at(current, 6, &["CAESAR"]) {
                    codes.add("S", "S");
                    current += 2;
                    continue;
                }
                // Italian chianti
                if string_at(current, 4, &["CHIA"]) {
                    codes.add("K", "K");
                    current += 2;
                    continue;
                }
                if string_at(current, 2, &["CH"]) {
                    // michael
                    if current > 0 && string_at(current, 4, &["CHAE"]) {
                        codes.add("K", "X");
                        current += 2;
                        continue;
                    }
                    // Greek roots, as in chemistry and chorus
                    if current == 0
                        && (string_at(current + 1, 5, &["HARAC", "HARIS"])
                            || string_at(current + 1, 3, &["HOR", "HYM", "HIA", "HEM"]))
                        && !string_at(0, 5, &["CHORE"])
                    {
                        codes.add("K", "K");
                        current += 2;
                        continue;
                    }
                    // Germanic, Greek, or otherwise CH for the KH sound
                    if string_at(0, 4, &["VAN ", "VON "])
                        || string_at(0, 3, &["SCH"])
                        // architect but not arch, orchestra, orchid
                        || string_at(current - 2, 6, &["ORCHES", "ARCHIT", "ORCHID"])
                        || string_at(current + 2, 1, &["T", "S"])
                        // wachtler, wechsler, but not tichner
                        || ((string_at(current - 1, 1, &["A", "O", "U", "E"]) || current == 0)
                            && string_at(current + 2, 1, &["L", "R", "N", "M", "B", "H", "F", "V", "W", " "]))
                    {
                        codes.add("K", "K");
                    } else if current > 0 {
                        // McHugh
                        if string_at(0, 2, &["MC"]) {
                            codes.add("K", "K");
                        } else {
                            codes.add("X", "K");
                        }
                    } else {
                        codes.add("X", "X");
                    }
                    current += 2;
                    continue;
                }
                // czerny
                if string_at(current, 2, &["CZ"]) && !string_at(current - 2, 4, &["WICZ"]) {
                    codes.add("S", "X");
                    current += 2;
                    continue;
                }
                // focaccia
                if string_at(current + 1, 3, &["CIA"]) {
                    codes.add("X", "X");
                    current += 3;
                    continue;
                }
                // double C, but not McClellan
                if string_at(current, 2, &["CC"]) && !(current == 1 && get(0) == b'M') {
                    // bellocchio but not bacchus
                    if string_at(current + 2, 1, &["I", "E", "H"]) && !string_at(current + 2, 2, &["HU"]) {
                        // accident, accede, succeed
                        if (current == 1 && get(current - 1) == b'A') || string_at(current - 1, 5, &["UCCEE", "UCCES"])
                        {
                            codes.add("KS", "KS");
                        } else {
                            // bacci, bertucci, other Italian
                            codes.add("X", "X");
                        }
                        current += 3;
                    } else {
                        // Pierce's rule
                        codes.add("K", "K");
                        current += 2;
                    }
                    continue;
                }
                if string_at(current, 2, &["CK", "CG", "CQ"]) {
                    codes.add("K", "K");
                    current += 2;
                    continue;
                }
                if string_at(current, 2, &["CI", "CE", "CY"]) {
                    // Italian or English
                    if string_at(current, 3, &["CIO", "CIE", "CIA"]) {
                        codes.add("S", "X");
                    } else {
                        codes.add("S", "S");
                    }
                    current += 2;
                    continue;
                }
                codes.add("K", "K");
                // a name sent in as mac caffrey or mac gregor
                if string_at(current + 1, 2, &[" C", " Q", " G"]) {
                    current += 3;
                } else if string_at(current + 1, 1, &["C", "K", "Q"]) && !string_at(current + 1, 2, &["CE", "CI"]) {
                    current += 2;
                } else {
                    current += 1;
                }
            }
            b'D' => {
                if string_at(current, 2, &["DG"]) {
                    if string_at(current + 2, 1, &["I", "E", "Y"]) {
                        // edge
                        codes.add("J", "J");
                        current += 3;
                    } else {
                        // edgar
                        codes.add("TK", "TK");
                        current += 2;
                    }
                    continue;
                }
                if string_at(current, 2, &["DT", "DD"]) {
                    codes.add("T", "T");
                    current += 2;
                    continue;
                }
                codes.add("T", "T");
                current += 1;
            }
            b'F' => {
                current += if get(current + 1) == b'F' { 2 } else { 1 };
                codes.add("F", "F");
            }
            b'G' => {
                if get(current + 1) == b'H' {
                    if current > 0 && !is_vowel(current - 1) {
                        codes.add("K", "K");
                        current += 2;
                        continue;
                    }
                    // ghislane, ghiradelli
                    if current == 0 {
                        if get(current + 2) == b'I' {
                            codes.add("J", "J");
                        } else {
                            codes.add("K", "K");
                        }
                        current += 2;
                        continue;
                    }
                    // Parker's rule (with some further refinements): hugh; bough; broughton
                    if (current > 1 && string_at(current - 2, 1, &["B", "H", "D"]))
                        || (current > 2 && string_at(current - 3, 1, &["B", "H", "D"]))
                        || (current > 3 && string_at(current - 4, 1, &["B", "H"]))
                    {
                        current += 2;
                        continue;
                    }
                    // laugh, McLaughlin, cough, gough, rough, tough
                    if current > 2 && get(current - 1) == b'U' && string_at(current - 3, 1, &["C", "G", "L", "R", "T"])
                    {
                        codes.add("F", "F");
                    } else if current > 0 && get(current - 1) != b'I' {
                        codes.add("K", "K");
                    }
                    current += 2;
                    continue;
                }
                if get(current + 1) == b'N' {
                    if current == 1 && is_vowel(0) && !slavo_germanic {
                        codes.add("KN", "N");
                    } else if !string_at(current + 2, 2, &["EY"]) && get(current + 1) != b'Y' && !slavo_germanic {
                        // not cagney
                        codes.add("N", "KN");
                    } else {
                        codes.add("KN", "KN");
                    }
                    current += 2;
                    continue;
                }
                // tagliaro
                if string_at(current + 1, 2, &["LI"]) && !slavo_germanic {
                    codes.add("KL", "L");
                    current += 2;
                    continue;
                }
                // -ges-, -gep-, -gel-, -gie- at the beginning
                if current == 0
                    && (get(current + 1) == b'Y'
                        || string_at(
                            current + 1,
                            2,
                            &["ES", "EP", "EB", "EL", "EY", "IB", "IL", "IN", "IE", "EI", "ER"],
                        ))
                {
                    codes.add("K", "J");
                    current += 2;
                    continue;
                }
                // -ger-, -gy-
                if (string_at(current + 1, 2, &["ER"]) || get(current + 1) == b'Y')
                    && !string_at(0, 6, &["DANGER", "RANGER", "MANGER"])
                    && !string_at(current - 1, 1, &["E", "I"])
                    && !string_at(current - 1, 3, &["RGY", "OGY"])
                {
                    codes.add("K", "J");
                    current += 2;
                    continue;
                }
                // Italian, as in biaggi
                if string_at(current + 1, 1, &["E", "I", "Y"]) || string_at(current - 1, 4, &["AGGI", "OGGI"]) {
                    if string_at(0, 4, &["VAN ", "VON "])
                        || string_at(0, 3, &["SCH"])
                        || string_at(current + 1, 2, &["ET"])
                    {
                        // obviously Germanic
                        codes.add("K", "K");
                    } else if string_at(current + 1, 4, &["IER "]) {
                        // always soft if a French ending
                        codes.add("J", "J");
                    } else {
                        codes.add("J", "K");
                    }
                    current += 2;
                    continue;
                }
                current += if get(current + 1) == b'G' { 2 } else { 1 };
                codes.add("K", "K");
            }
            b'H' => {
                // keep it only if first and before a vowel, or between two vowels
                if (current == 0 || is_vowel(current - 1)) && is_vowel(current + 1) {
                    codes.add("H", "H");
                    current += 2;
                } else {
                    // this takes care of HH too
                    current += 1;
                }
            }
            b'J' => {
                // obviously Spanish: jose, san jacinto
                if string_at(current, 4, &["JOSE"]) || string_at(0, 4, &["SAN "]) {
                    if (current == 0 && get(current + 4) == b' ') || string_at(0, 4, &["SAN "]) {
                        codes.add("H", "H");
                    } else {
                        codes.add("J", "H");
                    }
                    current += 1;
                    continue;
                }
                if current == 0 && !string_at(current, 4, &["JOSE"]) {
                    // Yankelovich, Jankelowicz
                    codes.add("J", "A");
                } else if is_vowel(current - 1)
                    && !slavo_germanic
                    && (get(current + 1) == b'A' || get(current + 1) == b'O')
                {
                    // the Spanish pronunciation of bajador
                    codes.add("J", "H");
                } else if current == last {
                    codes.add("J", "");
                } else if !string_at(current + 1, 1, &["L", "T", "K", "S", "N", "M", "B", "Z"])
                    && !string_at(current - 1, 1, &["S", "K", "L"])
                {
                    codes.add("J", "J");
                }
                // it could happen!
                current += if get(current + 1) == b'J' { 2 } else { 1 };
            }
            b'K' => {
                current += if get(current + 1) == b'K' { 2 } else { 1 };
                codes.add("K", "K");
            }
            b'L' => {
                if get(current + 1) == b'L' {
                    // Spanish, as in cabrillo and gallegos
                    if (current == length - 3 && string_at(current - 1, 4, &["ILLO", "ILLA", "ALLE"]))
                        || ((string_at(last - 1, 2, &["AS", "OS"]) || string_at(last, 1, &["A", "O"]))
                            && string_at(current - 1, 4, &["ALLE"]))
                    {
                        codes.add("L", "");
                        current += 2;
                        continue;
                    }
                    current += 2;
                } else {
                    current += 1;
                }
                codes.add("L", "L");
            }
            b'M' => {
                // dumb, thumb
                if (string_at(current - 1, 3, &["UMB"]) && (current + 1 == last || string_at(current + 2, 2, &["ER"])))
                    || get(current + 1) == b'M'
                {
                    current += 2;
                } else {
                    current += 1;
                }
                codes.add("M", "M");
            }
            b'N' => {
                current += if get(current + 1) == b'N' { 2 } else { 1 };
                codes.add("N", "N");
            }
            // N with tilde
            0xD1 => {
                current += 1;
                codes.add("N", "N");
            }
            b'P' => {
                if get(current + 1) == b'H' {
                    codes.add("F", "F");
                    current += 2;
                    continue;
                }
                // also campbell and raspberry
                current += if string_at(current + 1, 1, &["P", "B"]) { 2 } else { 1 };
                codes.add("P", "P");
            }
            b'Q' => {
                current += if get(current + 1) == b'Q' { 2 } else { 1 };
                codes.add("K", "K");
            }
            b'R' => {
                // French, as in rogier, but not hochmeier
                if current == last
                    && !slavo_germanic
                    && string_at(current - 2, 2, &["IE"])
                    && !string_at(current - 4, 2, &["ME", "MA"])
                {
                    codes.add("", "R");
                } else {
                    codes.add("R", "R");
                }
                current += if get(current + 1) == b'R' { 2 } else { 1 };
            }
            b'S' => {
                // island, isle, carlisle, carlysle
                if string_at(current - 1, 3, &["ISL", "YSL"]) {
                    current += 1;
                    continue;
                }
                // sugar-
                if current == 0 && string_at(current, 5, &["SUGAR"]) {
                    codes.add("X", "S");
                    current += 1;
                    continue;
                }
                if string_at(current, 2, &["SH"]) {
                    // Germanic
                    if string_at(current + 1, 4, &["HEIM", "HOEK", "HOLM", "HOLZ"]) {
                        codes.add("S", "S");
                    } else {
                        codes.add("X", "X");
                    }
                    current += 2;
                    continue;
                }
                // Italian and Armenian
                if string_at(current, 3, &["SIO", "SIA"]) || string_at(current, 4, &["SIAN"]) {
                    if slavo_germanic {
                        codes.add("S", "S");
                    } else {
                        codes.add("S", "X");
                    }
                    current += 3;
                    continue;
                }
                // German and anglicisations, as smith matching schmidt and snider matching schneider; also -sz-
                // in Slavic languages, though in Hungarian it is pronounced S
                if (current == 0 && string_at(current + 1, 1, &["M", "N", "L", "W"]))
                    || string_at(current + 1, 1, &["Z"])
                {
                    codes.add("S", "X");
                    current += if string_at(current + 1, 1, &["Z"]) { 2 } else { 1 };
                    continue;
                }
                if string_at(current, 2, &["SC"]) {
                    // Schlesinger's rule
                    if get(current + 2) == b'H' {
                        // Dutch origin, as in school and schooner
                        if string_at(current + 3, 2, &["OO", "ER", "EN", "UY", "ED", "EM"]) {
                            // schermerhorn, schenker
                            if string_at(current + 3, 2, &["ER", "EN"]) {
                                codes.add("X", "SK");
                            } else {
                                codes.add("SK", "SK");
                            }
                        } else if current == 0 && !is_vowel(3) && get(3) != b'W' {
                            codes.add("X", "S");
                        } else {
                            codes.add("X", "X");
                        }
                        current += 3;
                        continue;
                    }
                    if string_at(current + 2, 1, &["I", "E", "Y"]) {
                        codes.add("S", "S");
                    } else {
                        codes.add("SK", "SK");
                    }
                    current += 3;
                    continue;
                }
                // French, as in resnais and artois
                if current == last && string_at(current - 2, 2, &["AI", "OI"]) {
                    codes.add("", "S");
                } else {
                    codes.add("S", "S");
                }
                current += if string_at(current + 1, 1, &["S", "Z"]) { 2 } else { 1 };
            }
            b'T' => {
                if string_at(current, 4, &["TION"]) {
                    codes.add("X", "X");
                    current += 3;
                    continue;
                }
                if string_at(current, 3, &["TIA", "TCH"]) {
                    codes.add("X", "X");
                    current += 3;
                    continue;
                }
                if string_at(current, 2, &["TH"]) || string_at(current, 3, &["TTH"]) {
                    // thomas, thames, or Germanic
                    if string_at(current + 2, 2, &["OM", "AM"])
                        || string_at(0, 4, &["VAN ", "VON "])
                        || string_at(0, 3, &["SCH"])
                    {
                        codes.add("T", "T");
                    } else {
                        codes.add("0", "T");
                    }
                    current += 2;
                    continue;
                }
                current += if string_at(current + 1, 1, &["T", "D"]) { 2 } else { 1 };
                codes.add("T", "T");
            }
            b'V' => {
                current += if get(current + 1) == b'V' { 2 } else { 1 };
                codes.add("F", "F");
            }
            b'W' => {
                // it can be in the middle of a word too
                if string_at(current, 2, &["WR"]) {
                    codes.add("R", "R");
                    current += 2;
                    continue;
                }
                if current == 0 && (is_vowel(current + 1) || string_at(current, 2, &["WH"])) {
                    if is_vowel(current + 1) {
                        // Wasserman should match Vasserman
                        codes.add("A", "F");
                    } else {
                        // Uomo should match Womo
                        codes.add("A", "A");
                    }
                }
                // Arnow should match Arnoff
                if (current == last && is_vowel(current - 1))
                    || string_at(current - 1, 5, &["EWSKI", "EWSKY", "OWSKI", "OWSKY"])
                    || string_at(0, 3, &["SCH"])
                {
                    codes.add("", "F");
                    current += 1;
                    continue;
                }
                // Polish, as in filipowicz
                if string_at(current, 4, &["WICZ", "WITZ"]) {
                    codes.add("TS", "FX");
                    current += 4;
                    continue;
                }
                // else skip it
                current += 1;
            }
            b'X' => {
                // French, as in breaux
                if !(current == last
                    && (string_at(current - 3, 3, &["IAU", "EAU"]) || string_at(current - 2, 2, &["AU", "OU"])))
                {
                    codes.add("KS", "KS");
                }
                current += if string_at(current + 1, 1, &["C", "X"]) { 2 } else { 1 };
            }
            b'Z' => {
                // Chinese pinyin, as in zhao
                if get(current + 1) == b'H' {
                    codes.add("J", "J");
                    current += 2;
                    continue;
                } else if string_at(current + 1, 2, &["ZO", "ZI", "ZA"])
                    || (slavo_germanic && current > 0 && get(current - 1) != b'T')
                {
                    codes.add("S", "TS");
                } else {
                    codes.add("S", "S");
                }
                current += if get(current + 1) == b'Z' { 2 } else { 1 };
            }
            _ => current += 1,
        }
    }
    codes.primary.truncate(4);
    codes.secondary.truncate(4);
    (String::from_utf8_lossy(&codes.primary).into_owned(), String::from_utf8_lossy(&codes.secondary).into_owned())
}
