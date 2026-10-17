//! Postgres's full-text search: `tsvector` (sorted lexemes with positions), `tsquery` (lexemes
//! joined by & | ! and <N>), the `english` and `simple` configurations (Postgres's default parser,
//! english stop words and Snowball english stemmer), `@@` and `ts_rank`. Values travel as their
//! Postgres text forms, parsed again where needed. Checked against Postgres in tests/pgdiff.rs.
//! ponytail: the parser knows words, hyphenated words, numbers, emails, hosts and URLs; Postgres's
//! file paths, versions and XML tags come out as plain words.

use std::collections::BTreeMap;

/// Positions carry a weight in their top two bits: 3 = A, 2 = B, 1 = C, 0 = D (the default).
const POS_MAX: u16 = (1 << 14) - 1;
const POS_PER_LEXEME: usize = 256;
/// The widest `<N>` a tsquery takes (Postgres's MAXENTRYPOS).
const MAX_DISTANCE: u64 = 1 << 14;

/// The SQLSTATE of a tsquery's parse error: a phrase distance out of range is 22023, the rest syntax (42601).
pub fn query_error_code(message: &str) -> &'static str {
    if message.starts_with("distance in phrase operator") { "22023" } else { "42601" }
}
/// How deep a tsquery's operators may nest: matching, printing and dropping one recurse. A long
/// `&` or `|` chain stays shallow (see `chain`); a longer phrase, or deeper nesting, is refused.
const MAX_DEPTH: usize = 1000;
/// How deep `!` and parentheses may nest while parsing: each level is a recursive call, and no
/// real query nests more than a handful (1000 overflowed a 2 MB stack in a debug build).
const MAX_NEST: usize = 256;

/// The most words a tsvector or tsquery is made from here, and the longest text: each word
/// takes a hundred bytes or so while it's parsed, and each character 4.
const MAX_WORDS: usize = 1 << 22;
const MAX_TEXT: usize = 1 << 28;

/// An error if `s` is too big to make a tsvector or tsquery from.
pub fn size_ok(s: &str) -> Result<(), String> {
    let words = s.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).count();
    if s.len() > MAX_TEXT || words > MAX_WORDS {
        return Err(format!("text search input is too large: more than {MAX_WORDS} words or {MAX_TEXT} bytes"));
    }
    Ok(())
}

fn too_deep() -> String {
    format!("tsquery is too complex: its operators nest more than {MAX_DEPTH} deep")
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TsVector(pub BTreeMap<String, Vec<u16>>); // lexeme -> positions (empty: none recorded)

#[derive(Clone, Debug, PartialEq)]
pub enum Query {
    Val { lex: String, prefix: bool, weights: u8 }, // weights: bit 3 = A ... bit 0 = D; 0 = any
    Not(Box<Query>),
    And(Box<Query>, Box<Query>),
    Or(Box<Query>, Box<Query>),
    Phrase(Box<Query>, Box<Query>, u16),
}

#[derive(Clone, Copy, PartialEq)]
pub enum Config {
    English,
    Simple,
    /// one made with CREATE TEXT SEARCH CONFIGURATION: its place in `CONFIGS`
    User(u16),
}

pub fn config(name: &str) -> Result<Config, String> {
    let n = name.trim().to_ascii_lowercase();
    let n = n.trim_start_matches("pg_catalog.").trim_start_matches("public.");
    if let Some(i) = CONFIGS.lock().unwrap().iter().position(|c| c.name == n) {
        return Ok(Config::User(i as u16));
    }
    match n {
        "english" => Ok(Config::English),
        "simple" => Ok(Config::Simple),
        _ => Err(format!("text search configuration \"{name}\" does not exist")),
    }
}

// ---- dictionaries and configurations made with CREATE TEXT SEARCH ----

/// What a dictionary does with a token: not recognized (the next dictionary is asked), a stop word (dropped), or
/// its lexemes.
#[derive(Clone, Debug, PartialEq)]
pub enum Lexized {
    Unknown,
    Stop,
    Lexemes(Vec<String>),
}

#[derive(Clone, Debug)]
pub enum Dict {
    Simple {
        stop: Vec<String>,
        accept: bool,
    },
    Snowball {
        stop: Vec<String>,
    },
    Synonym {
        map: BTreeMap<String, String>,
        case_sensitive: bool,
    },
    Ispell {
        d: Box<super::ispell::Ispell>,
        stop: Vec<String>,
    },
    /// phrases (as the subdictionary normalizes them) and what each is replaced by
    Thesaurus {
        sub: String,
        rules: Vec<(Vec<String>, Vec<String>)>,
    },
}

#[derive(Clone, Debug)]
pub struct UserConfig {
    pub name: String,
    /// token class (`asciiword`, `int`, ...) and the dictionaries asked for it, in order
    pub maps: Vec<(String, Vec<String>)>,
}

static DICTS: std::sync::Mutex<Vec<(String, Dict)>> = std::sync::Mutex::new(vec![]);
static CONFIGS: std::sync::Mutex<Vec<UserConfig>> = std::sync::Mutex::new(vec![]);

pub fn register_dict(name: &str, d: Dict) {
    let mut all = DICTS.lock().unwrap();
    match all.iter_mut().find(|(n, _)| n == name) {
        Some(x) => x.1 = d,
        None => all.push((name.to_string(), d)),
    }
}

pub fn register_config(c: UserConfig) {
    let mut all = CONFIGS.lock().unwrap();
    match all.iter_mut().find(|x| x.name == c.name) {
        Some(x) => *x = c,
        None => all.push(c),
    }
}

pub fn forget_dict(name: &str) {
    DICTS.lock().unwrap().retain(|(n, _)| n != name);
}

pub fn forget_config(name: &str) {
    CONFIGS.lock().unwrap().retain(|c| c.name != name);
}

/// The token classes of the default parser, as ts_token_type lists them: alias and description.
pub const TOKEN_TYPES: [(&str, &str); 23] = [
    ("asciiword", "Word, all ASCII"),
    ("word", "Word, all letters"),
    ("numword", "Word, letters and digits"),
    ("email", "Email address"),
    ("url", "URL"),
    ("host", "Host"),
    ("sfloat", "Scientific notation"),
    ("version", "Version number"),
    ("hword_numpart", "Hyphenated word part, letters and digits"),
    ("hword_part", "Hyphenated word part, all letters"),
    ("hword_asciipart", "Hyphenated word part, all ASCII"),
    ("blank", "Space symbols"),
    ("tag", "XML tag"),
    ("protocol", "Protocol head"),
    ("numhword", "Hyphenated word, letters and digits"),
    ("asciihword", "Hyphenated word, all ASCII"),
    ("hword", "Hyphenated word, all letters"),
    ("url_path", "URL path"),
    ("file", "File or path name"),
    ("float", "Decimal notation"),
    ("int", "Signed integer"),
    ("uint", "Unsigned integer"),
    ("entity", "XML entity"),
];

/// The default parser's class of a token: its type's alias.
fn class_of(_t: &str, kind: Kind) -> &'static str {
    kind.class()
}

/// Where dictionary files are looked for: CHRONOS_TSEARCH_DATA (a path list), then the working directory. A name
/// is lowercase letters, digits and `_` only, as Postgres's get_tsearch_config_filename takes it: never a path.
pub fn data_file(name: &str, ext: &str) -> Result<String, String> {
    if name.is_empty() || !name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_') {
        return Err(format!("invalid text search configuration file name \"{name}\""));
    }
    let dirs = std::env::var("CHRONOS_TSEARCH_DATA").unwrap_or_default();
    for d in dirs.split(':').chain(std::iter::once(".")) {
        let p = format!("{d}/{name}.{ext}");
        if let Ok(text) = std::fs::read_to_string(&p) {
            return Ok(text);
        }
    }
    Err(format!("could not open stop-word file \"{name}.{ext}\": No such file or directory"))
}

/// Stop words from a `.stop` file, lowercased.
pub fn stop_file(name: &str) -> Result<Vec<String>, String> {
    Ok(data_file(name, "stop")?.lines().map(|l| l.trim().to_lowercase()).filter(|l| !l.is_empty()).collect())
}

/// A synonym dictionary's `.syn` file: `word synonym` a line.
pub fn synonym_file(name: &str, case_sensitive: bool) -> Result<BTreeMap<String, String>, String> {
    let text = data_file(name, "syn").map_err(|e| e.replace("stop-word", "synonym"))?;
    let mut map = BTreeMap::new();
    for l in text.lines() {
        let mut w = l.split_whitespace();
        if let (Some(a), Some(b)) = (w.next(), w.next()) {
            let a = if case_sensitive { a.to_string() } else { a.to_lowercase() };
            map.entry(a).or_insert_with(|| if case_sensitive { b.to_string() } else { b.to_lowercase() });
        }
    }
    Ok(map)
}

/// A thesaurus dictionary's file: `sample words : substitute words` a line, the words taken to lexemes by the
/// subdictionary (a stop word in a sample is left out).
#[allow(clippy::type_complexity)]
pub fn thesaurus_file(name: &str, sub: &str) -> Result<Vec<(Vec<String>, Vec<String>)>, String> {
    let text = data_file(name, "ths").map_err(|e| e.replace("stop-word", "thesaurus"))?;
    let words = |s: &str, n: usize, what: &str| -> Result<Vec<String>, String> {
        let mut out = vec![];
        for w in s.split_whitespace() {
            match lexize(sub, w)? {
                Lexized::Lexemes(l) => out.extend(l.into_iter().next()),
                Lexized::Stop => {}
                Lexized::Unknown => {
                    return Err(format!("thesaurus {what} word \"{w}\" isn't recognized by subdictionary (rule {n})"));
                }
            }
        }
        Ok(out)
    };
    let mut rules = vec![];
    for (n, l) in text.lines().enumerate() {
        let l = l.split('#').next().unwrap_or("").trim();
        if l.is_empty() {
            continue;
        }
        let Some((a, b)) = l.split_once(':') else {
            return Err(format!("unexpected delimiter (rule {})", n + 1));
        };
        let (sample, subst) = (words(a, n + 1, "sample")?, words(b, n + 1, "substitute")?);
        if sample.is_empty() {
            return Err(format!("thesaurus sample is empty (rule {})", n + 1));
        }
        rules.push((sample, subst));
    }
    Ok(rules)
}

/// The dictionary `name` as registered.
fn dict_named(name: &str) -> Option<Dict> {
    DICTS.lock().unwrap().iter().find(|(n, _)| n == name).map(|(_, d)| d.clone())
}

/// A thesaurus among the dictionaries asked for the token at `i` that has a phrase starting there: how many tokens
/// the phrase takes and what replaces it (the longest phrase wins).
fn thesaurus_at(cfg: Config, toks: &[(String, Kind)], i: usize) -> Option<(usize, Vec<String>)> {
    let (t, k) = &toks[i];
    for name in dict_chain(cfg, class_of(t, *k)) {
        let Some(Dict::Thesaurus { sub, rules }) = dict_named(&name) else { continue };
        // the tokens from here on as the subdictionary has them (None: a stop word)
        let norm: Vec<Option<String>> = toks[i..]
            .iter()
            .take(rules.iter().map(|r| r.0.len()).max().unwrap_or(0) + 8)
            .map(|(w, _)| match lexize(&sub, w) {
                Ok(Lexized::Lexemes(l)) => l.into_iter().next(),
                _ => None,
            })
            .collect();
        // (a phrase starts at a word, not at a stop word before it)
        if norm.first().is_none_or(|x| x.is_none()) {
            continue;
        }
        let mut best: Option<(usize, &Vec<String>, usize)> = None;
        for (sample, subst) in &rules {
            let (mut at, mut ok) = (0usize, true);
            for want in sample {
                while norm.get(at).is_some_and(|x| x.is_none()) {
                    at += 1;
                }
                if norm.get(at).and_then(|x| x.as_ref()) != Some(want) {
                    ok = false;
                    break;
                }
                at += 1;
            }
            if ok && best.is_none_or(|(_, _, n)| sample.len() > n) {
                best = Some((at, subst, sample.len()));
            }
        }
        if let Some((used, subst, _)) = best {
            return Some((used, subst.clone()));
        }
    }
    None
}

