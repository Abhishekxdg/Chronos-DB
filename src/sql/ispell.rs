//! The ISpell template of text search dictionaries: a dictionary file (`word/FLAGS`, a flag for each affix rule a
//! word takes) and an affix file, in ispell's own format (`flag *A:` and `find > -strip,add`) or hunspell's
//! (`SFX A Y 1`, `SFX A strip add condition`, with `FLAG long` and `FLAG num`). A word's normal forms are those
//! dictionary words it comes from by taking off a prefix and a suffix; compound words are taken apart into dictionary
//! words that may be compounded (the file's compound flag).

use std::collections::{BTreeMap, HashMap};

#[derive(Clone, Debug)]
enum Piece {
    Any,
    Lit(char),
    Class(Vec<char>, bool),
}

#[derive(Clone, Debug)]
struct Aff {
    flag: String,
    suffix: bool,
    cross: bool,
    find: Vec<Piece>,
    strip: String,
    add: String,
}

#[derive(Clone, Debug, Default)]
pub struct Ispell {
    words: HashMap<String, Vec<String>>,
    affs: Vec<Aff>,
    compound: Option<String>,
    only_compound: Option<String>,
}

fn pattern(s: &str) -> Vec<Piece> {
    let c: Vec<char> = s.chars().collect();
    let (mut out, mut i) = (vec![], 0);
    while i < c.len() {
        match c[i] {
            '.' => out.push(Piece::Any),
            '[' => {
                let mut j = i + 1;
                let neg = c.get(j) == Some(&'^');
                if neg {
                    j += 1;
                }
                let mut set = vec![];
                while j < c.len() && c[j] != ']' {
                    set.push(c[j].to_ascii_lowercase());
                    j += 1;
                }
                out.push(Piece::Class(set, neg));
                i = j;
            }
            ch => out.push(Piece::Lit(ch.to_ascii_lowercase())),
        }
        i += 1;
    }
    out
}

/// Whether the pattern matches the end (a suffix rule) or start (a prefix rule) of `word`.
fn matches(p: &[Piece], word: &str, suffix: bool) -> bool {
    let w: Vec<char> = word.chars().collect();
    if p.len() > w.len() {
        return false;
    }
    let part: &[char] = if suffix { &w[w.len() - p.len()..] } else { &w[..p.len()] };
    p.iter().zip(part).all(|(pc, c)| match pc {
        Piece::Any => true,
        Piece::Lit(l) => l == c,
        Piece::Class(set, neg) => set.contains(c) != *neg,
    })
}

/// A flag field of a dictionary word or rule, as separate flags: one character each, two (`FLAG long`), or numbers
/// between commas (`FLAG num`).
fn split_flags(s: &str, mode: &str) -> Vec<String> {
    match mode {
        "long" => s.chars().collect::<Vec<_>>().chunks(2).map(|c| c.iter().collect()).collect(),
        "num" => s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect(),
        _ => s.chars().map(|c| c.to_string()).collect(),
    }
}

impl Ispell {
    pub fn load(dict: &str, affix: &str) -> Result<Ispell, String> {
        let mut me = Ispell::default();
        let mut mode = "char".to_string();
        let hunspell = affix.lines().any(|l| {
            let w = l.split_whitespace().next().unwrap_or("");
            matches!(w, "PFX" | "SFX" | "FLAG" | "SET" | "COMPOUNDFLAG")
        });
        if hunspell {
            #[allow(clippy::type_complexity)]
            let (mut cross, mut current): (HashMap<(String, bool), bool>, Option<(String, bool)>) =
                (HashMap::new(), None);
            for line in affix.lines() {
                let w: Vec<&str> = line.split('#').next().unwrap_or("").split_whitespace().collect();
                match w.as_slice() {
                    ["FLAG", m] => mode = (*m).to_string(),
                    ["COMPOUNDFLAG", f] => me.compound = Some((*f).to_string()),
                    ["ONLYINCOMPOUND", f] => me.only_compound = Some((*f).to_string()),
                    [kind @ ("PFX" | "SFX"), flag, y, count]
                        if count.parse::<usize>().is_ok() && matches!(*y, "Y" | "N") =>
                    {
                        let k = (flag.to_string(), *kind == "SFX");
                        cross.insert(k.clone(), *y == "Y");
                        current = Some(k);
                    }
                    [kind @ ("PFX" | "SFX"), flag, strip, add, rest @ ..] => {
                        let suffix = *kind == "SFX";
                        let add = add.split('/').next().unwrap_or("");
                        let find = rest.first().copied().unwrap_or(".");
                        me.affs.push(Aff {
                            flag: (*flag).to_string(),
                            suffix,
                            cross: cross.get(&(flag.to_string(), suffix)).copied().unwrap_or(false),
                            find: pattern(find),
                            strip: if *strip == "0" { String::new() } else { strip.to_lowercase() },
                            add: if add == "0" { String::new() } else { add.to_lowercase() },
                        });
                        let _ = &current;
                    }
                    _ => {}
                }
            }
        } else {
            let (mut suffix, mut flag, mut cross) = (false, String::new(), false);
            for line in affix.lines() {
                let line = line.split('#').next().unwrap_or("").trim_end().to_string();
                let t = line.trim();
                if t.is_empty() {
                    continue;
                }
                if let Some(rest) = t.strip_prefix("compoundwords controlled") {
                    me.compound = Some(rest.trim().to_string());
                } else if t == "prefixes" {
                    suffix = false;
                } else if t == "suffixes" {
                    suffix = true;
                } else if let Some(rest) = t.strip_prefix("flag ") {
                    let rest = rest.trim().trim_end_matches(':');
                    cross = rest.starts_with('*');
                    flag = rest.trim_start_matches('*').trim_start_matches('~').trim().to_string();
                } else if let Some((find, add)) = t.split_once('>') {
                    let (find, add) = (find.trim(), add.trim());
                    let (strip, add) = match add.split_once(',') {
                        Some((s, a)) => (s.trim().trim_start_matches('-'), a.trim()),
                        None if add.starts_with('-') => (add.trim_start_matches('-'), ""),
                        None => ("", add),
                    };
                    me.affs.push(Aff {
                        flag: flag.clone(),
                        suffix,
                        cross,
                        find: pattern(find),
                        strip: strip.to_lowercase(),
                        add: add.to_lowercase(),
                    });
                }
            }
        }
        for line in dict.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let (w, flags) = line.split_once('/').unwrap_or((line, ""));
            me.words.entry(w.to_lowercase()).or_default().extend(split_flags(flags.trim_end_matches('\\'), &mode));
        }
        if me.words.is_empty() {
            return Err("dictionary file is empty".into());
        }
        Ok(me)
    }

    fn has(&self, base: &str, flag: &str) -> bool {
        self.words.get(base).is_some_and(|f| f.iter().any(|x| x == flag))
    }

    fn only_in_compound(&self, base: &str) -> bool {
        self.only_compound.as_ref().is_some_and(|f| self.has(base, f))
    }

    /// The dictionary words `word` is a form of.
    pub fn normal_forms(&self, word: &str) -> Vec<String> {
        let w = word.to_lowercase();
        let mut out: Vec<String> = vec![];
        let mut push = |s: String, out: &mut Vec<String>| {
            if !out.contains(&s) {
                out.push(s);
            }
        };
        if self.words.contains_key(&w) && !self.only_in_compound(&w) {
            push(w.clone(), &mut out);
        }
        let try_suffix = |me: &Ispell,
                          stem: &str,
                          need_cross: bool,
                          out: &mut Vec<String>,
                          push: &mut dyn FnMut(String, &mut Vec<String>)| {
            for a in me.affs.iter().filter(|a| a.suffix) {
                if need_cross && !a.cross {
                    continue;
                }
                if let Some(base_part) = stem.strip_suffix(a.add.as_str()) {
                    let base = format!("{base_part}{}", a.strip);
                    if matches(&a.find, &base, true) && me.has(&base, &a.flag) && !me.only_in_compound(&base) {
                        push(base, out);
                    }
                }
            }
        };
        // a suffix taken off
        try_suffix(self, &w, false, &mut out, &mut push);
        // a prefix taken off, with or without a suffix after it
        for a in self.affs.iter().filter(|a| !a.suffix) {
            if let Some(rest) = w.strip_prefix(a.add.as_str()) {
                let stem = format!("{}{rest}", a.strip);
                if !matches(&a.find, &stem, false) {
                    continue;
                }
                if self.has(&stem, &a.flag) && !self.only_in_compound(&stem) {
                    push(stem.clone(), &mut out);
                }
                if a.cross {
                    // (the prefix and suffix both: the base has both flags)
                    let mut bases: Vec<String> = vec![];
                    let mut collect = |s: String, _: &mut Vec<String>| bases.push(s);
                    let mut scratch = vec![];
                    try_suffix(self, &stem, true, &mut scratch, &mut collect);
                    for b in bases {
                        if self.has(&b, &a.flag) {
                            push(b, &mut out);
                        }
                    }
                }
            }
        }
        out
    }

    /// Whether `word` (a dictionary word or one of its forms) may be part of a compound.
    fn compoundable(&self, w: &str) -> Vec<String> {
        let Some(cf) = &self.compound else { return vec![] };
        self.normal_forms(w)
            .into_iter()
            .chain(self.words.contains_key(w).then(|| w.to_string()))
            .filter(|b| self.has(b, cf))
            .collect()
    }

    /// The word taken apart into compoundable pieces: each piece's normal form, pieces joined by `,`-less lists.
    pub fn compound_forms(&self, word: &str) -> Vec<Vec<String>> {
        let w: Vec<char> = word.to_lowercase().chars().collect();
        let mut out = vec![];
        self.split(&w, 0, &mut vec![], &mut out);
        out
    }

    fn split(&self, w: &[char], from: usize, acc: &mut Vec<String>, out: &mut Vec<Vec<String>>) {
        if from == w.len() {
            if acc.len() > 1 {
                out.push(acc.clone());
            }
            return;
        }
        for end in (from + 1..=w.len()).rev() {
            let piece: String = w[from..end].iter().collect();
            let forms = self.compoundable(&piece);
            if forms.is_empty() {
                continue;
            }
            for f in forms.into_iter().take(1) {
                acc.push(f);
                self.split(w, end, acc, out);
                acc.pop();
            }
        }
    }
}

/// The dictionary's result for a token: normal forms (None: not a word of it).
pub fn lexize(d: &Ispell, token: &str) -> Option<Vec<String>> {
    let forms = d.normal_forms(token);
    if !forms.is_empty() {
        return Some(forms);
    }
    let compounds = d.compound_forms(token);
    if let Some(first) = compounds.into_iter().next() {
        return Some(first);
    }
    None
}

#[allow(dead_code)]
fn unused(_: BTreeMap<String, String>) {}