/// What the dictionary `name` makes of a token.
pub fn lexize(name: &str, token: &str) -> Result<Lexized, String> {
    let low = token.to_lowercase();
    match name {
        "simple" => return Ok(Lexized::Lexemes(vec![low])),
        "english_stem" => {
            return Ok(if STOP.contains(&low.as_str()) { Lexized::Stop } else { Lexized::Lexemes(vec![stem(&low)]) });
        }
        _ => {}
    }
    let all = DICTS.lock().unwrap();
    let Some((_, d)) = all.iter().find(|(n, _)| n == name) else {
        return Err(format!("text search dictionary \"{name}\" does not exist"));
    };
    Ok(match d {
        Dict::Simple { stop, accept } => {
            if stop.contains(&low) {
                Lexized::Stop
            } else if *accept {
                Lexized::Lexemes(vec![low])
            } else {
                Lexized::Unknown
            }
        }
        Dict::Snowball { stop } => {
            if stop.contains(&low) {
                Lexized::Stop
            } else {
                Lexized::Lexemes(vec![stem(&low)])
            }
        }
        Dict::Ispell { d, stop } => {
            if stop.contains(&low) {
                Lexized::Stop
            } else {
                super::ispell::lexize(d, &low).map_or(Lexized::Unknown, Lexized::Lexemes)
            }
        }
        // (a phrase dictionary: one word alone is left to the next dictionary)
        Dict::Thesaurus { .. } => Lexized::Unknown,
        Dict::Synonym { map, case_sensitive } => {
            let k = if *case_sensitive { token.to_string() } else { low };
            map.get(&k).map_or(Lexized::Unknown, |s| Lexized::Lexemes(vec![s.clone()]))
        }
    })
}

/// The dictionaries a configuration asks for a token class.
pub fn mapped(cfg: Config, class: &str) -> Vec<String> {
    dict_chain(cfg, class)
}

/// A configuration's dictionaries for a token class.
fn dict_chain(cfg: Config, class: &str) -> Vec<String> {
    const WORDS: [&str; 6] = ["asciiword", "asciihword", "hword_asciipart", "word", "hword", "hword_part"];
    const OTHERS: [&str; 13] = [
        "email",
        "url",
        "host",
        "sfloat",
        "version",
        "hword_numpart",
        "numhword",
        "url_path",
        "file",
        "float",
        "int",
        "uint",
        "numword",
    ];
    match cfg {
        Config::English if WORDS.contains(&class) => vec!["english_stem".into()],
        Config::English | Config::Simple if WORDS.contains(&class) || OTHERS.contains(&class) => vec!["simple".into()],
        Config::User(i) => CONFIGS
            .lock()
            .unwrap()
            .get(i as usize)
            .and_then(|c| c.maps.iter().find(|(k, _)| k == class).map(|(_, d)| d.clone()))
            .unwrap_or_default(),
        _ => vec![],
    }
}

// ---- the parser: text into tokens, as Postgres's default parser splits it ----

/// A token's type as the default parser has it: 1..=23, an index into `TOKEN_TYPES` (asciiword is 1, blank 12).
#[derive(Clone, Copy, PartialEq, Debug)]
struct Kind(u8);

impl Kind {
    /// The types the english configuration stems (its words, whole or hyphenated parts); the rest it keeps as they are.
    fn is_word(self) -> bool {
        matches!(self.0, 1 | 2 | 10 | 11 | 16 | 17)
    }

    fn class(self) -> &'static str {
        TOKEN_TYPES[usize::from(self.0).clamp(1, TOKEN_TYPES.len()) - 1].0
    }
}

/// The types no configuration maps to a dictionary (blank, tag, protocol, entity): tokens that take no position and
/// give no lexeme.
fn unmapped(kind: Kind) -> bool {
    matches!(kind.0, 12..=14 | 23)
}

/// The tokens of `s`, in order, with their kinds, as Postgres's default parser cuts them (see `tsparser`): a
/// hyphenated word comes whole, then its parts. Blanks, tags, protocol heads and entities (which no configuration
/// has a dictionary for) are left out.
fn tokens(s: &str) -> Vec<(String, Kind)> {
    super::tsparser::parse(s).into_iter().map(|(t, w)| (w, Kind(t))).filter(|(_, k)| !unmapped(*k)).collect()
}

const STOP: [&str; 127] = [
    "i",
    "me",
    "my",
    "myself",
    "we",
    "our",
    "ours",
    "ourselves",
    "you",
    "your",
    "yours",
    "yourself",
    "yourselves",
    "he",
    "him",
    "his",
    "himself",
    "she",
    "her",
    "hers",
    "herself",
    "it",
    "its",
    "itself",
    "they",
    "them",
    "their",
    "theirs",
    "themselves",
    "what",
    "which",
    "who",
    "whom",
    "this",
    "that",
    "these",
    "those",
    "am",
    "is",
    "are",
    "was",
    "were",
    "be",
    "been",
    "being",
    "have",
    "has",
    "had",
    "having",
    "do",
    "does",
    "did",
    "doing",
    "a",
    "an",
    "the",
    "and",
    "but",
    "if",
    "or",
    "because",
    "as",
    "until",
    "while",
    "of",
    "at",
    "by",
    "for",
    "with",
    "about",
    "against",
    "between",
    "into",
    "through",
    "during",
    "before",
    "after",
    "above",
    "below",
    "to",
    "from",
    "up",
    "down",
    "in",
    "out",
    "on",
    "off",
    "over",
    "under",
    "again",
    "further",
    "then",
    "once",
    "here",
    "there",
    "when",
    "where",
    "why",
    "how",
    "all",
    "any",
    "both",
    "each",
    "few",
    "more",
    "most",
    "other",
    "some",
    "such",
    "no",
    "nor",
    "not",
    "only",
    "own",
    "same",
    "so",
    "than",
    "too",
    "very",
    "s",
    "t",
    "can",
    "will",
    "just",
    "don",
    "should",
    "now",
];

/// A token's lexeme under `cfg`, or None for a stop word.
fn lexeme(cfg: Config, t: &str, kind: Kind) -> Option<String> {
    if let Config::User(_) = cfg {
        for d in dict_chain(cfg, class_of(t, kind)) {
            match lexize(&d, t) {
                Ok(Lexized::Lexemes(l)) => return l.into_iter().next(),
                Ok(Lexized::Stop) => return None,
                _ => {}
            }
        }
        return None;
    }
    let low = t.to_lowercase();
    match (cfg, kind) {
        (Config::English, k) if k.is_word() && STOP.contains(&low.as_str()) => None,
        (Config::English, k) if k.is_word() => Some(stem(&low)),
        _ => Some(low),
    }
}

/// The tokens `cfg` has a dictionary for (a token of a class it maps to none takes no position: LexizeExec).
fn mapped_tokens(cfg: Config, s: &str) -> Vec<(String, Kind)> {
    let mut toks = tokens(s);
    if let Config::User(_) = cfg {
        toks.retain(|(_, k)| !dict_chain(cfg, k.class()).is_empty());
    }
    toks
}

/// Each token's lexeme (None for a stop word) in order: position i + 1 is the i-th.
fn lexemes(cfg: Config, s: &str) -> Vec<Option<String>> {
    mapped_tokens(cfg, s).into_iter().map(|(t, k)| lexeme(cfg, &t, k)).collect()
}

pub fn to_tsvector(cfg: Config, s: &str) -> TsVector {
    if let Config::User(_) = cfg
        && let Some(v) = with_thesaurus(cfg, s)
    {
        return v;
    }
    let mut v = TsVector::default();
    for (i, l) in lexemes(cfg, s).into_iter().enumerate() {
        let Some(l) = l else { continue };
        let ps = v.0.entry(l).or_default();
        let p = (i + 1).min(POS_MAX as usize) as u16;
        if ps.len() < POS_PER_LEXEME && ps.last() != Some(&p) {
            ps.push(p);
        }
    }
    v
}

/// What `tsvector_update_trigger` does with each of its texts (`parsetext` run over them in turn): `s`'s words are
/// added to `v` at the positions after the ones before it, no gap between texts, stop words taking their places as
/// they do in a document; `pos` is how far the texts so far went.
pub fn extend(cfg: Config, s: &str, v: &mut TsVector, pos: &mut usize) {
    for (lex, ps) in to_tsvector(cfg, s).0 {
        let mine = v.0.entry(lex).or_default();
        for p in ps {
            let at = ((p & POS_MAX) as usize + *pos).min(POS_MAX as usize) as u16;
            if mine.len() < POS_PER_LEXEME && !mine.iter().any(|q| q & POS_MAX == at) {
                mine.push(at | (p & !POS_MAX));
            }
        }
        mine.sort_by_key(|p| p & POS_MAX);
    }
    *pos += lexemes(cfg, s).len();
}

/// A document's vector where a thesaurus replaces phrases (None: none does). A phrase's replacement takes the
/// positions from where the phrase starts, one a word; the words after it go on from the phrase's end.
#[inline(never)]
fn with_thesaurus(cfg: Config, s: &str) -> Option<TsVector> {
    let toks = mapped_tokens(cfg, s);
    let mut found = false;
    let (mut v, mut i, mut pos) = (TsVector::default(), 0, 0usize);
    let put = |v: &mut TsVector, l: String, p: usize| {
        let ps = v.0.entry(l).or_default();
        let p = p.min(POS_MAX as usize) as u16;
        if ps.len() < POS_PER_LEXEME && ps.last() != Some(&p) {
            ps.push(p);
        }
    };
    while i < toks.len() {
        if let Some((used, subst)) = thesaurus_at(cfg, &toks, i) {
            found = true;
            for (k, l) in subst.into_iter().enumerate() {
                put(&mut v, l, pos + k + 1);
            }
            pos += used;
            i += used;
            continue;
        }
        pos += 1;
        if let Some(l) = lexeme(cfg, &toks[i].0, toks[i].1) {
            put(&mut v, l, pos);
        }
        i += 1;
    }
    found.then_some(v)
}

// ---- the Snowball english stemmer (Porter2), as Postgres's english_stem runs it ----

pub fn stem(word: &str) -> String {
    let special = [
        ("skis", "ski"),
        ("skies", "sky"),
        ("dying", "die"),
        ("lying", "lie"),
        ("tying", "tie"),
        ("idly", "idl"),
        ("gently", "gentl"),
        ("ugly", "ugli"),
        ("early", "earli"),
        ("only", "onli"),
        ("singly", "singl"),
        ("sky", "sky"),
        ("news", "news"),
        ("howe", "howe"),
        ("atlas", "atlas"),
        ("cosmos", "cosmos"),
        ("bias", "bias"),
        ("andes", "andes"),
    ];
    if let Some((_, s)) = special.iter().find(|(w, _)| *w == word) {
        return s.to_string();
    }
    let mut w: Vec<char> = word.chars().collect();
    if w.len() < 3 {
        return word.into();
    }
    // prelude: an initial apostrophe goes; a y that starts the word or follows a vowel is Y
    if w[0] == '\'' {
        w.remove(0);
    }
    let vowel = |c: char| matches!(c, 'a' | 'e' | 'i' | 'o' | 'u' | 'y');
    let mut found_y = false;
    for i in 0..w.len() {
        if w[i] == 'y' && (i == 0 || vowel(w[i - 1])) {
            w[i] = 'Y';
            found_y = true;
        }
    }
    // regions: R1 after the first non-vowel that follows a vowel, R2 the same within R1
    let after = |w: &[char], from: usize| -> usize {
        let mut i = from;
        while i < w.len() && !vowel(w[i]) {
            i += 1;
        }
        while i < w.len() && vowel(w[i]) {
            i += 1;
        }
        (i + 1).min(w.len())
    };
    let s: String = w.iter().collect();
    let p1 = match ["gener", "commun", "arsen"].iter().find(|p| s.starts_with(*p)) {
        Some(p) => p.len(),
        None => after(&w, 0),
    };
    let p2 = after(&w, p1);
    let mut st = Stem { w, p1, p2 };
    st.step_1a();
    let s: String = st.w.iter().collect();
    if !["inning", "outing", "canning", "herring", "earring", "proceed", "exceed", "succeed"].contains(&s.as_str()) {
        st.step_1b();
        st.step_1c();
        st.step_2();
        st.step_3();
        st.step_4();
        st.step_5();
    }
    let out: String = st.w.iter().collect();
    if found_y { out.replace('Y', "y") } else { out }
}

struct Stem {
    w: Vec<char>,
    p1: usize,
    p2: usize,
}

fn is_v(c: char) -> bool {
    matches!(c, 'a' | 'e' | 'i' | 'o' | 'u' | 'y')
}

impl Stem {
    fn ends(&self, s: &str) -> bool {
        let s: Vec<char> = s.chars().collect();
        self.w.len() >= s.len() && self.w[self.w.len() - s.len()..] == s[..]
    }

    /// The longest of `suffixes` the word ends with.
    fn longest<'a>(&self, suffixes: &[&'a str]) -> Option<&'a str> {
        suffixes.iter().filter(|s| self.ends(s)).max_by_key(|s| s.chars().count()).copied()
    }

    fn start(&self, suffix: &str) -> usize {
        self.w.len() - suffix.chars().count()
    }

    fn replace(&mut self, suffix: &str, with: &str) {
        let at = self.start(suffix);
        self.w.truncate(at);
        self.w.extend(with.chars());
    }

    /// Whether the word, cut at `end`, ends in a short syllable.
    fn short_v(&self, end: usize) -> bool {
        let w = &self.w;
        (end >= 3
            && !is_v(w[end - 1])
            && !matches!(w[end - 1], 'w' | 'x' | 'Y')
            && is_v(w[end - 2])
            && !is_v(w[end - 3]))
            || (end == 2 && !is_v(w[1]) && is_v(w[0]))
    }

    fn step_1a(&mut self) {
        if let Some(s) = self.longest(&["'", "'s", "'s'"]) {
            self.replace(s, "");
        }
        match self.longest(&["sses", "ied", "ies", "s", "us", "ss"]) {
            Some("sses") => self.replace("sses", "ss"),
            Some(s @ ("ied" | "ies")) => {
                let with = if self.start(s) > 1 { "i" } else { "ie" };
                self.replace(s, with);
            }
            Some("s") => {
                let at = self.start("s");
                if at >= 1 && self.w[..at - 1].iter().any(|&c| is_v(c)) {
                    self.replace("s", "");
                }
            }
            _ => {}
        }
    }

    fn step_1b(&mut self) {
        match self.longest(&["eed", "eedly", "ed", "edly", "ing", "ingly"]) {
            Some(s @ ("eed" | "eedly")) if self.start(s) >= self.p1 => self.replace(s, "ee"),
            Some("eed" | "eedly") | None => {} // not in R1: nothing, not a shorter suffix
            Some(s) => {
                let at = self.start(s);
                if !self.w[..at].iter().any(|&c| is_v(c)) {
                    return;
                }
                self.replace(s, "");
                if self.longest(&["at", "bl", "iz"]).is_some() {
                    self.w.push('e');
                } else if self.longest(&["bb", "dd", "ff", "gg", "mm", "nn", "pp", "rr", "tt"]).is_some() {
                    self.w.pop();
                } else if self.w.len() == self.p1 && self.short_v(self.w.len()) {
                    self.w.push('e');
                }
            }
        }
    }

    fn step_1c(&mut self) {
        let n = self.w.len();
        if n >= 3 && matches!(self.w[n - 1], 'y' | 'Y') && !is_v(self.w[n - 2]) {
            self.w[n - 1] = 'i';
        }
    }

    fn step_2(&mut self) {
        let rules = [
            ("tional", "tion"),
            ("enci", "ence"),
            ("anci", "ance"),
            ("abli", "able"),
            ("entli", "ent"),
            ("izer", "ize"),
            ("ization", "ize"),
            ("ational", "ate"),
            ("ation", "ate"),
            ("ator", "ate"),
            ("alism", "al"),
            ("aliti", "al"),
            ("alli", "al"),
            ("fulness", "ful"),
            ("ousli", "ous"),
            ("ousness", "ous"),
            ("iveness", "ive"),
            ("iviti", "ive"),
            ("biliti", "ble"),
            ("bli", "ble"),
            ("ogi", "og"),
            ("fulli", "ful"),
            ("lessli", "less"),
            ("li", ""),
        ];
        let names: Vec<&str> = rules.iter().map(|r| r.0).collect();
        let Some(s) = self.longest(&names) else { return };
        let at = self.start(s);
        if at < self.p1 {
            return;
        }
        let before = if at > 0 { Some(self.w[at - 1]) } else { None };
        let with = rules.iter().find(|r| r.0 == s).unwrap().1;
        match s {
            "ogi" if before != Some('l') => {}
            "li" if !before.is_some_and(|c| "cdeghkmnrt".contains(c)) => {}
            _ => self.replace(s, with),
        }
    }

    fn step_3(&mut self) {
        let rules = [
            ("tional", "tion"),
            ("ational", "ate"),
            ("alize", "al"),
            ("icate", "ic"),
            ("iciti", "ic"),
            ("ical", "ic"),
            ("ful", ""),
            ("ness", ""),
            ("ative", ""),
        ];
        let names: Vec<&str> = rules.iter().map(|r| r.0).collect();
        let Some(s) = self.longest(&names) else { return };
        let at = self.start(s);
        if at < self.p1 || (s == "ative" && at < self.p2) {
            return;
        }
        self.replace(s, rules.iter().find(|r| r.0 == s).unwrap().1);
    }

    fn step_4(&mut self) {
        let names = [
            "al", "ance", "ence", "er", "ic", "able", "ible", "ant", "ement", "ment", "ent", "ism", "ate", "iti",
            "ous", "ive", "ize", "ion",
        ];
        let Some(s) = self.longest(&names) else { return };
        let at = self.start(s);
        if at < self.p2 {
            return;
        }
        if s == "ion" && !(at > 0 && matches!(self.w[at - 1], 's' | 't')) {
            return;
        }
        self.replace(s, "");
    }

    fn step_5(&mut self) {
        let n = self.w.len();
        if n == 0 {
            return;
        }
        let at = n - 1;
        match self.w[at] {
            'e' if at >= self.p2 || (at >= self.p1 && !self.short_v(at)) => {
                self.w.pop();
            }
            'l' if at >= self.p2 && at > 0 && self.w[at - 1] == 'l' => {
                self.w.pop();
            }
            _ => {}
        }
    }
}

// ---- text forms ----

fn quote(out: &mut String, s: &str) {
    out.push('\'');
    for c in s.chars() {
        if c == '\'' || c == '\\' {
            out.push(c);
        }
        out.push(c);
    }
    out.push('\'');
}

const WEIGHTS: [char; 4] = ['D', 'C', 'B', 'A'];

impl std::fmt::Display for TsVector {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let mut out = String::new();
        for (i, (l, ps)) in self.0.iter().enumerate() {
            if i > 0 {
                out.push(' ');
            }
            quote(&mut out, l);
            for (j, p) in ps.iter().enumerate() {
                out.push(if j == 0 { ':' } else { ',' });
                out.push_str(&(p & POS_MAX).to_string());
                if p >> 14 != 0 {
                    out.push(WEIGHTS[(p >> 14) as usize]);
                }
            }
        }
        f.write_str(&out)
    }
}

/// Reads a lexeme at `i`: quoted ('it''s', with '' or \ escapes) or bare (up to space or :).
fn read_lexeme(c: &[char], i: &mut usize, stop: &[char]) -> Result<String, String> {
    let mut s = String::new();
    if c.get(*i) == Some(&'\'') {
        *i += 1;
        loop {
            match c.get(*i) {
                None => return Err("unterminated quoted string".into()),
                Some('\'') if c.get(*i + 1) == Some(&'\'') => {
                    s.push('\'');
                    *i += 2;
                }
                Some('\'') => {
                    *i += 1;
                    break;
                }
                Some('\\') if *i + 1 < c.len() => {
                    s.push(c[*i + 1]);
                    *i += 2;
                }
                Some(&x) => {
                    s.push(x);
                    *i += 1;
                }
            }
        }
    } else {
        while let Some(&x) = c.get(*i) {
            if x.is_whitespace() || stop.contains(&x) {
                break;
            }
            if x == '\\' && *i + 1 < c.len() {
                *i += 1;
            }
            s.push(c[*i]);
            *i += 1;
        }
    }
    Ok(s)
}

impl TsVector {
    /// A tsvector's text form, as `'...'::tsvector` reads it.
    pub fn parse(s: &str) -> Result<TsVector, String> {
        size_ok(s)?;
        let c: Vec<char> = s.chars().collect();
        let (mut v, mut i) = (TsVector::default(), 0);
        let bad = || format!("syntax error in tsvector: \"{s}\"");
        loop {
            while i < c.len() && c[i].is_whitespace() {
                i += 1;
            }
            if i == c.len() {
                break;
            }
            let l = read_lexeme(&c, &mut i, &[':'])?;
            if l.is_empty() {
                return Err(bad());
            }
            let ps = v.0.entry(l).or_default();
            if c.get(i) == Some(&':') {
                loop {
                    i += 1;
                    let from = i;
                    while i < c.len() && c[i].is_ascii_digit() {
                        i += 1;
                    }
                    let p: u32 = c[from..i].iter().collect::<String>().parse().map_err(|_| bad())?;
                    if p == 0 {
                        return Err(format!("wrong position info in tsvector: \"{s}\""));
                    }
                    let mut p = p.min(POS_MAX as u32) as u16;
                    // (as gettoken_tsvector reads what follows: weights, A or * 3 down to D 0, a second only after D;
                    // digits ignored; then a comma, a space or the end)
                    let mut w: u16 = 0;
                    while let Some(&x) = c.get(i) {
                        let weight = match x.to_ascii_lowercase() {
                            'a' | '*' => 3,
                            'b' => 2,
                            'c' => 1,
                            'd' => 0,
                            x if x.is_ascii_digit() => {
                                i += 1;
                                continue;
                            }
                            _ => break,
                        };
                        if w != 0 {
                            return Err(bad());
                        }
                        w = weight;
                        i += 1;
                    }
                    p |= w << 14;
                    ps.push(p);
                    match c.get(i) {
                        Some(',') => {}
                        Some(x) if !x.is_whitespace() => return Err(bad()),
                        _ => break,
                    }
                }
            }
        }
        // positions sorted, one each (the highest weight wins), at most 256
        for ps in v.0.values_mut() {
            ps.sort_by_key(|p| (p & POS_MAX, std::cmp::Reverse(p >> 14)));
            ps.dedup_by_key(|p| *p & POS_MAX);
            ps.truncate(POS_PER_LEXEME);
        }
        Ok(v)
    }

    /// `a || b`: b's positions move past a's last one; shared lexemes keep both.
    pub fn concat(mut self, b: &TsVector) -> TsVector {
        let shift = self.0.values().flatten().map(|p| p & POS_MAX).max().unwrap_or(0);
        for (l, ps) in &b.0 {
            let mine = self.0.entry(l.clone()).or_default();
            let had = !mine.is_empty();
            mine.extend(ps.iter().map(|p| (p & !POS_MAX) | ((p & POS_MAX) + shift).min(POS_MAX)));
            if had || ps.is_empty() {
                mine.sort_by_key(|p| p & POS_MAX);
                mine.dedup_by_key(|p| *p & POS_MAX);
            }
            mine.truncate(POS_PER_LEXEME);
        }
        self
    }

    pub fn setweight(&mut self, w: u8) {
        for p in self.0.values_mut().flatten() {
            *p = (*p & POS_MAX) | ((w as u16) << 14);
        }
    }

    pub fn strip(&mut self) {
        self.0.values_mut().for_each(Vec::clear);
    }
}

impl Query {
    /// How deep its operators nest (a lone operand is 1), counted without recursing.
    fn depth(&self) -> usize {
        let (mut most, mut todo) = (0, vec![(self, 1)]);
        while let Some((q, d)) = todo.pop() {
            most = most.max(d);
            match q {
                Query::Val { .. } => {}
                Query::Not(a) => todo.push((&**a, d + 1)),
                Query::And(a, b) | Query::Or(a, b) | Query::Phrase(a, b, _) => {
                    todo.extend([(&**a, d + 1), (&**b, d + 1)])
                }
            }
        }
        most
    }

    fn priority(&self) -> i32 {
        match self {
            Query::Val { .. } => 5,
            Query::Not(_) => 4,
            Query::Phrase(..) => 3,
            Query::And(..) => 2,
            Query::Or(..) => 1,
        }
    }

    /// Postgres's text form (`infix` in tsquery.c): parentheses only where priority needs them.
    fn infix(&self, out: &mut String, parent: i32, right_phrase: bool) {
        let p = self.priority();
        match self {
            Query::Val { lex, prefix, weights } => {
                quote(out, lex);
                if *prefix || *weights != 0 {
                    out.push(':');
                    if *prefix {
                        out.push('*');
                    }
                    for (bit, c) in [(3, 'A'), (2, 'B'), (1, 'C'), (0, 'D')] {
                        if weights & (1 << bit) != 0 {
                            out.push(c);
                        }
                    }
                }
            }
            Query::Not(a) => {
                if p < parent {
                    out.push_str("( ");
                }
                out.push('!');
                a.infix(out, p, false);
                if p < parent {
                    out.push_str(" )");
                }
            }
            Query::And(a, b) | Query::Or(a, b) | Query::Phrase(a, b, _) => {
                let phrase = matches!(self, Query::Phrase(..));
                let parens = p < parent || (phrase && right_phrase);
                if parens {
                    out.push_str("( ");
                }
                a.infix(out, p, false);
                out.push_str(&match self {
                    Query::And(..) => " & ".to_string(),
                    Query::Or(..) => " | ".to_string(),
                    Query::Phrase(_, _, 1) => " <-> ".to_string(),
                    Query::Phrase(_, _, d) => format!(" <{d}> "),
                    _ => unreachable!(),
                });
                b.infix(out, p, phrase);
                if parens {
                    out.push_str(" )");
                }
            }
        }
    }

    pub fn text(q: &Option<Query>) -> String {
        let mut s = String::new();
        if let Some(q) = q {
            q.infix(&mut s, -1, false);
        }
        s
    }
}

// ---- binary forms, as Postgres's tsvectorsend and tsquerysend write them ----

impl TsVector {
    pub fn to_wire(&self) -> Vec<u8> {
        let mut b = (self.0.len() as i32).to_be_bytes().to_vec();
        for (l, ps) in &self.0 {
            b.extend(l.as_bytes());
            b.push(0);
            b.extend((ps.len() as i16).to_be_bytes());
            for p in ps {
                b.extend(p.to_be_bytes());
            }
        }
        b
    }
}

impl Query {
    /// Items in Postgres's order: an operator, its right operand, then its left.
    fn items(&self, out: &mut Vec<u8>, n: &mut i32) {
        *n += 1;
        match self {
            Query::Val { lex, prefix, weights } => {
                out.extend([1, *weights, *prefix as u8]);
                out.extend(lex.as_bytes());
                out.push(0);
            }
            Query::Not(a) => {
                out.extend([2, 1]);
                a.items(out, n);
            }
            Query::And(a, b) | Query::Or(a, b) | Query::Phrase(a, b, _) => {
                match self {
                    Query::And(..) => out.extend([2, 2]),
                    Query::Or(..) => out.extend([2, 3]),
                    Query::Phrase(_, _, d) => {
                        out.extend([2, 4]);
                        out.extend((*d as i16).to_be_bytes());
                    }
                    _ => unreachable!(),
                }
                b.items(out, n);
                a.items(out, n);
            }
        }
    }

    pub fn to_wire(q: &Option<Query>) -> Vec<u8> {
        let (mut items, mut n) = (vec![], 0);
        if let Some(q) = q {
            q.items(&mut items, &mut n);
        }
        [n.to_be_bytes().to_vec(), items].concat()
    }
}

// ---- tsquery syntax: operands, ! & | <-> <N> and parentheses ----

enum Tok {
    Val(String, bool, u8),
    Not,
    And,
    Or,
    Phrase(u16),
    Open,
    Close,
}

fn lex_query(s: &str) -> Result<Vec<Tok>, String> {
    let c: Vec<char> = s.chars().collect();
    let bad = || format!("syntax error in tsquery: \"{s}\"");
    let (mut out, mut i) = (vec![], 0);
    while i < c.len() {
        match c[i] {
            x if x.is_whitespace() => i += 1,
            '!' => {
                out.push(Tok::Not);
                i += 1;
            }
            '&' => {
                out.push(Tok::And);
                i += 1;
            }
            '|' => {
                out.push(Tok::Or);
                i += 1;
            }
            '(' => {
                out.push(Tok::Open);
                i += 1;
            }
            ')' => {
                out.push(Tok::Close);
                i += 1;
            }
            '<' => {
                let end = c[i..].iter().position(|&x| x == '>').ok_or_else(bad)? + i;
                let inner: String = c[i + 1..end].iter().collect();
                // (Postgres's strtol: digits, at most MAXENTRYPOS; more digits than a long holds is that error too)
                let digits = inner.trim();
                let d = if inner == "-" {
                    1
                } else if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
                    digits.parse::<u64>().unwrap_or(u64::MAX)
                } else {
                    return Err(bad());
                };
                if d > MAX_DISTANCE {
                    return Err(format!(
                        "distance in phrase operator must be an integer value between zero and {MAX_DISTANCE} inclusive"
                    ));
                }
                out.push(Tok::Phrase(d as u16));
                i = end + 1;
            }
            _ => {
                let l = read_lexeme(&c, &mut i, &['!', '&', '|', '(', ')', '<', ':'])?;
                let (mut prefix, mut weights) = (false, 0u8);
                if c.get(i) == Some(&':') {
                    i += 1;
                    while let Some(&x) = c.get(i) {
                        match x.to_ascii_uppercase() {
                            '*' => prefix = true,
                            'A' => weights |= 8,
                            'B' => weights |= 4,
                            'C' => weights |= 2,
                            'D' => weights |= 1,
                            _ => break,
                        }
                        i += 1;
                    }
                }
                out.push(Tok::Val(l, prefix, weights));
            }
        }
    }
    Ok(out)
}

/// A parsed operand: its lexemes as a phrase, or None when all were stop words.
type Operand<'a> = &'a dyn Fn(&str, bool, u8) -> Result<Option<Query>, String>;

/// Parses tsquery syntax; `operand` turns each operand into its query (normalized or not).
fn parse_query(s: &str, operand: Operand) -> Result<Option<Query>, String> {
    let toks = lex_query(s)?;
    let bad = || format!("syntax error in tsquery: \"{s}\"");
    if toks.is_empty() {
        return Err(bad());
    }
    // precedence climbing over Option<Query> (None: an operand that was only stop words)
    struct P<'a> {
        t: &'a [Tok],
        i: usize,
        nest: usize, // ! and ( around the token at i
    }
    fn unary(p: &mut P, operand: Operand, bad: &dyn Fn() -> String) -> Result<Removed, String> {
        if matches!(p.t.get(p.i), Some(Tok::Not | Tok::Open)) && p.nest >= MAX_NEST {
            return Err(format!("tsquery is too complex: ! and parentheses nest more than {MAX_NEST} deep"));
        }
        match p.t.get(p.i) {
            Some(Tok::Not) => {
                p.i += 1;
                p.nest += 1;
                let a = unary(p, operand, bad)?;
                p.nest -= 1;
                a.map(|q| Query::Not(Box::new(q))).shallow()
            }
            Some(Tok::Open) => {
                p.i += 1;
                p.nest += 1;
                let a = binary(p, 0, operand, bad)?;
                p.nest -= 1;
                if !matches!(p.t.get(p.i), Some(Tok::Close)) {
                    return Err(bad());
                }
                p.i += 1;
                Ok(a)
            }
            Some(Tok::Val(l, prefix, w)) => {
                p.i += 1;
                Removed::of(operand(l, *prefix, *w)?).shallow()
            }
            _ => Err(bad()),
        }
    }
    fn binary(p: &mut P, min: i32, operand: Operand, bad: &dyn Fn() -> String) -> Result<Removed, String> {
        let mut left = unary(p, operand, bad)?;
        // a run of & (or of |): its operands, joined when it ends (see `chain`); 0 = none open
        let (mut run, mut run_prio) = (vec![], 0);
        loop {
            // left-associative: the caller takes an operator no tighter than its own
            let next = match p.t.get(p.i) {
                Some(Tok::Or) => Some((1, 0u16)),
                Some(Tok::And) => Some((2, 0)),
                Some(Tok::Phrase(d)) => Some((3, *d)),
                _ => None,
            }
            .filter(|(prio, _)| *prio > min);
            // operators come no tighter than the one before, so a run ends at the first other one
            if run_prio != 0 && next.is_none_or(|(prio, _)| prio != run_prio) {
                let op = if run_prio == 2 { Query::And } else { Query::Or };
                let all = left.q.into_iter().chain(std::mem::take(&mut run)).collect();
                left = Removed::of(chain(all, op)).shallow()?;
                run_prio = 0;
            }
            let Some((prio, op)) = next else { return Ok(left) };
            p.i += 1;
            let right = binary(p, prio, operand, bad)?;
            if prio == 3 {
                left = Removed::join(left, right, op).shallow()?;
            } else {
                run_prio = prio;
                run.extend(right.q);
            }
        }
    }
    let mut p = P { t: &toks, i: 0, nest: 0 };
    let q = binary(&mut p, 0, operand, &bad)?;
    if p.i != toks.len() {
        return Err(bad());
    }
    Ok(q.q)
}

/// A query with stop words taken out, and how many positions went with them at each edge (for
/// the distance of a phrase around them, as Postgres's clean_stopwords counts it).
struct Removed {
    q: Option<Query>,
    left: u16,
    right: u16,
    depth: usize, // of q
}

/// Gaps added up, as far as a position goes.
fn gap(parts: &[u16]) -> u16 {
    parts.iter().map(|&p| p as u32).sum::<u32>().min(POS_MAX as u32) as u16
}

impl Removed {
    fn of(q: Option<Query>) -> Removed {
        let depth = q.as_ref().map_or(0, Query::depth);
        Removed { q, left: 0, right: 0, depth }
    }

    fn map(self, f: impl FnOnce(Query) -> Query) -> Removed {
        Removed { q: self.q.map(f), left: 0, right: 0, depth: self.depth + 1 }
    }

    /// Itself, if no deeper than a query may be.
    fn shallow(self) -> Result<Removed, String> {
        if self.depth > MAX_DEPTH { Err(too_deep()) } else { Ok(self) }
    }

    /// `a <d> b`.
    fn join(a: Removed, b: Removed, d: u16) -> Removed {
        let (x, y) = (Box::new, Box::new);
        match (a.q, b.q) {
            (Some(l), Some(r)) => Removed {
                q: Some(Query::Phrase(x(l), y(r), gap(&[a.right, d, b.left]))),
                left: a.left,
                right: b.right,
                depth: a.depth.max(b.depth) + 1,
            },
            (Some(l), None) => {
                Removed { q: Some(l), left: a.left, right: gap(&[a.right, d, b.left, b.right]), depth: a.depth }
            }
            (None, Some(r)) => {
                Removed { q: Some(r), left: gap(&[a.left, a.right, d, b.left]), right: b.right, depth: b.depth }
            }
            (None, None) => Removed { q: None, left: gap(&[a.left, a.right, d, b.left, b.right]), right: 0, depth: 0 },
        }
    }
}

/// `op` (& or |) over `qs`, left to right as Postgres joins them; a long chain is joined in
/// pairs first, so it nests as deep as the log of its length, not its length.
fn chain(mut qs: Vec<Query>, op: fn(Box<Query>, Box<Query>) -> Query) -> Option<Query> {
    while qs.len() > 32 {
        let mut it = qs.into_iter();
        let mut pairs = vec![];
        while let Some(a) = it.next() {
            pairs.push(match it.next() {
                Some(b) => op(Box::new(a), Box::new(b)),
                None => a,
            });
        }
        qs = pairs;
    }
    qs.into_iter().reduce(|a, b| op(Box::new(a), Box::new(b)))
}

/// Lexemes as a phrase: `a <-> b <-> c`, left to right, with stop words widening the gaps.
fn phrase(lexemes: Vec<Option<String>>, prefix: bool, weights: u8) -> Result<Option<Query>, String> {
    if lexemes.len() > MAX_DEPTH {
        return Err(too_deep());
    }
    let mut acc = Removed::of(None);
    let mut first = true;
    for l in lexemes {
        let r = Removed::of(l.map(|lex| Query::Val { lex, prefix, weights }));
        acc = if first { r } else { Removed::join(acc, r, 1) };
        first = false;
    }
    Ok(acc.q)
}

/// `'...'::tsquery`: operands as written.
pub fn parse_tsquery(s: &str) -> Result<Option<Query>, String> {
    if s.trim().is_empty() {
        return Ok(None); // Postgres: a notice, and the empty query
    }
    size_ok(s)?;
    let q = parse_query(s, &|l, prefix, weights| Ok(Some(Query::Val { lex: l.into(), prefix, weights })))?;
    if q.is_none() {
        return Err(format!("syntax error in tsquery: \"{s}\""));
    }
    Ok(q)
}

/// to_tsquery: each operand through the configuration (a word it splits becomes a phrase).
pub fn to_tsquery(cfg: Config, s: &str) -> Result<Option<Query>, String> {
    parse_query(s, &|l, prefix, weights| phrase(lexemes(cfg, l), prefix, weights))
}

/// plainto_tsquery: every word, joined by &.
pub fn plainto_tsquery(cfg: Config, s: &str) -> Option<Query> {
    chain(
        lexemes(cfg, s).into_iter().flatten().map(|lex| Query::Val { lex, prefix: false, weights: 0 }).collect(),
        Query::And,
    )
}

/// phraseto_tsquery: every word, joined by <->, stop words widening the gaps.
pub fn phraseto_tsquery(cfg: Config, s: &str) -> Result<Option<Query>, String> {
    phrase(lexemes(cfg, s), false, 0)
}

/// websearch_to_tsquery: words joined by &, "quoted phrases", `or`, and -word for not.
pub fn websearch_to_tsquery(cfg: Config, s: &str) -> Result<Option<Query>, String> {
    enum Item {
        Q(Query),
        Or,
    }
    let c: Vec<char> = s.chars().collect();
    let (mut items, mut i) = (vec![], 0);
    while i < c.len() {
        if c[i].is_whitespace() || (!c[i].is_alphanumeric() && c[i] != '"' && c[i] != '-') {
            i += 1;
            continue;
        }
        let neg = c[i] == '-';
        if neg {
            i += 1;
        }
        let q = if c.get(i) == Some(&'"') {
            let end = c[i + 1..].iter().position(|&x| x == '"').map_or(c.len(), |e| i + 1 + e);
            let text: String = c[i + 1..end].iter().collect();
            i = end + 1;
            phrase(lexemes(cfg, &text), false, 0)?
        } else {
            let from = i;
            while i < c.len() && !c[i].is_whitespace() && c[i] != '"' {
                i += 1;
            }
            let word: String = c[from..i].iter().collect();
            if !neg && word.eq_ignore_ascii_case("or") {
                items.push(Item::Or);
                continue;
            }
            phrase(lexemes(cfg, &word), false, 0)?
        };
        if let Some(q) = q {
            items.push(Item::Q(if neg { Query::Not(Box::new(q)) } else { q }));
        }
    }
    // & binds tighter than or; an or with nothing on one side is dropped
    let mut ors: Vec<Query> = vec![];
    let mut and: Vec<Query> = vec![];
    let mut pending_or = false;
    for it in items {
        match it {
            Item::Or => pending_or = !and.is_empty(),
            Item::Q(q) => {
                if pending_or {
                    ors.extend(chain(std::mem::take(&mut and), Query::And));
                    pending_or = false;
                }
                and.push(q);
            }
        }
    }
    ors.extend(chain(and, Query::And));
    Ok(chain(ors, Query::Or))
}

// ---- matching and ranking ----

/// Postgres's TSTernaryValue: a phrase over lexemes stored without positions can only "maybe" match.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Tern {
    No,
    Yes,
    Maybe,
}

/// Postgres's ExecPhraseData: where a match ends (sorted, unique), its width in lexemes less one,
/// and `negate`: matched everywhere except at `pos`.
#[derive(Default)]
struct PhraseData {
    pos: Vec<u16>,
    width: i32,
    negate: bool,
}

/// One lexeme's entry against an operand (`checkclass_str`): with `want`, the positions that pass
/// the weight filter. A lexeme stored without positions matches whatever the weight (as Postgres
/// keeps it), but only "maybe" when positions are needed.
fn check_entry(ps: &[u16], weights: u8, want: bool) -> (Tern, Vec<u16>) {
    let keep = |p: &&u16| weights & (1 << (**p >> 14)) != 0;
    if ps.is_empty() {
        return (if want { Tern::Maybe } else { Tern::Yes }, vec![]);
    }
    if weights != 0 && want {
        let out: Vec<u16> = ps.iter().filter(keep).map(|p| p & POS_MAX).collect();
        (if out.is_empty() { Tern::No } else { Tern::Yes }, out)
    } else if weights != 0 {
        (if ps.iter().any(|p| keep(&p)) { Tern::Yes } else { Tern::No }, vec![])
    } else if want {
        (Tern::Yes, ps.iter().map(|p| p & POS_MAX).collect())
    } else {
        (Tern::Yes, vec![])
    }
}

/// An operand against a tsvector (`checkcondition_str`): the exact lexeme, or with a prefix every
/// lexeme it starts, their positions merged.
fn check_operand(v: &TsVector, lex: &str, prefix: bool, weights: u8, want: bool) -> (Tern, Vec<u16>) {
    let mut res = v.0.get(lex).map_or((Tern::No, vec![]), |ps| check_entry(ps, weights, want));
    if prefix && (res.0 != Tern::Yes || want) {
        res = (Tern::No, vec![]);
        let mut all: Vec<u16> = vec![];
        for (_, ps) in v.0.range(lex.to_string()..).take_while(|(k, _)| k.starts_with(lex)) {
            if res.0 == Tern::Yes && !want {
                break;
            }
            let (sub, pos) = check_entry(ps, weights, want);
            if sub == Tern::No {
                continue;
            }
            if want {
                if sub == Tern::Maybe {
                    res.0 = Tern::Maybe;
                    all.clear();
                    break;
                }
                all.extend(pos);
            } else if sub == Tern::Yes || res.0 == Tern::No {
                res.0 = sub;
            }
        }
        if want && !all.is_empty() {
            all.sort_unstable();
            all.dedup();
            res = (Tern::Yes, all);
        }
    }
    res
}

const TSPO_L_ONLY: u8 = 1;
const TSPO_R_ONLY: u8 = 2;
const TSPO_BOTH: u8 = 4;

/// Postgres's TS_phrase_output: merges two sorted position lists (each moved by its offset),
/// keeping those `emit` asks for.
fn phrase_output(data: &mut PhraseData, l: &PhraseData, r: &PhraseData, emit: u8, loff: i32, roff: i32) -> Tern {
    let (mut li, mut ri) = (0, 0);
    while li < l.pos.len() || ri < r.pos.len() {
        let lpos = match l.pos.get(li) {
            Some(&p) => (p & POS_MAX) as i32 + loff,
            None if emit & TSPO_R_ONLY == 0 => break,
            None => i32::MAX,
        };
        let rpos = match r.pos.get(ri) {
            Some(&p) => (p & POS_MAX) as i32 + roff,
            None if emit & TSPO_L_ONLY == 0 => break,
            None => i32::MAX,
        };
        let mut out = 0;
        if lpos < rpos {
            if emit & TSPO_L_ONLY != 0 {
                out = lpos;
            }
            li += 1;
        } else if lpos == rpos {
            if emit & TSPO_BOTH != 0 {
                out = rpos;
            }
            li += 1;
            ri += 1;
        } else {
            if emit & TSPO_R_ONLY != 0 {
                out = rpos;
            }
            ri += 1;
        }
        if out > 0 {
            data.pos.push(out.min(u16::MAX as i32) as u16);
        }
    }
    if data.pos.is_empty() { Tern::No } else { Tern::Yes }
}

/// Postgres's TS_phrase_execute: a query at or below a phrase operator, where positions count.
fn phrase_execute(v: &TsVector, q: &Query, data: &mut PhraseData) -> Tern {
    match q {
        Query::Val { lex, prefix, weights } => {
            let (t, pos) = check_operand(v, lex, *prefix, *weights, true);
            if t == Tern::Yes {
                data.pos = pos;
            }
            t
        }
        Query::Not(a) => match phrase_execute(v, a, data) {
            Tern::No => {
                data.negate = true;
                Tern::Yes
            }
            Tern::Yes if !data.pos.is_empty() => {
                data.negate = !data.negate;
                Tern::Yes
            }
            Tern::Yes => {
                data.negate = false;
                Tern::No
            }
            Tern::Maybe => Tern::Maybe,
        },
        Query::Phrase(a, b, _) | Query::And(a, b) => {
            let (mut ld, mut rd) = (PhraseData::default(), PhraseData::default());
            let lm = phrase_execute(v, a, &mut ld);
            if lm == Tern::No {
                return Tern::No;
            }
            let rm = phrase_execute(v, b, &mut rd);
            if rm == Tern::No {
                return Tern::No;
            }
            if lm == Tern::Maybe || rm == Tern::Maybe {
                return Tern::Maybe;
            }
            let (loff, roff) = if let Query::Phrase(_, _, d) = q {
                data.width = *d as i32 + ld.width + rd.width;
                (*d as i32 + rd.width, 0)
            } else {
                let w = ld.width.max(rd.width);
                data.width = w;
                (w - ld.width, w - rd.width)
            };
            match (ld.negate, rd.negate) {
                (true, true) => {
                    phrase_output(data, &ld, &rd, TSPO_BOTH | TSPO_L_ONLY | TSPO_R_ONLY, loff, roff);
                    data.negate = true;
                    Tern::Yes
                }
                (true, false) => phrase_output(data, &ld, &rd, TSPO_R_ONLY, loff, roff),
                (false, true) => phrase_output(data, &ld, &rd, TSPO_L_ONLY, loff, roff),
                (false, false) => phrase_output(data, &ld, &rd, TSPO_BOTH, loff, roff),
            }
        }
        Query::Or(a, b) => {
            let (mut ld, mut rd) = (PhraseData::default(), PhraseData::default());
            let lm = phrase_execute(v, a, &mut ld);
            let rm = phrase_execute(v, b, &mut rd);
            if lm == Tern::No && rm == Tern::No {
                return Tern::No;
            }
            if lm == Tern::Maybe || rm == Tern::Maybe {
                return Tern::Maybe;
            }
            if lm == Tern::No {
                ld.width = 0;
            }
            if rm == Tern::No {
                rd.width = 0;
            }
            let w = ld.width.max(rd.width);
            let (loff, roff) = (w - ld.width, w - rd.width);
            data.width = w;
            match (ld.negate, rd.negate) {
                (true, true) => {
                    phrase_output(data, &ld, &rd, TSPO_BOTH, loff, roff);
                    data.negate = true;
                    Tern::Yes
                }
                (true, false) => {
                    phrase_output(data, &ld, &rd, TSPO_L_ONLY, loff, roff);
                    data.negate = true;
                    Tern::Yes
                }
                (false, true) => {
                    phrase_output(data, &ld, &rd, TSPO_R_ONLY, loff, roff);
                    data.negate = true;
                    Tern::Yes
                }
                (false, false) => phrase_output(data, &ld, &rd, TSPO_BOTH | TSPO_L_ONLY | TSPO_R_ONLY, loff, roff),
            }
        }
    }
}

/// Postgres's TS_execute_recurse above any phrase operator: positions don't matter here, and a
/// phrase that can only "maybe" match (no positions stored) does not match.
fn execute(v: &TsVector, q: &Query) -> Tern {
    match q {
        Query::Val { lex, prefix, weights } => check_operand(v, lex, *prefix, *weights, false).0,
        Query::Not(a) => match execute(v, a) {
            Tern::No => Tern::Yes,
            Tern::Yes => Tern::No,
            Tern::Maybe => Tern::Maybe,
        },
        Query::And(a, b) => {
            let l = execute(v, a);
            if l == Tern::No {
                return Tern::No;
            }
            match execute(v, b) {
                Tern::No => Tern::No,
                Tern::Yes => l,
                Tern::Maybe => Tern::Maybe,
            }
        }
        Query::Or(a, b) => {
            let l = execute(v, a);
            if l == Tern::Yes {
                return Tern::Yes;
            }
            match execute(v, b) {
                Tern::No => l,
                Tern::Yes => Tern::Yes,
                Tern::Maybe => Tern::Maybe,
            }
        }
        Query::Phrase(..) => match phrase_execute(v, q, &mut PhraseData::default()) {
            Tern::Yes => Tern::Yes,
            _ => Tern::No,
        },
    }
}

/// The order of tsqueries (Postgres's CompareTSQ): by the number of items, then their storage size (the
/// lexemes' bytes), then item by item (`QTNodeCompare`): an operator before an operand, a higher operator
/// (NOT 1, AND 2, OR 3, PHRASE 4) first, an operator's right operand before its left; operands by the
/// signed legacy CRC-32 of their lexeme, higher first, then by the lexeme.
pub fn compare_queries(a: &Option<Query>, b: &Option<Query>) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    fn count(q: &Query, items: &mut usize, bytes: &mut usize) {
        *items += 1;
        match q {
            Query::Val { lex, .. } => *bytes += lex.len() + 1,
            Query::Not(a) => count(a, items, bytes),
            Query::And(a, b) | Query::Or(a, b) | Query::Phrase(a, b, _) => {
                count(a, items, bytes);
                count(b, items, bytes);
            }
        }
    }
    fn oper(q: &Query) -> u8 {
        match q {
            Query::Val { .. } => 0,
            Query::Not(_) => 1,
            Query::And(..) => 2,
            Query::Or(..) => 3,
            Query::Phrase(..) => 4,
        }
    }
    fn node(a: &Query, b: &Query) -> Ordering {
        match (a, b) {
            (Query::Val { lex: x, .. }, Query::Val { lex: y, .. }) => {
                let crc = |s: &str| super::textext::legacy_crc32(s.as_bytes()) as i32;
                crc(y).cmp(&crc(x)).then_with(|| x.as_bytes().cmp(y.as_bytes()))
            }
            (Query::Val { .. }, _) => Ordering::Greater,
            (_, Query::Val { .. }) => Ordering::Less,
            _ if oper(a) != oper(b) => oper(b).cmp(&oper(a)),
            (Query::Not(x), Query::Not(y)) => node(x, y),
            (Query::And(al, ar) | Query::Or(al, ar), Query::And(bl, br) | Query::Or(bl, br)) => {
                node(ar, br).then_with(|| node(al, bl))
            }
            (Query::Phrase(al, ar, ad), Query::Phrase(bl, br, bd)) => {
                node(ar, br).then_with(|| node(al, bl)).then_with(|| bd.cmp(ad))
            }
            _ => Ordering::Equal,
        }
    }
    let size = |q: &Option<Query>| {
        let (mut items, mut bytes) = (0, 0);
        if let Some(q) = q {
            count(q, &mut items, &mut bytes);
        }
        (items, bytes)
    };
    let (sa, sb) = (size(a), size(b));
    sa.cmp(&sb).then_with(|| match (a, b) {
        (Some(x), Some(y)) => node(x, y),
        _ => Ordering::Equal,
    })
}

/// `v @@ q`. An empty query (only stop words) matches nothing.
pub fn matches(v: &TsVector, q: &Option<Query>) -> bool {
    q.as_ref().is_some_and(|q| execute(v, q) != Tern::No)
}

/// The query's distinct operands, in lexeme order (as Postgres's SortAndUniqItems).
fn operands(q: &Query, out: &mut Vec<(String, bool)>) {
    match q {
        Query::Val { lex, prefix, .. } => out.push((lex.clone(), *prefix)),
        Query::Not(a) => operands(a, out),
        Query::And(a, b) | Query::Or(a, b) | Query::Phrase(a, b, _) => {
            operands(a, out);
            operands(b, out);
        }
    }
}

/// `a @> b` of two tsqueries (tsq_mcontains): every lexeme `b` names is one `a` names, whatever the operators, the
/// weights and the prefixes between them; an empty query names none.
pub fn contains(a: &Option<Query>, b: &Option<Query>) -> bool {
    let lexemes = |q: &Option<Query>| {
        let mut out = vec![];
        if let Some(q) = q {
            operands(q, &mut out);
        }
        out.into_iter().map(|(l, _)| l).collect::<std::collections::BTreeSet<String>>()
    };
    lexemes(b).is_subset(&lexemes(a))
}

const RANK_WEIGHTS: [f32; 4] = [0.1, 0.2, 0.4, 1.0];
const MAX_ENTRY_POS: u16 = 1 << 14;

/// ts_rank, computed as Postgres's tsrank.c does, in the same single-precision steps. It's
/// quadratic in the query's operands (and their positions): it stops when the statement has to.
pub fn rank(weights: &[f32; 4], v: &TsVector, q: &Option<Query>, method: i32) -> super::Result<f32> {
    let Some(q) = q else { return Ok(0.0) };
    if v.0.is_empty() {
        return Ok(0.0);
    }
    let mut items = vec![];
    operands(q, &mut items);
    items.sort_by(|a, b| a.0.cmp(&b.0));
    items.dedup_by(|a, b| a.0 == b.0);
    // each operand's entries: exact, or every lexeme with the prefix
    let entries = |(lex, prefix): &(String, bool)| -> Vec<&Vec<u16>> {
        if *prefix {
            v.0.range(lex.clone()..).take_while(|(k, _)| k.starts_with(lex.as_str())).map(|(_, p)| p).collect()
        } else {
            v.0.get(lex).into_iter().collect()
        }
    };
    let null_pos = [MAX_ENTRY_POS - 1];
    let wpos = |p: u16| weights[(p >> 14) as usize];
    let and = matches!(q, Query::And(..) | Query::Phrase(..));
    let mut res: f32 = if and && items.len() >= 2 {
        // calc_rank_and: every pair of operands' positions, closer counting more
        let mut res: f32 = -1.0;
        let mut pos: Vec<Option<(&[u16], bool)>> = vec![None; items.len()];
        for i in 0..items.len() {
            for e in entries(&items[i]) {
                let (post, is_null): (&[u16], bool) = if e.is_empty() { (&null_pos, true) } else { (e, false) };
                pos[i] = Some((post, is_null));
                for &earlier in &pos[..i] {
                    super::tick()?;
                    let Some((ct, k_null)) = earlier else { continue };
                    for &l in post {
                        super::tick()?;
                        for &p in ct {
                            let mut dist = ((l & (MAX_ENTRY_POS - 1)) as i32 - (p & (MAX_ENTRY_POS - 1)) as i32).abs();
                            if dist != 0 || is_null || k_null {
                                if dist == 0 {
                                    dist = MAX_ENTRY_POS as i32;
                                }
                                let wd: f32 = if dist > 100 {
                                    1e-30
                                } else {
                                    (1.0 / (1.005 + 0.05 * ((dist as f32) as f64 / 1.5 - 2.0).exp())) as f32
                                };
                                let curw = ((wpos(l) * wpos(p) * wd) as f64).sqrt() as f32;
                                res = if res < 0.0 {
                                    curw
                                } else {
                                    (1.0 - (1.0 - res as f64) * (1.0 - curw as f64)) as f32
                                };
                            }
                        }
                    }
                }
            }
        }
        res
    } else {
        // calc_rank_or: each occurrence counts less than the one before
        let mut res: f32 = 0.0;
        for it in &items {
            for e in entries(it) {
                super::tick()?;
                let post: &[u16] = if e.is_empty() { &[0] } else { e };
                let (mut resj, mut wjm, mut jm): (f32, f32, i32) = (0.0, -1.0, 0);
                for (j, &p) in post.iter().enumerate() {
                    let j = j as i32;
                    resj += wpos(p) / ((j + 1) * (j + 1)) as f32;
                    if wpos(p) > wjm {
                        wjm = wpos(p);
                        jm = j;
                    }
                }
                let t = wjm + resj - wjm / ((jm + 1) * (jm + 1)) as f32;
                res = (res as f64 + t as f64 / 1.64493406685) as f32;
            }
        }
        if !items.is_empty() {
            res /= items.len() as f32;
        }
        res
    };
    if res < 0.0 {
        res = 1e-20;
    }
    let length: usize = v.0.values().map(|p| p.len().max(1)).sum();
    if method & 1 != 0 {
        res = (res as f64 / ((length as f64 + 1.0).ln() / 2f64.ln())) as f32;
    }
    if method & 2 != 0 && length > 0 {
        res /= length as f32;
    }
    if method & 8 != 0 {
        res /= v.0.len() as f32;
    }
    if method & 16 != 0 {
        res = (res as f64 / ((v.0.len() as f64 + 1.0).ln() / 2f64.ln())) as f32;
    }
    if method & 32 != 0 {
        res /= res + 1.0;
    }
    Ok(res)
}

pub fn default_weights() -> [f32; 4] {
    RANK_WEIGHTS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stems_as_snowball() {
        for (w, s) in [
            ("caresses", "caress"),
            ("ponies", "poni"),
            ("ties", "tie"),
            ("cats", "cat"),
            ("feed", "feed"),
            ("agreed", "agre"),
            ("plastered", "plaster"),
            ("motoring", "motor"),
            ("hopping", "hop"),
            ("hoping", "hope"),
            ("happy", "happi"),
            ("relational", "relat"),
            ("generalization", "general"),
            ("running", "run"),
            ("dogs", "dog"),
            ("lazy", "lazi"),
            ("jumped", "jump"),
            ("foxes", "fox"),
        ] {
            assert_eq!(stem(w), s, "{w}");
        }
    }

    #[test]
    fn vectors_and_queries_print_as_postgres() {
        let v = to_tsvector(Config::English, "The quick brown foxes jumped over the lazy dogs");
        assert_eq!(v.to_string(), "'brown':3 'dog':9 'fox':4 'jump':5 'lazi':8 'quick':2");
        let q = to_tsquery(Config::English, "fat & (rat | !cat)").unwrap();
        assert_eq!(Query::text(&q), "'fat' & ( 'rat' | !'cat' )");
        assert_eq!(Query::text(&to_tsquery(Config::English, "cat <-> the <-> rat").unwrap()), "'cat' <2> 'rat'");
        assert!(matches(&v, &to_tsquery(Config::English, "quick <-> brown & dog").unwrap()));
        assert!(!matches(&v, &to_tsquery(Config::English, "brown <-> quick").unwrap()));
    }
}

/// ts_parse and ts_debug's tokens: id, class alias, token (the blanks between them too).
pub fn parsed(s: &str) -> Vec<(i64, &'static str, String)> {
    super::tsparser::parse(s).into_iter().map(|(t, w)| (i64::from(t), Kind(t).class(), w)).collect()
}

/// ts_debug's row of a token: alias, description, token, the dictionaries asked, the one that took it, its lexemes.
#[allow(clippy::type_complexity)]
pub fn debug_row(
    cfg: Config,
    id: i64,
    alias: &'static str,
    token: &str,
) -> (String, String, String, Vec<String>, Option<String>, Option<Vec<String>>) {
    let desc = TOKEN_TYPES.get((id - 1).max(0) as usize).map_or("", |t| t.1).to_string();
    let dicts = dict_chain(cfg, alias);
    let (mut used, mut lex) = (None, None);
    for d in &dicts {
        match lexize(d, token) {
            Ok(Lexized::Lexemes(l)) => {
                (used, lex) = (Some(d.clone()), Some(l));
                break;
            }
            Ok(Lexized::Stop) => {
                (used, lex) = (Some(d.clone()), Some(vec![]));
                break;
            }
            _ => {}
        }
    }
    (alias.to_string(), desc, token.to_string(), dicts, used, lex)
}

/// How many tokens a text has (stop words count: they take a position each).
pub fn token_count(s: &str) -> usize {
    tokens(s).len()
}

// ---- ts_rank_cd: cover density, as Postgres's tsrank.c has it ----

/// A query's operands in a fixed order, with their weights masks.
fn operand_list(q: &Query, out: &mut Vec<(String, bool, u8)>) {
    match q {
        Query::Val { lex, prefix, weights } => out.push((lex.clone(), *prefix, *weights)),
        Query::Not(a) => operand_list(a, out),
        Query::And(a, b) | Query::Or(a, b) | Query::Phrase(a, b, _) => {
            operand_list(a, out);
            operand_list(b, out);
        }
    }
}

#[derive(Clone, Default)]
struct OpState {
    exists: bool,
    /// the positions seen (without weights), ascending
    pos: Vec<u16>,
}

/// Whether the query holds for the operands seen so far, and (for a phrase) the positions it ends at.
fn exec_cd(q: &Query, ops: &[OpState], at: &mut usize) -> (bool, Vec<u16>) {
    match q {
        Query::Val { .. } => {
            let o = &ops[*at];
            *at += 1;
            (o.exists, o.pos.clone())
        }
        Query::Not(a) => {
            let (b, _) = exec_cd(a, ops, at);
            (!b, vec![])
        }
        Query::And(a, b) => {
            let ((x, px), (y, py)) = (exec_cd(a, ops, at), exec_cd(b, ops, at));
            let mut p = px;
            p.extend(py);
            p.sort_unstable();
            p.dedup();
            (x && y, p)
        }
        Query::Or(a, b) => {
            let ((x, px), (y, py)) = (exec_cd(a, ops, at), exec_cd(b, ops, at));
            let mut p = if x { px } else { vec![] };
            if y {
                p.extend(py);
            }
            p.sort_unstable();
            p.dedup();
            (x || y, p)
        }
        Query::Phrase(a, b, n) => {
            let ((x, px), (y, py)) = (exec_cd(a, ops, at), exec_cd(b, ops, at));
            if !(x && y) {
                return (false, vec![]);
            }
            let out: Vec<u16> =
                py.iter().copied().filter(|r| px.iter().any(|l| *r as i32 - *l as i32 == *n as i32)).collect();
            (!out.is_empty(), out)
        }
    }
}

/// ts_rank_cd(weights, vector, query, normalization).
pub fn rank_cd(weights: &[f32; 4], v: &TsVector, q: &Option<Query>, method: i32) -> super::Result<f32> {
    let Some(q) = q else { return Ok(0.0) };
    let mut ops = vec![];
    operand_list(q, &mut ops);
    // the document: (position with weight, the lexeme, the operands it is) by position
    let mut found: Vec<(u16, String, usize)> = vec![];
    for (i, (lex, prefix, mask)) in ops.iter().enumerate() {
        let entries: Vec<(&String, &Vec<u16>)> = if *prefix {
            v.0.range(lex.clone()..).take_while(|(k, _)| k.starts_with(lex.as_str())).collect()
        } else {
            v.0.get_key_value(lex).into_iter().collect()
        };
        for (k, ps) in entries {
            for &p in ps {
                if *mask == 0 || *mask & (1 << (p >> 14)) != 0 {
                    found.push((p, k.clone(), i));
                }
            }
        }
    }
    if found.is_empty() {
        return Ok(0.0);
    }
    found.sort_by(|a, b| ((a.0 & (MAX_ENTRY_POS - 1)), &a.1, a.0).cmp(&((b.0 & (MAX_ENTRY_POS - 1)), &b.1, b.0)));
    let mut doc: Vec<(u16, Vec<usize>)> = vec![];
    let mut last: Option<(u16, String)> = None;
    for (p, k, i) in found {
        match (&last, doc.last_mut()) {
            (Some((lp, lk)), Some(d)) if *lp == p && *lk == k => d.1.push(i),
            _ => doc.push((p, vec![i])),
        }
        last = Some((p, k));
    }
    let wpos = |p: u16| (p & (MAX_ENTRY_POS - 1)) as i32;
    let fill = |st: &mut Vec<OpState>, e: &(u16, Vec<usize>), reverse: bool| {
        for &i in &e.1 {
            let o = &mut st[i];
            o.exists = true;
            let p = e.0 & (MAX_ENTRY_POS - 1);
            let at = if reverse { 0 } else { o.pos.len().saturating_sub(1) };
            if o.pos.is_empty() {
                o.pos.push(p);
            } else if o.pos[at] != p {
                if reverse { o.pos.insert(0, p) } else { o.pos.push(p) }
            }
        }
    };
    let run = |st: &Vec<OpState>| exec_cd(q, st, &mut 0).0;
    let invws: Vec<f64> = weights.iter().map(|w| 1.0 / f64::from(*w)).collect();
    let (mut wdoc, mut sum_dist, mut prev, mut n_extent) = (0.0f64, 0.0f64, 0.0f64, 0i64);
    let mut ext_pos: usize = 0;
    loop {
        super::tick()?;
        // the next cover: its upper bound going up, then its lower bound going down
        let (mut ep, mut eq, mut begin, mut end) = (i32::MAX, 0i32, 0usize, 0usize);
        let covered = loop {
            let mut st = vec![OpState::default(); ops.len()];
            let (mut found_up, mut lastpos) = (false, 0usize);
            let mut ptr = ext_pos;
            while ptr < doc.len() {
                fill(&mut st, &doc[ptr], false);
                if run(&st) {
                    if wpos(doc[ptr].0) > eq {
                        eq = wpos(doc[ptr].0);
                        end = ptr;
                        lastpos = ptr;
                        found_up = true;
                    }
                    break;
                }
                ptr += 1;
            }
            if !found_up {
                break false;
            }
            let mut st = vec![OpState::default(); ops.len()];
            let mut ptr = lastpos as isize;
            while ptr >= ext_pos as isize {
                fill(&mut st, &doc[ptr as usize], true);
                if run(&st) {
                    if wpos(doc[ptr as usize].0) < ep {
                        begin = ptr as usize;
                        ep = wpos(doc[ptr as usize].0);
                    }
                    break;
                }
                ptr -= 1;
            }
            if ep <= eq {
                ext_pos = (ptr + 1) as usize;
                break true;
            }
            ext_pos += 1;
            (ep, eq) = (i32::MAX, 0);
        };
        if !covered {
            break;
        }
        let inv_sum: f64 = (begin..=end).map(|k| invws[(doc[k].0 >> 14) as usize]).sum();
        let cpos = (end - begin + 1) as f64 / inv_sum;
        let mut noise = (eq - ep) - (end as i32 - begin as i32);
        if noise < 0 {
            noise = (end as i32 - begin as i32) / 2;
        }
        wdoc += cpos / f64::from(1 + noise);
        let cur = f64::from(eq + ep) / 2.0;
        if n_extent > 0 && cur > prev {
            sum_dist += 1.0 / (cur - prev);
        }
        prev = cur;
        n_extent += 1;
    }
    let length: usize = v.0.values().map(|p| p.len().max(1)).sum();
    let size = v.0.len() as f64;
    if method & 1 != 0 && size > 0.0 {
        wdoc /= ((length + 1) as f64).ln();
    }
    if method & 2 != 0 && length > 0 {
        wdoc /= length as f64;
    }
    if method & 4 != 0 && n_extent > 0 && sum_dist > 0.0 {
        wdoc /= n_extent as f64 / sum_dist;
    }
    if method & 8 != 0 && size > 0.0 {
        wdoc /= size;
    }
    if method & 16 != 0 && size > 0.0 {
        wdoc /= (size + 1.0).ln() / 2f64.ln();
    }
    if method & 32 != 0 {
        wdoc /= wdoc + 1.0;
    }
    Ok(wdoc as f32)
}

// ---- ts_headline: the default parser's headline selection, as Postgres's wparser_def.c does it ----

/// What a token gives under `cfg`.
fn lexize_token(cfg: Config, t: &str, kind: Kind) -> Lexized {
    if let Config::User(_) = cfg {
        for d in dict_chain(cfg, class_of(t, kind)) {
            match lexize(&d, t) {
                Ok(Lexized::Unknown) | Err(_) => {}
                Ok(l) => return l,
            }
        }
        return Lexized::Unknown;
    }
    let low = t.to_lowercase();
    match (cfg, kind) {
        (Config::English, k) if k.is_word() && STOP.contains(&low.as_str()) => Lexized::Stop,
        (Config::English, k) if k.is_word() => Lexized::Lexemes(vec![stem(&low)]),
        _ => Lexized::Lexemes(vec![low]),
    }
}

struct HWord {
    word: String,
    /// the token class alias; "blank" for the text between words
    class: &'static str,
    pos: i32,
    item: Option<usize>,
    repeated: bool,
    selected: bool,
    in_: bool,
    skip: bool,
    replace: bool,
}

/// The whole text as tokens, as the default parser cuts it: words, numbers and the blank runs between them.
fn hl_tokens(s: &str) -> Vec<(String, &'static str, Kind)> {
    super::tsparser::parse(s).into_iter().map(|(t, w)| (w, Kind(t).class(), Kind(t))).collect()
}

/// Whether `q` holds over the words in `range`, as TS_execute does with checkcondition_HL.
fn holds_in(q: &Query, words: &[HWord], nops: usize) -> bool {
    let mut st = vec![OpState::default(); nops];
    for w in words {
        if let Some(i) = w.item {
            st[i].exists = true;
            let p = w.pos as u16;
            if st[i].pos.last().is_none_or(|l| *l < p) {
                st[i].pos.push(p);
            }
        }
    }
    exec_cd(q, &st, &mut 0).0
}

/// A match location: the positions a term or phrase ends at, and its width.
type Loc = (Vec<i32>, i32);

fn locations(q: &Query, words: &[HWord], at: &mut usize) -> (bool, Vec<Loc>) {
    match q {
        Query::Val { .. } => {
            let i = *at;
            *at += 1;
            let mut pos: Vec<i32> = vec![];
            for w in words.iter().filter(|w| w.item == Some(i)) {
                if pos.last().is_none_or(|l| *l < w.pos) {
                    pos.push(w.pos);
                }
            }
            if pos.is_empty() { (false, vec![]) } else { (true, vec![(pos, 0)]) }
        }
        Query::Not(a) => {
            let (m, _) = locations(a, words, at);
            (!m, vec![])
        }
        Query::And(a, b) => {
            let ((x, mut lx), (y, ly)) = (locations(a, words, at), locations(b, words, at));
            if !(x && y) {
                return (false, vec![]);
            }
            lx.extend(ly);
            (true, lx)
        }
        Query::Or(a, b) => {
            let ((x, lx), (y, ly)) = (locations(a, words, at), locations(b, words, at));
            if !(x || y) {
                return (false, vec![]);
            }
            if lx.is_empty() {
                return (true, ly);
            }
            if ly.is_empty() {
                return (true, lx);
            }
            let mut out = vec![];
            for (pl, wl) in &lx {
                for (pr, wr) in &ly {
                    let mut p: Vec<i32> = pl.iter().chain(pr).copied().collect();
                    p.sort_unstable();
                    p.dedup();
                    out.push((p, *wl.max(wr)));
                }
            }
            (true, out)
        }
        Query::Phrase(a, b, n) => {
            let ((x, lx), (y, ly)) = (locations(a, words, at), locations(b, words, at));
            if !(x && y) {
                return (false, vec![]);
            }
            let (Some((pl, wl)), Some((pr, wr))) = (lx.first(), ly.first()) else { return (false, vec![]) };
            let mut pos = vec![];
            let mut width = 0;
            for &r in pr {
                for &l in pl {
                    if r - wr - l == i32::from(*n) {
                        pos.push(r);
                        width = width.max(r - (l - wl));
                    }
                }
            }
            if pos.is_empty() { (false, vec![]) } else { (true, vec![(pos, width)]) }
        }
    }
}

/// What ts_headline decides by: the document's words with the query's operands they match, and the options.
struct Hl<'a> {
    words: Vec<HWord>,
    nops: usize,
    query: &'a Option<Query>,
    locs: Vec<Loc>,
    shortword: i32,
    min_words: i32,
    max_words: i32,
    highlightall: bool,
}

fn nonword(c: &str) -> bool {
    matches!(c, "blank" | "tag" | "url" | "numhword" | "asciihword" | "hword")
}

fn noend(c: &str) -> bool {
    nonword(c) || matches!(c, "sfloat" | "version" | "float" | "int" | "uint" | "protocol" | "entity")
}

fn skipclass(c: &str) -> bool {
    matches!(c, "url" | "numhword" | "asciihword" | "hword")
}

impl Hl<'_> {
    fn bad_end(&self, j: usize) -> bool {
        noend(self.words[j].class) || (self.words[j].word.len() as i32) <= self.shortword
    }

    fn interesting(&self, j: usize) -> bool {
        self.words[j].item.is_some() && !self.words[j].repeated
    }

    /// hlCover: the next minimal cover from lexeme position `nextpos`.
    #[inline(never)]
    fn cover(&self, nextpos: &mut i32) -> Option<(usize, usize)> {
        let q = self.query.as_ref()?;
        let mut pos = *nextpos;
        loop {
            let mut pose = -1;
            for (ps, _) in &self.locs {
                let first = ps.iter().copied().find(|e| *e >= pos)?;
                pose = pose.max(first);
            }
            if pose < 0 {
                return None;
            }
            let mut posb = i32::MAX - 1;
            for (ps, w) in &self.locs {
                let last = ps.iter().rev().map(|p| p - w).find(|st| *st <= pose).unwrap_or(-1);
                posb = posb.min(last);
            }
            posb = posb.max(pos);
            if posb <= pose {
                let (mut idxb, mut idxe) = (-1i64, -1i64);
                for (i, w) in self.words.iter().enumerate() {
                    if w.item.is_none() {
                        continue;
                    }
                    if idxb < 0 && w.pos >= posb {
                        idxb = i as i64;
                    }
                    if w.pos <= pose {
                        idxe = i as i64;
                    } else {
                        break;
                    }
                }
                if idxb >= 0 && idxe >= idxb && holds_in(q, &self.words[idxb as usize..=idxe as usize], self.nops) {
                    *nextpos = posb + 1;
                    return Some((idxb as usize, idxe as usize));
                }
            }
            pos = posb + 1;
        }
    }

    /// mark_fragment: the words from `from` to `to` are in, the query's among them selected.
    #[inline(never)]
    fn mark(&mut self, from: i64, to: i64) {
        let mut i = from;
        while i <= to && i >= 0 && (i as usize) < self.words.len() {
            let w = &mut self.words[i as usize];
            if w.item.is_some() {
                w.selected = true;
            }
            if !self.highlightall {
                if w.class == "tag" {
                    w.replace = true;
                } else if skipclass(w.class) {
                    w.skip = true;
                }
            } else if skipclass(w.class) {
                w.skip = true;
            }
            w.in_ = !w.repeated;
            i += 1;
        }
    }

    /// The first `min_words` words, for when nothing else was chosen.
    fn first_words(&self) -> i64 {
        let (mut curlen, mut end) = (0, -1i64);
        let mut i = 0;
        while i < self.words.len() && curlen < self.min_words {
            if !nonword(self.words[i].class) {
                curlen += 1;
            }
            end = i as i64;
            i += 1;
        }
        end
    }

    /// mark_hl_words: one headline, the best of the covers.
    #[inline(never)]
    fn select_words(&mut self) {
        let n = self.words.len();
        let (min_words, max_words) = (self.min_words, self.max_words);
        let (mut bestb, mut beste, mut bestlen, mut bestcover) = (-1i64, -1i64, -1i64, false);
        if self.highlightall {
            self.mark(0, n as i64 - 1);
            return;
        }
        let mut nextpos = 0;
        while let Some((p, q)) = self.cover(&mut nextpos) {
            let (p, q) = (p as i64, q as i64);
            let (mut curlen, mut poslen) = (0i32, 0i64);
            let (mut posb, mut pose) = (p, p);
            let mut i = p;
            while i <= q && curlen < max_words {
                if !nonword(self.words[i as usize].class) {
                    curlen += 1;
                }
                if self.interesting(i as usize) {
                    poslen += 1;
                }
                pose = i;
                i += 1;
            }
            if curlen < max_words {
                i -= 1;
                while (i as usize) < n && curlen < max_words {
                    if i > q {
                        if !nonword(self.words[i as usize].class) {
                            curlen += 1;
                        }
                        if self.interesting(i as usize) {
                            poslen += 1;
                        }
                    }
                    pose = i;
                    if self.bad_end(i as usize) {
                        i += 1;
                        continue;
                    }
                    if curlen >= min_words {
                        break;
                    }
                    i += 1;
                }
                if curlen < min_words {
                    i = p - 1;
                    while i >= 0 {
                        if !nonword(self.words[i as usize].class) {
                            curlen += 1;
                        }
                        if self.interesting(i as usize) {
                            poslen += 1;
                        }
                        if curlen >= max_words {
                            break;
                        }
                        if self.bad_end(i as usize) {
                            i -= 1;
                            continue;
                        }
                        if curlen >= min_words {
                            break;
                        }
                        i -= 1;
                    }
                    posb = if i >= 0 { i } else { 0 };
                }
            } else {
                if i > q {
                    i = q;
                }
                while curlen > min_words {
                    if !self.bad_end(i as usize) {
                        break;
                    }
                    if !nonword(self.words[i as usize].class) {
                        curlen -= 1;
                    }
                    if self.interesting(i as usize) {
                        poslen -= 1;
                    }
                    pose = i - 1;
                    i -= 1;
                }
            }
            let poscover = posb <= p && pose >= q;
            if poscover & !bestcover
                || (poscover == bestcover && poslen > bestlen)
                || (poscover == bestcover
                    && poslen == bestlen
                    && beste >= 0
                    && pose >= 0
                    && !self.bad_end(pose as usize)
                    && self.bad_end(beste as usize))
            {
                (bestb, beste, bestlen, bestcover) = (posb, pose, poslen, poscover);
            }
        }
        if bestlen < 0 {
            bestb = 0;
            beste = self.first_words();
        }
        self.mark(bestb, beste);
    }

    /// mark_hl_fragments: up to `max_fragments` stretches around the best covers.
    #[inline(never)]
    fn select_fragments(&mut self, max_fragments: i32) {
        struct CoverPos {
            start: i64,
            end: i64,
            curlen: i32,
            poslen: i32,
            chosen: bool,
            excluded: bool,
        }
        let (min_words, max_words) = (self.min_words, self.max_words);
        let n = self.words.len();
        let mut covers: Vec<CoverPos> = vec![];
        let mut nextpos = 0;
        while let Some((p, q)) = self.cover(&mut nextpos) {
            let (mut startpos, mut endpos) = (p as i64, q as i64);
            while startpos <= endpos {
                // get_next_fragment
                let mut i = startpos;
                while i <= endpos {
                    startpos = i;
                    if self.interesting(i as usize) {
                        break;
                    }
                    i += 1;
                }
                let (mut curlen, mut poslen) = (0i32, 0i32);
                i = startpos;
                while i <= endpos && curlen < max_words {
                    if !nonword(self.words[i as usize].class) {
                        curlen += 1;
                    }
                    if self.interesting(i as usize) {
                        poslen += 1;
                    }
                    i += 1;
                }
                if endpos > i {
                    endpos = i;
                    while i >= startpos {
                        endpos = i;
                        if self.interesting(i as usize) {
                            break;
                        }
                        if !nonword(self.words[i as usize].class) {
                            curlen -= 1;
                        }
                        i -= 1;
                    }
                }
                covers.push(CoverPos { start: startpos, end: endpos, curlen, poslen, chosen: false, excluded: false });
                startpos = endpos + 1;
                endpos = q as i64;
            }
        }
        let mut chosen_any = false;
        for _ in 0..max_fragments {
            let (mut maxitems, mut minwords, mut min_i) = (0, i32::MAX, -1i64);
            for (i, c) in covers.iter().enumerate() {
                if !c.chosen && !c.excluded && (maxitems < c.poslen || (maxitems == c.poslen && minwords > c.curlen)) {
                    (maxitems, minwords, min_i) = (c.poslen, c.curlen, i as i64);
                }
            }
            if min_i < 0 {
                break;
            }
            let mi = min_i as usize;
            covers[mi].chosen = true;
            let (mut startpos, mut endpos, mut curlen) = (covers[mi].start, covers[mi].end, covers[mi].curlen);
            if curlen < max_words {
                let maxstretch = (max_words - curlen) / 2;
                let mut stretch = 0;
                let mut posmarker = startpos;
                let mut i = startpos - 1;
                while i >= 0 && stretch < maxstretch && !self.words[i as usize].in_ {
                    if !nonword(self.words[i as usize].class) {
                        curlen += 1;
                        stretch += 1;
                    }
                    posmarker = i;
                    i -= 1;
                }
                i = posmarker;
                while i < startpos && self.bad_end(i as usize) {
                    if !nonword(self.words[i as usize].class) {
                        curlen -= 1;
                    }
                    i += 1;
                }
                startpos = i;
                posmarker = endpos;
                i = endpos + 1;
                while (i as usize) < n && curlen < max_words && !self.words[i as usize].in_ {
                    if !nonword(self.words[i as usize].class) {
                        curlen += 1;
                    }
                    posmarker = i;
                    i += 1;
                }
                i = posmarker;
                while i > endpos && self.bad_end(i as usize) {
                    if !nonword(self.words[i as usize].class) {
                        curlen -= 1;
                    }
                    i -= 1;
                }
                endpos = i;
            }
            covers[mi].start = startpos;
            covers[mi].end = endpos;
            covers[mi].curlen = curlen;
            self.mark(startpos, endpos);
            chosen_any = true;
            for (i, c) in covers.iter_mut().enumerate() {
                if i != mi
                    && ((c.start >= startpos && c.start <= endpos)
                        || (c.end >= startpos && c.end <= endpos)
                        || (c.start < startpos && c.end > endpos))
                {
                    c.excluded = true;
                }
            }
        }
        if !chosen_any {
            let end = self.first_words();
            self.mark(0, end);
        }
        let _ = min_words;
    }

    /// generateHeadline: the words that are in, the selected ones between the markers, fragments apart.
    fn render(&self, startsel: &str, stopsel: &str, fragdelim: &str) -> String {
        let (mut out, mut infrag, mut frags) = (String::new(), false, 0);
        for w in &self.words {
            if w.in_ && !w.repeated {
                if !infrag {
                    infrag = true;
                    frags += 1;
                    if frags > 1 {
                        out.push_str(fragdelim);
                    }
                }
                if w.replace {
                    out.push(' ');
                } else if !w.skip {
                    if w.selected {
                        out.push_str(startsel);
                    }
                    out.push_str(&w.word);
                    if w.selected {
                        out.push_str(stopsel);
                    }
                }
            } else if !w.repeated {
                infrag = false;
            }
        }
        out
    }
}

/// ts_headline's options: `Name=value, ...`, a value maybe in double quotes.
#[inline(never)]
fn headline_options(options: &str) -> Result<Vec<(String, String)>, String> {
    let oc: Vec<char> = options.chars().collect();
    let (mut out, mut i) = (vec![], 0);
    while i < oc.len() {
        while i < oc.len() && (oc[i].is_whitespace() || oc[i] == ',') {
            i += 1;
        }
        if i >= oc.len() {
            break;
        }
        let ks = i;
        while i < oc.len() && oc[i] != '=' && oc[i] != ',' {
            i += 1;
        }
        let key: String = oc[ks..i].iter().collect::<String>().trim().to_string();
        let mut val = String::new();
        if i < oc.len() && oc[i] == '=' {
            i += 1;
            while i < oc.len() && oc[i].is_whitespace() {
                i += 1;
            }
            if i < oc.len() && oc[i] == '"' {
                i += 1;
                while i < oc.len() {
                    if oc[i] == '"' {
                        if i + 1 < oc.len() && oc[i + 1] == '"' {
                            val.push('"');
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    val.push(oc[i]);
                    i += 1;
                }
            } else {
                let vs = i;
                while i < oc.len() && oc[i] != ',' {
                    i += 1;
                }
                val = oc[vs..i].iter().collect::<String>().trim().to_string();
            }
        }
        out.push((key, val));
    }
    Ok(out)
}

/// The document's words, lexized, with the query operands each matches.
#[inline(never)]
fn headline_words(cfg: Config, doc: &str, ops: &[(String, bool, u8)]) -> Vec<HWord> {
    let mut words: Vec<HWord> = vec![];
    let mut vectorpos = 0i32;
    for (t, class, kind) in hl_tokens(doc) {
        let blank = HWord {
            word: t.clone(),
            class,
            pos: 0,
            item: None,
            repeated: false,
            selected: false,
            in_: false,
            skip: false,
            replace: false,
        };
        if class == "blank" {
            words.push(blank);
            continue;
        }
        match lexize_token(cfg, &t, kind) {
            Lexized::Unknown => words.push(blank),
            lex => {
                vectorpos += 1;
                let lexemes = if let Lexized::Lexemes(l) = lex { l } else { vec![] };
                words.push(HWord { pos: vectorpos, ..blank });
                for l in &lexemes {
                    for (k, (lex, prefix, _)) in ops.iter().enumerate() {
                        let hit = if *prefix { l.starts_with(lex.as_str()) } else { l == lex };
                        if !hit {
                            continue;
                        }
                        if words.last().is_some_and(|w| w.item.is_none() && !w.repeated) {
                            words.last_mut().unwrap().item = Some(k);
                        } else {
                            words.push(HWord {
                                word: t.clone(),
                                class,
                                pos: vectorpos,
                                item: Some(k),
                                repeated: true,
                                selected: false,
                                in_: false,
                                skip: false,
                                replace: false,
                            });
                        }
                    }
                }
            }
        }
    }
    words
}

/// ts_headline: the document with the query's words marked, an excerpt chosen as Postgres chooses it.
pub fn headline(cfg: Config, doc: &str, query: &Option<Query>, options: &str) -> Result<String, String> {
    let (mut min_words, mut max_words, mut shortword, mut max_fragments, mut highlightall) =
        (15i32, 35i32, 3i32, 0i32, false);
    let (mut startsel, mut stopsel, mut fragdelim) = (None::<String>, None::<String>, None::<String>);
    for (key, val) in headline_options(options)? {
        let num = |v: &str| {
            v.trim().parse::<i32>().map_err(|_| format!("invalid value for headline parameter \"{key}\": \"{v}\""))
        };
        match key.to_ascii_lowercase().as_str() {
            "maxwords" => max_words = num(&val)?,
            "minwords" => min_words = num(&val)?,
            "shortword" => shortword = num(&val)?,
            "maxfragments" => max_fragments = num(&val)?,
            "startsel" => startsel = Some(val),
            "stopsel" => stopsel = Some(val),
            "fragmentdelimiter" => fragdelim = Some(val),
            "highlightall" => {
                highlightall = matches!(val.to_ascii_lowercase().as_str(), "1" | "on" | "true" | "t" | "y" | "yes")
            }
            _ => return Err(format!("unrecognized headline parameter: \"{key}\"")),
        }
    }
    if !highlightall {
        if min_words >= max_words {
            return Err("MinWords should be less than MaxWords".into());
        }
        if min_words <= 0 {
            return Err("MinWords should be positive".into());
        }
        if shortword < 0 {
            return Err("ShortWord should be >= 0".into());
        }
        if max_fragments < 0 {
            return Err("MaxFragments should be >= 0".into());
        }
    }
    let (startsel, stopsel, fragdelim) =
        (startsel.unwrap_or("<b>".into()), stopsel.unwrap_or("</b>".into()), fragdelim.unwrap_or(" ... ".into()));
    // (Postgres keeps their lengths in 16 bits)
    for (name, v) in [("StartSel", &startsel), ("StopSel", &stopsel), ("FragmentDelimiter", &fragdelim)] {
        if v.len() > i16::MAX as usize {
            return Err(format!("value for \"{name}\" is too long"));
        }
    }
    let mut ops = vec![];
    if let Some(q) = query {
        operand_list(q, &mut ops);
    }
    let words = headline_words(cfg, doc, &ops);
    let mut hl = Hl { words, nops: ops.len(), query, locs: vec![], shortword, min_words, max_words, highlightall };
    if let Some(q) = query
        && !ops.is_empty()
    {
        hl.locs = locations(q, &hl.words, &mut 0).1;
    }
    if max_fragments == 0 {
        hl.select_words();
    } else {
        hl.select_fragments(max_fragments);
    }
    Ok(hl.render(&startsel, &stopsel, &fragdelim))
}
