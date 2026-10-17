//! XML values as text, with a parser (for well-formedness and for xpath) and an XPath 1.0 subset: paths with child,
//! descendant, parent, attribute, namespace and self steps, name tests and `*`, `text()`, `comment()`, `node()`,
//! predicates (a number, comparisons, `and`, `or`, `not()`, `last()`, `position()`), unions, and the common functions.
//! The parser reports what libxml2 reports, where it reports it, as Postgres shows it (see `detail`).

#[derive(Clone, Debug, PartialEq)]
pub enum Node {
    Elem { name: String, attrs: Vec<(String, String)>, kids: Vec<Node> },
    Text(String),
    CData(String),
    Comment(String),
    Pi(String, String),
}

/// A parse error as libxml reports it: its message and the byte the parser was at (an empty message: one Postgres
/// shows without a detail).
type Fault = (String, usize);

/// What an XML declaration says (`<?xml version="1.0" encoding="UTF-8" standalone="yes"?>`), as Postgres's
/// parse_xml_decl reads it: its length (0: there is none) and each part given (standalone 1 yes, 0 no, -1 not given).
pub struct Decl {
    pub len: usize,
    pub version: Option<String>,
    pub encoding: Option<String>,
    pub standalone: i8,
}

impl Default for Decl {
    fn default() -> Self {
        Decl { len: 0, version: None, encoding: None, standalone: -1 }
    }
}

/// parse_xml_decl: the declaration `s` starts with, or why it is malformed (errdetail_for_xml_code's words).
pub fn decl(s: &str) -> Result<Decl, &'static str> {
    const VERSION: &str = "Malformed declaration: missing version.";
    const ENCODING: &str = "Missing encoding in text declaration.";
    const STANDALONE: &str = "standalone accepts only 'yes' or 'no'.";
    const SPACE: &str = "Space required.";
    let mut d = Decl::default();
    if !s.starts_with("<?xml") {
        return Ok(d);
    }
    // (`<?xml-stylesheet ...?>` is a processing instruction, not a declaration)
    if s[5..].chars().next().is_some_and(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | ':')) {
        return Ok(d);
    }
    let b = s.as_bytes();
    let space = |p: usize| matches!(b.get(p), Some(b' ' | b'\t' | b'\n' | b'\r'));
    let skip = |mut p: usize| {
        while space(p) {
            p += 1;
        }
        p
    };
    // a quoted value at p: it, and the place after its closing quote
    let quoted = |p: usize| -> Option<(String, usize)> {
        let q = *b.get(p)?;
        if q != b'"' && q != b'\'' {
            return None;
        }
        let end = p + 1 + s[p + 1..].find(q as char)?;
        Some((s[p + 1..end].to_string(), end + 1))
    };
    let mut p = 5;
    if !space(p) {
        return Err(SPACE);
    }
    p = skip(p);
    if !s[p..].starts_with("version") {
        return Err(VERSION);
    }
    p = skip(p + 7);
    if b.get(p) != Some(&b'=') {
        return Err(VERSION);
    }
    let Some((v, next)) = quoted(skip(p + 1)) else { return Err(VERSION) };
    d.version = Some(v);
    p = next;
    let q = skip(p);
    if s[q..].starts_with("encoding") {
        if !space(p) {
            return Err(SPACE);
        }
        p = skip(q + 8);
        if b.get(p) != Some(&b'=') {
            return Err(ENCODING);
        }
        let Some((e, next)) = quoted(skip(p + 1)) else { return Err(ENCODING) };
        d.encoding = Some(e);
        p = next;
    }
    let q = skip(p);
    if s[q..].starts_with("standalone") {
        if !space(p) {
            return Err(SPACE);
        }
        p = skip(q + 10);
        if b.get(p) != Some(&b'=') {
            return Err(STANDALONE);
        }
        p = skip(p + 1);
        let r = &s[p..];
        if r.starts_with("'yes'") || r.starts_with("\"yes\"") {
            d.standalone = 1;
            p += 5;
        } else if r.starts_with("'no'") || r.starts_with("\"no\"") {
            d.standalone = 0;
            p += 4;
        } else {
            return Err(STANDALONE);
        }
    }
    p = skip(p);
    if !s[p..].starts_with("?>") {
        return Err("Parsing XML declaration: '?>' expected.");
    }
    p += 2;
    if !s[..p].is_ascii() {
        return Err("Invalid character value.");
    }
    d.len = p;
    Ok(d)
}

/// print_xml_decl: the declaration Postgres writes for a version and a standalone (1, 0, -1 none), which is none
/// for a plain 1.0 that says nothing of standalone.
pub fn decl_text(version: Option<&str>, standalone: i8) -> String {
    if !(version.is_some_and(|v| v != "1.0") || standalone != -1) {
        return String::new();
    }
    let s = match standalone {
        1 => " standalone=\"yes\"",
        0 => " standalone=\"no\"",
        _ => "",
    };
    format!("<?xml version=\"{}\"{s}?>", version.unwrap_or("1.0"))
}

/// An xml value as Postgres's output function writes it (xml_out): its declaration as `decl_text` writes it, and
/// where that is none, one newline after it gone.
pub fn out(s: &str) -> String {
    let Ok(d) = decl(s) else { return s.to_string() };
    let head = decl_text(d.version.as_deref(), d.standalone);
    let mut rest = &s[d.len..];
    if head.is_empty()
        && let Some(r) = rest.strip_prefix('\n')
    {
        rest = r;
    }
    head + rest
}

/// xml_doctype_in_content: whether content has a DOCTYPE after only blanks, comments and PIs (it is then read as a
/// document).
fn doctype_in_content(s: &str) -> bool {
    let mut p = s;
    loop {
        p = p.trim_start_matches([' ', '\t', '\n', '\r']);
        let Some(r) = p.strip_prefix('<') else { return false };
        if let Some(r) = r.strip_prefix('!') {
            if r.starts_with("DOCTYPE") {
                return true;
            }
            let Some(c) = r.strip_prefix("--") else { return false };
            let Some(e) = c.find("--") else { return false };
            if !c[e + 2..].starts_with('>') {
                return false;
            }
            p = &c[e + 3..];
            continue;
        }
        let Some(r) = r.strip_prefix('?') else { return false };
        let Some(e) = r.find("?>") else { return false };
        p = &r[e + 2..];
    }
}

/// A value as XML writes it: dates and times in ISO 8601 whatever DateStyle says (a time zone's offset with its
/// minutes), anything else as its text.
pub fn value_text(v: &super::Value) -> String {
    use super::Value;
    match v {
        Value::Date(_) | Value::Timestamp(_) => match v.to_json() {
            serde_json::Value::String(s) => s,
            _ => v.to_text(),
        },
        Value::TimestampTz(_) => {
            let s = v.to_text().replacen(' ', "T", 1);
            let b = s.as_bytes();
            let n = b.len();
            if n > 3 && (b[n - 3] == b'+' || b[n - 3] == b'-') && b[n - 2].is_ascii_digit() && b[n - 1].is_ascii_digit()
            {
                format!("{s}:00")
            } else {
                s
            }
        }
        v => v.to_text(),
    }
}

pub fn escape_text(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn escape_attr(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// map_sql_identifier_to_xml_name (not fully escaped): a name's characters an XML name can't have where they are as
/// `_xHHHH_`, a leading `:` and an `_` before `x` too.
pub fn ident_name(s: &str) -> String {
    let mut out = String::new();
    let cs: Vec<char> = s.chars().collect();
    for (i, &c) in cs.iter().enumerate() {
        let first = i == 0;
        if c == ':' && first {
            out.push_str("_x003A_");
        } else if c == '_' && cs.get(i + 1) == Some(&'x') {
            out.push_str("_x005F_");
        } else {
            let ok = c.is_alphabetic()
                || c == '_'
                || c == ':'
                || (!first && (c.is_numeric() || matches!(c, '.' | '-' | '\u{b7}') || c.is_alphanumeric()));
            if ok {
                out.push(c);
            } else {
                out.push_str(&format!("_x{:04X}_", c as u32));
            }
        }
    }
    out
}

struct P<'a> {
    s: &'a str,
    i: usize,
    /// the errors libxml reports and reads on after (an entity, an end tag that doesn't match), before the one that
    /// stops it, if any
    errs: Vec<Fault>,
    /// what it warns of (a relative namespace URI)
    warns: Vec<Fault>,
    /// the entities the DOCTYPE declares, and whether it names an external DTD (where any entity may be declared)
    ents: Vec<String>,
    external: bool,
    /// the DOCTYPE as written, and how many nodes came before it
    doctype: Option<(String, usize)>,
    /// namespaces are checked (xpath's parse): the prefixes each open element declares
    ns: Option<Vec<Vec<String>>>,
}

impl<'a> P<'a> {
    fn new(s: &'a str, ns: bool) -> Self {
        P { s, i: 0, errs: vec![], warns: vec![], ents: vec![], external: false, doctype: None, ns: ns.then(Vec::new) }
    }
    fn rest(&self) -> &'a str {
        let s: &'a str = self.s;
        &s[self.i..]
    }
    fn line(&self) -> usize {
        1 + self.s.as_bytes()[..self.i.min(self.s.len())].iter().filter(|b| **b == b'\n').count()
    }
    fn skip_ws(&mut self) {
        while self.rest().starts_with(|c: char| c.is_whitespace()) {
            self.i += self.rest().chars().next().map_or(1, char::len_utf8);
        }
    }
    fn name(&mut self) -> Option<String> {
        let r = self.rest();
        let end = r.find(|c: char| !(c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | ':'))).unwrap_or(r.len());
        if end == 0 || r.starts_with(|c: char| c.is_ascii_digit() || matches!(c, '-' | '.')) {
            return None;
        }
        self.i += end;
        Some(r[..end].to_string())
    }

    /// Text or an attribute's value (`raw`, found at `base`) with its references replaced. A reference libxml can't
    /// read is reported and passed over.
    fn unescape(&mut self, raw: &str, base: usize) -> Result<String, Fault> {
        let mut out = String::with_capacity(raw.len());
        let mut k = 0;
        while let Some(j) = raw[k..].find('&') {
            let amp = k + j;
            out.push_str(&raw[k..amp]);
            let tail = &raw[amp + 1..];
            if let Some(num) = tail.strip_prefix('#') {
                let end =
                    num.find(';').ok_or_else(|| ("xmlParseCharRef: invalid decimal value".to_string(), base + amp))?;
                let body = &num[..end];
                let code = match body.strip_prefix('x') {
                    Some(h) => u32::from_str_radix(h, 16).ok(),
                    None => body.parse::<u32>().ok(),
                };
                let Some(c) = code.and_then(char::from_u32).filter(|c| *c != '\0') else {
                    return Err((format!("xmlParseCharRef: invalid xmlChar value {}", code.unwrap_or(0)), base + amp));
                };
                out.push(c);
                k = amp + 2 + end + 1;
                continue;
            }
            let n =
                tail.find(|c: char| !(c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | ':'))).unwrap_or(tail.len());
            if n == 0 || tail.starts_with(|c: char| c.is_ascii_digit() || matches!(c, '-' | '.')) {
                self.errs.push(("xmlParseEntityRef: no name".into(), base + amp + 1));
                k = amp + 1;
                continue;
            }
            if !tail[n..].starts_with(';') {
                self.errs.push(("EntityRef: expecting ';'".into(), base + amp + 1 + n));
                k = amp + 1 + n;
                continue;
            }
            match &tail[..n] {
                "amp" => out.push('&'),
                "lt" => out.push('<'),
                "gt" => out.push('>'),
                "quot" => out.push('"'),
                "apos" => out.push('\''),
                // (one the DTD declares, or one an external DTD may: its text isn't read)
                e if self.external || self.ents.iter().any(|x| x == e) => {}
                e => self.errs.push((format!("Entity '{e}' not defined"), base + amp + n + 2)),
            }
            k = amp + n + 2;
        }
        out.push_str(&raw[k..]);
        Ok(out)
    }

    /// One construct of content where the parser is: a comment, a CDATA section, a PI, an element or text.
    fn one(&mut self, out: &mut Vec<Node>) -> Result<(), Fault> {
        let r = self.rest();
        if let Some(c) = r.strip_prefix("<!--") {
            let Some(e) = c.find("-->") else { return Err(("Comment not terminated".into(), self.i + 4)) };
            out.push(Node::Comment(c[..e].to_string()));
            self.i += 4 + e + 3;
        } else if let Some(c) = r.strip_prefix("<![CDATA[") {
            let Some(e) = c.find("]]>") else { return Err(("CData section not finished".into(), self.i + 9)) };
            out.push(Node::CData(c[..e].to_string()));
            self.i += 9 + e + 3;
        } else if let Some(c) = r.strip_prefix("<?") {
            let Some(e) = c.find("?>") else { return Err(("ParsePI: PI never end ...".into(), self.s.len())) };
            let body = &c[..e];
            let (target, data) = body.split_once(char::is_whitespace).unwrap_or((body, ""));
            // (the XML declaration is not a node)
            if !target.eq_ignore_ascii_case("xml") {
                out.push(Node::Pi(target.to_string(), data.trim_start().to_string()));
            }
            self.i += 2 + e + 2;
        } else if r.starts_with('<') {
            self.i += 1;
            let Some(name) = self.name() else { return Err(("StartTag: invalid element name".into(), self.i)) };
            let open = self.line();
            let mut attrs: Vec<(String, String)> = vec![];
            loop {
                self.skip_ws();
                let r = self.rest();
                let close = r.starts_with("/>");
                if close || r.starts_with('>') {
                    self.check_ns(&name, &attrs);
                    if close {
                        self.i += 2;
                        out.push(Node::Elem { name, attrs, kids: vec![] });
                    } else {
                        self.i += 1;
                        let kids = self.nodes(Some((&name, open)))?;
                        out.push(Node::Elem { name, attrs, kids });
                    }
                    if let Some(scopes) = &mut self.ns {
                        scopes.pop();
                    }
                    break;
                }
                let Some(an) = self.name() else {
                    return Err((format!("Couldn't find end of Start Tag {name} line {open}"), self.i));
                };
                self.skip_ws();
                if !self.rest().starts_with('=') {
                    return Err((format!("Specification mandates value for attribute {an}"), self.i));
                }
                self.i += 1;
                self.skip_ws();
                let Some(q) = self.rest().chars().next().filter(|c| *c == '"' || *c == '\'') else {
                    return Err(("AttValue: \" or ' expected".into(), self.i));
                };
                self.i += 1;
                let base = self.i;
                let r = self.rest();
                let Some(e) = r.find(q) else { return Err(("AttValue: ' expected".into(), self.s.len())) };
                if let Some(lt) = r[..e].find('<') {
                    return Err(("Unescaped '<' not allowed in attributes values".into(), base + lt));
                }
                let v = self.unescape(&r[..e], base)?;
                self.i += e + 1;
                if attrs.iter().any(|(n, _)| *n == an) {
                    self.errs.push((format!("Attribute {an} redefined"), self.i));
                    continue;
                }
                attrs.push((an, v));
            }
        } else {
            let e = r.find('<').unwrap_or(r.len());
            let t = self.unescape(&r[..e], self.i)?;
            if !t.is_empty() {
                out.push(Node::Text(t));
            }
            self.i += e;
        }
        Ok(())
    }

    /// In xpath's parse, at the end of a start tag: the namespaces it declares (a value that is no URI is an error, a
    /// relative one a warning) and that its name and attributes' prefixes are declared.
    fn check_ns(&mut self, name: &str, attrs: &[(String, String)]) {
        let Some(scopes) = &mut self.ns else { return };
        let at = self.i;
        let mut declared = vec![];
        for (k, v) in attrs {
            let Some(pre) = (if k == "xmlns" { Some("") } else { k.strip_prefix("xmlns:") }) else { continue };
            if v.contains(['<', '>', '"', ' ', '{', '}', '|', '\\', '^', '`']) {
                // (libxml's error, which Postgres shows with no detail)
                self.errs.push((String::new(), at));
            } else if !v.is_empty() && !v.contains(':') {
                self.warns.push((format!("xmlns: URI {v} is not absolute"), at));
            }
            declared.push(pre.to_string());
        }
        scopes.push(declared);
        let bound = |p: &str| p == "xml" || p == "xmlns" || scopes.iter().any(|s| s.iter().any(|d| d == p));
        if let Some((pre, local)) = name.split_once(':')
            && !bound(pre)
        {
            self.errs.push((format!("Namespace prefix {pre} on {local} is not defined"), at));
        }
        for (k, _) in attrs {
            if let Some((pre, local)) = k.split_once(':')
                && !bound(pre)
            {
                self.errs.push((format!("Namespace prefix {pre} for {local} on {name} is not defined"), at));
            }
        }
    }

    /// Content up to the end tag of `until` (its name and the line its start tag is on), or to the end.
    fn nodes(&mut self, until: Option<(&str, usize)>) -> Result<Vec<Node>, Fault> {
        let mut out = vec![];
        loop {
            let r = self.rest();
            if r.is_empty() {
                return match until {
                    Some((n, l)) => Err((format!("Premature end of data in tag {n} line {l}"), self.i)),
                    None => Ok(out),
                };
            }
            if r.starts_with("</") {
                let Some((n, l)) = until else { return Err(("chunk is not well balanced".into(), self.i)) };
                self.i += 2;
                let close = self.name().unwrap_or_default();
                self.skip_ws();
                if !self.rest().starts_with('>') {
                    return Err(("expected '>'".into(), self.i));
                }
                self.i += 1;
                if close != n {
                    self.errs.push((format!("Opening and ending tag mismatch: {n} line {l} and {close}"), self.i));
                }
                return Ok(out);
            }
            self.one(&mut out)?;
        }
    }

    /// A document after its declaration: one root element, with comments, PIs and blanks beside it and a DOCTYPE
    /// before it.
    fn document(&mut self) -> Result<Vec<Node>, Fault> {
        let mut out = vec![];
        let mut root = false;
        loop {
            self.skip_ws();
            let r = self.rest();
            if r.is_empty() {
                break;
            }
            if r.starts_with("<!--") || r.starts_with("<?") {
                self.one(&mut out)?;
            } else if !root && self.doctype.is_none() && r.starts_with("<!DOCTYPE") {
                self.doctype_decl(out.len())?;
            } else if !root && r.starts_with('<') && !r.starts_with("</") && !r.starts_with("<!") {
                self.one(&mut out)?;
                root = true;
            } else if root {
                return Err(("Extra content at the end of the document".into(), self.i));
            } else {
                return Err(("Start tag expected, '<' not found".into(), self.i));
            }
        }
        if !root {
            return Err(("Start tag expected, '<' not found".into(), self.i));
        }
        Ok(out)
    }

    /// `<!DOCTYPE name [external id] [[internal subset]]>`: the entities it declares, and whether it names a DTD of
    /// its own.
    fn doctype_decl(&mut self, before: usize) -> Result<(), Fault> {
        let (s, start) = (self.s, self.i);
        let b = s.as_bytes();
        let (mut k, mut quote, mut subset) = (start + "<!DOCTYPE".len(), None::<u8>, None::<(usize, Option<usize>)>);
        while k < b.len() {
            let c = b[k];
            match quote {
                Some(q) if c == q => quote = None,
                Some(_) => {}
                None if c == b'"' || c == b'\'' => quote = Some(c),
                None if c == b'[' && subset.is_none() => subset = Some((k + 1, None)),
                None if c == b']' && matches!(subset, Some((_, None))) => subset = subset.map(|(a, _)| (a, Some(k))),
                None if c == b'>' && !matches!(subset, Some((_, None))) => break,
                None => {}
            }
            k += 1;
        }
        if k >= b.len() {
            return Err(("DOCTYPE improperly terminated".into(), s.len()));
        }
        let head = &s[start + "<!DOCTYPE".len()..subset.map_or(k, |(a, _)| a - 1)];
        self.external = head.split_whitespace().any(|w| w == "SYSTEM" || w == "PUBLIC");
        if let Some((a, Some(z))) = subset {
            let mut rest = &s[a..z];
            while let Some(at) = rest.find("<!ENTITY") {
                rest = rest[at + "<!ENTITY".len()..].trim_start();
                // (a parameter entity, `<!ENTITY % name`, isn't one text can name)
                if !rest.starts_with('%') {
                    self.ents.push(rest.chars().take_while(|c| !c.is_whitespace()).collect());
                }
            }
        }
        self.doctype = Some((s[start..=k].to_string(), before));
        self.i = k + 1;
        Ok(())
    }
}

/// libxml's errors as Postgres's DETAIL shows them: each `line N: message`, then the line of input it was found on
/// and a `^` under the place (xmlParserPrintFileContext: at most 80 bytes of it).
fn detail(s: &str, faults: &[Fault]) -> String {
    let b = s.as_bytes();
    let byte = |k: usize| b.get(k).copied().unwrap_or(0);
    let nl = |c: u8| c == b'\n' || c == b'\r';
    let mut out = vec![];
    for (m, at) in faults.iter().filter(|(m, _)| !m.is_empty()) {
        let at = (*at).min(b.len());
        let mut cur = at;
        while cur > 0 && nl(byte(cur)) {
            cur -= 1;
        }
        let mut n = 0;
        while n < 80 && cur > 0 && !nl(byte(cur)) {
            cur -= 1;
            n += 1;
        }
        if nl(byte(cur)) {
            cur += 1;
        }
        let col = at.saturating_sub(cur);
        let mut end = cur;
        while end < b.len() && !nl(b[end]) && end - cur < 80 {
            end += 1;
        }
        let content = &b[cur.min(end)..end];
        let mut caret: Vec<u8> =
            content.iter().take(col.min(79)).map(|c| if *c == b'\t' { b'\t' } else { b' ' }).collect();
        caret.push(b'^');
        let line = 1 + b[..at].iter().filter(|c| **c == b'\n').count();
        out.push(format!(
            "line {line}: {m}\n{}\n{}",
            String::from_utf8_lossy(content),
            String::from_utf8_lossy(&caret)
        ));
    }
    out.join("\n")
}

/// Why a text isn't XML: its XML declaration is malformed (errdetail_for_xml_code's words), or what libxml reports, as
/// Postgres's DETAIL shows it (empty: no detail).
pub enum Invalid {
    Decl(&'static str),
    Errors(String),
}

/// A text read as XML: its nodes, whether it was read as a document, its DOCTYPE (and how many nodes come before
/// it), and the warnings libxml gave (as Postgres's WARNING shows them; empty: none).
pub struct Read {
    pub nodes: Vec<Node>,
    pub document: bool,
    doctype: Option<(String, usize)>,
    pub warnings: String,
}

/// Postgres's xml_parse: `s` read as a document, or as content (where a DOCTYPE after only comments and PIs makes it
/// read as a document, and the XML declaration is read first, places then counting from after it). `ns`: namespaces
/// are checked too, as xpath's parse does.
pub fn read(s: &str, document: bool, ns: bool) -> Result<Read, Invalid> {
    let (text, as_doc) = if document {
        (s, true)
    } else {
        let d = decl(s).map_err(Invalid::Decl)?;
        let after = &s[d.len..];
        if doctype_in_content(after) { (s, true) } else { (after, false) }
    };
    let mut p = P::new(text, ns);
    let got = if as_doc {
        p.i = decl(text).map_or(0, |d| d.len);
        p.document()
    } else {
        p.nodes(None)
    };
    let mut errs = std::mem::take(&mut p.errs);
    let nodes = match got {
        Ok(n) => n,
        Err(f) => {
            errs.push(f);
            vec![]
        }
    };
    if !errs.is_empty() {
        return Err(Invalid::Errors(detail(text, &errs)));
    }
    Ok(Read { nodes, document: as_doc, doctype: p.doctype.take(), warnings: detail(text, &p.warns) })
}

pub fn text_of(n: &Node) -> String {
    match n {
        Node::Elem { kids, .. } => kids.iter().map(text_of).collect(),
        Node::Text(t) | Node::CData(t) => t.clone(),
        Node::Comment(_) | Node::Pi(..) => String::new(),
    }
}

pub fn serialize(n: &Node) -> String {
    match n {
        Node::Elem { name, attrs, kids } => {
            let a: String = attrs.iter().map(|(k, v)| format!(" {k}=\"{}\"", escape_attr(v))).collect();
            if kids.is_empty() {
                format!("<{name}{a}/>")
            } else {
                format!("<{name}{a}>{}</{name}>", kids.iter().map(serialize).collect::<String>())
            }
        }
        Node::Text(t) => escape_text(t),
        Node::CData(t) => format!("<![CDATA[{t}]]>"),
        Node::Comment(c) => format!("<!--{c}-->"),
        Node::Pi(t, d) => {
            if d.is_empty() {
                format!("<?{t}?>")
            } else {
                format!("<?{t} {d}?>")
            }
        }
    }
}

/// The nodes as libxml keeps them reading without blanks (XML_PARSE_NOBLANKS, areBlanks): whitespace among an
/// element's children goes, unless it is all the element holds or text came before it.
fn strip_blanks(nodes: Vec<Node>) -> Vec<Node> {
    nodes
        .into_iter()
        .map(|n| match n {
            Node::Elem { name, attrs, kids } => {
                let last = kids.len().saturating_sub(1);
                let mut kept: Vec<Node> = vec![];
                for (i, k) in kids.into_iter().enumerate() {
                    let blank = matches!(&k, Node::Text(t) if t.chars().all(|c| matches!(c, ' ' | '\t' | '\n' | '\r')));
                    let keep = !blank
                        || (kept.is_empty() && i == last)
                        || matches!(kept.last(), Some(Node::Text(_)))
                        || matches!(kept.first(), Some(Node::Text(_)));
                    if keep {
                        kept.push(k);
                    }
                }
                Node::Elem { name, attrs, kids: strip_blanks(kept) }
            }
            n => n,
        })
        .collect()
}

/// A node as libxml writes it formatted (XML_SAVE_FORMAT) at `level`: an element's children each on a line of their
/// own, two spaces in a level, unless one of them is text.
fn dump(n: &Node, level: usize, format: bool, out: &mut String) {
    let Node::Elem { name, attrs, kids } = n else {
        out.push_str(&serialize(n));
        return;
    };
    out.push('<');
    out.push_str(name);
    for (k, v) in attrs {
        out.push_str(&format!(" {k}=\"{}\"", escape_attr(v)));
    }
    if kids.is_empty() {
        out.push_str("/>");
        return;
    }
    let format = format && !kids.iter().any(|k| matches!(k, Node::Text(_) | Node::CData(_)));
    out.push('>');
    if format {
        out.push('\n');
    }
    for k in kids {
        if format && matches!(k, Node::Elem { .. } | Node::Comment(_) | Node::Pi(..)) {
            out.push_str(&"  ".repeat((level + 1).min(30)));
        }
        dump(k, level + 1, format, out);
        if format {
            out.push('\n');
        }
    }
    if format {
        out.push_str(&"  ".repeat(level.min(30)));
    }
    out.push_str("</");
    out.push_str(name);
    out.push('>');
}

/// XMLSERIALIZE(... INDENT), as Postgres's xmltotext_with_options writes it: the value read without blanks and written
/// formatted. A document (or content with a DOCTYPE) keeps an XML declaration it had and ends its parts with newlines
/// (cut at the end when DOCUMENT was asked for); content's nodes are on lines of their own, text beside what is before
/// it. None: it isn't a document (or isn't XML).
pub fn indent(s: &str, document: bool) -> Option<String> {
    let r = read(s, document, false).ok()?;
    let nodes = strip_blanks(r.nodes);
    let mut out = String::new();
    if !r.document {
        for (k, n) in nodes.iter().enumerate() {
            if k > 0 && !matches!(n, Node::Text(_)) {
                out.push('\n');
            }
            dump(n, 0, true, &mut out);
        }
        return Some(out);
    }
    // (the declaration only where it had one: XML_SAVE_NO_DECL otherwise)
    let d = decl(s).unwrap_or_default();
    if d.len > 0 {
        out.push_str(&format!("<?xml version=\"{}\"", d.version.as_deref().unwrap_or("1.0")));
        if let Some(e) = &d.encoding {
            out.push_str(&format!(" encoding=\"{e}\""));
        }
        match d.standalone {
            1 => out.push_str(" standalone=\"yes\""),
            0 => out.push_str(" standalone=\"no\""),
            _ => {}
        }
        out.push_str("?>\n");
    }
    for (k, n) in nodes.iter().enumerate() {
        if let Some((dt, at)) = &r.doctype
            && *at == k
        {
            out.push_str(dt);
            out.push('\n');
        }
        dump(n, 0, true, &mut out);
        out.push('\n');
    }
    if document {
        while out.ends_with(['\n', '\r']) {
            out.pop();
        }
    }
    Some(out)
}

// ---------------------------------------------------------------- XPath

/// A node of the tree in a path: where it is, so its parent and siblings can be found.
#[derive(Clone, Debug)]
enum Ref {
    Root,
    Node(Vec<usize>),
    /// an attribute: its element's path and its place
    Attr(Vec<usize>, usize),
}

struct Doc {
    top: Vec<Node>,
}

impl Doc {
    fn get(&self, path: &[usize]) -> Option<&Node> {
        let mut cur: Option<&Node> = None;
        let mut kids: &[Node] = &self.top;
        for &i in path {
            let n = kids.get(i)?;
            cur = Some(n);
            kids = match n {
                Node::Elem { kids, .. } => kids,
                _ => &[],
            };
        }
        cur
    }

    fn children(&self, r: &Ref) -> Vec<Ref> {
        let (base, kids): (Vec<usize>, &[Node]) = match r {
            Ref::Root => (vec![], &self.top),
            Ref::Node(p) => match self.get(p) {
                Some(Node::Elem { kids, .. }) => (p.clone(), kids),
                _ => return vec![],
            },
            Ref::Attr(..) => return vec![],
        };
        (0..kids.len())
            .map(|i| {
                let mut p = base.clone();
                p.push(i);
                Ref::Node(p)
            })
            .collect()
    }

    fn descendants(&self, r: &Ref, out: &mut Vec<Ref>) {
        for c in self.children(r) {
            out.push(c.clone());
            self.descendants(&c, out);
        }
    }

    fn parent(&self, r: &Ref) -> Option<Ref> {
        match r {
            Ref::Root => None,
            Ref::Node(p) if p.len() <= 1 => Some(Ref::Root),
            Ref::Node(p) => Some(Ref::Node(p[..p.len() - 1].to_vec())),
            Ref::Attr(p, _) => Some(if p.is_empty() { Ref::Root } else { Ref::Node(p.clone()) }),
        }
    }

    fn name(&self, r: &Ref) -> String {
        match r {
            Ref::Node(p) => match self.get(p) {
                Some(Node::Elem { name, .. }) => name.clone(),
                Some(Node::Pi(t, _)) => t.clone(),
                _ => String::new(),
            },
            Ref::Attr(p, i) => match self.get(p) {
                Some(Node::Elem { attrs, .. }) => attrs.get(*i).map(|a| a.0.clone()).unwrap_or_default(),
                _ => String::new(),
            },
            Ref::Root => String::new(),
        }
    }

    fn string_value(&self, r: &Ref) -> String {
        match r {
            Ref::Root => self.top.iter().map(text_of).collect(),
            Ref::Node(p) => self.get(p).map(text_of).unwrap_or_default(),
            Ref::Attr(p, i) => match self.get(p) {
                Some(Node::Elem { attrs, .. }) => attrs.get(*i).map(|a| a.1.clone()).unwrap_or_default(),
                _ => String::new(),
            },
        }
    }

    fn output(&self, r: &Ref) -> String {
        match r {
            Ref::Root => self.top.iter().map(serialize).collect(),
            // (an element declares the namespaces it and what is in it use that its ancestors declared, as the copy
            // Postgres writes, libxml's xmlCopyNode, does)
            Ref::Node(p) => match self.get(p) {
                Some(n @ Node::Elem { .. }) => {
                    let mut extra = vec![];
                    self.used_ns(p, p, &mut vec![], &mut extra);
                    serialize_with(n, &extra)
                }
                Some(n) => serialize(n),
                None => String::new(),
            },
            // an attribute (and a namespace) is its value
            Ref::Attr(..) => escape_text(&self.string_value(r)),
        }
    }

    /// The namespace a prefix ("" the default) stands for at the element at `path`, by its and its ancestors' xmlns
    /// attributes (None: none).
    fn ns_at(&self, path: &[usize], prefix: &str) -> Option<String> {
        if prefix == "xml" {
            return Some("http://www.w3.org/XML/1998/namespace".into());
        }
        let key = if prefix.is_empty() { "xmlns".to_string() } else { format!("xmlns:{prefix}") };
        for k in (1..=path.len()).rev() {
            if let Some(Node::Elem { attrs, .. }) = self.get(&path[..k])
                && let Some((_, v)) = attrs.iter().find(|(n, _)| *n == key)
            {
                return (!v.is_empty()).then(|| v.clone());
            }
        }
        None
    }

    /// The prefixes the element at `path` (inside the one at `root`) and what is inside it use that nothing from
    /// `root` down to them declares: each with the namespace `root`'s ancestors give it.
    fn used_ns(&self, root: &[usize], path: &[usize], scope: &mut Vec<String>, out: &mut Vec<(String, String)>) {
        let Some(Node::Elem { name, attrs, kids }) = self.get(path) else { return };
        let mark = scope.len();
        for (k, _) in attrs {
            if k == "xmlns" {
                scope.push(String::new());
            } else if let Some(p) = k.strip_prefix("xmlns:") {
                scope.push(p.to_string());
            }
        }
        let prefix = |n: &str| n.split_once(':').map_or(String::new(), |(p, _)| p.to_string());
        let mut used = vec![prefix(name.as_str())];
        used.extend(attrs.iter().filter(|(k, _)| k != "xmlns" && k.contains(':')).map(|(k, _)| prefix(k.as_str())));
        for p in used {
            if p == "xml" || p == "xmlns" || scope.contains(&p) || out.iter().any(|(q, _)| *q == p) {
                continue;
            }
            if let Some(uri) = self.ns_at(root, &p) {
                out.push((p, uri));
            }
        }
        for (i, k) in kids.iter().enumerate() {
            if matches!(k, Node::Elem { .. }) {
                let mut q = path.to_vec();
                q.push(i);
                self.used_ns(root, &q, scope, out);
            }
        }
        scope.truncate(mark);
    }
}

/// An element written with namespace declarations added (after its own, before its attributes, as libxml writes
/// them).
fn serialize_with(n: &Node, extra: &[(String, String)]) -> String {
    let Node::Elem { name, attrs, kids } = n else { return serialize(n) };
    if extra.is_empty() {
        return serialize(n);
    }
    let is_ns = |k: &str| k == "xmlns" || k.starts_with("xmlns:");
    let mut all: Vec<(String, String)> = attrs.iter().filter(|(k, _)| is_ns(k)).cloned().collect();
    all.extend(
        extra.iter().map(|(p, u)| (if p.is_empty() { "xmlns".to_string() } else { format!("xmlns:{p}") }, u.clone())),
    );
    all.extend(attrs.iter().filter(|(k, _)| !is_ns(k)).cloned());
    serialize(&Node::Elem { name: name.clone(), attrs: all, kids: kids.clone() })
}

#[derive(Clone, Debug)]
enum Val {
    Nodes(Vec<Ref>),
    Str(String),
    Num(f64),
    Bool(bool),
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Slash,
    DSlash,
    Dot,
    DotDot,
    At,
    Star,
    LParen,
    RParen,
    LBrack,
    RBrack,
    Comma,
    Pipe,
    Op(String),
    Name(String),
    Str(String),
    Num(f64),
    Axis(String),
}

fn lex(s: &str) -> Result<Vec<Tok>, String> {
    let c: Vec<char> = s.chars().collect();
    let (mut out, mut i) = (vec![], 0);
    while i < c.len() {
        let ch = c[i];
        match ch {
            ' ' | '\t' | '\n' | '\r' => i += 1,
            '/' if c.get(i + 1) == Some(&'/') => {
                out.push(Tok::DSlash);
                i += 2;
            }
            '/' => {
                out.push(Tok::Slash);
                i += 1;
            }
            '.' if c.get(i + 1) == Some(&'.') => {
                out.push(Tok::DotDot);
                i += 2;
            }
            '.' if c.get(i + 1).is_some_and(char::is_ascii_digit) => {
                let st = i;
                i += 1;
                while i < c.len() && c[i].is_ascii_digit() {
                    i += 1;
                }
                out.push(Tok::Num(c[st..i].iter().collect::<String>().parse().map_err(|_| "Invalid expression")?));
            }
            '.' => {
                out.push(Tok::Dot);
                i += 1;
            }
            '@' => {
                out.push(Tok::At);
                i += 1;
            }
            '*' => {
                out.push(Tok::Star);
                i += 1;
            }
            '(' => {
                out.push(Tok::LParen);
                i += 1;
            }
            ')' => {
                out.push(Tok::RParen);
                i += 1;
            }
            '[' => {
                out.push(Tok::LBrack);
                i += 1;
            }
            ']' => {
                out.push(Tok::RBrack);
                i += 1;
            }
            ',' => {
                out.push(Tok::Comma);
                i += 1;
            }
            '|' => {
                out.push(Tok::Pipe);
                i += 1;
            }
            '+' | '-' | '=' => {
                out.push(Tok::Op(ch.to_string()));
                i += 1;
            }
            '!' if c.get(i + 1) == Some(&'=') => {
                out.push(Tok::Op("!=".into()));
                i += 2;
            }
            '<' | '>' => {
                if c.get(i + 1) == Some(&'=') {
                    out.push(Tok::Op(format!("{ch}=")));
                    i += 2;
                } else {
                    out.push(Tok::Op(ch.to_string()));
                    i += 1;
                }
            }
            '"' | '\'' => {
                let st = i + 1;
                i += 1;
                while i < c.len() && c[i] != ch {
                    i += 1;
                }
                if i >= c.len() {
                    return Err("Unfinished literal".into());
                }
                out.push(Tok::Str(c[st..i].iter().collect()));
                i += 1;
            }
            d if d.is_ascii_digit() => {
                let st = i;
                while i < c.len() && (c[i].is_ascii_digit() || c[i] == '.') {
                    i += 1;
                }
                out.push(Tok::Num(c[st..i].iter().collect::<String>().parse().map_err(|_| "Invalid expression")?));
            }
            n if n.is_alphabetic() || n == '_' => {
                let st = i;
                while i < c.len()
                    && (c[i].is_alphanumeric() || matches!(c[i], '_' | '-' | '.' | ':'))
                    && !(c[i] == ':' && c.get(i + 1) == Some(&':'))
                {
                    i += 1;
                }
                let w: String = c[st..i].iter().collect();
                if c.get(i) == Some(&':') && c.get(i + 1) == Some(&':') {
                    i += 2;
                    out.push(Tok::Axis(w));
                } else {
                    out.push(Tok::Name(w));
                }
            }
            _ => return Err("Invalid expression".into()),
        }
    }
    Ok(out)
}

struct X<'a> {
    doc: &'a Doc,
    t: Vec<Tok>,
    i: usize,
    /// the namespaces the call registers: each prefix and its URI
    ns: &'a [(String, String)],
}

fn to_str(doc: &Doc, v: &Val) -> String {
    match v {
        Val::Str(s) => s.clone(),
        Val::Num(n) => num_text(*n),
        Val::Bool(b) => b.to_string(),
        Val::Nodes(n) => n.first().map(|r| doc.string_value(r)).unwrap_or_default(),
    }
}

fn num_text(n: f64) -> String {
    if n.is_nan() {
        "NaN".into()
    } else if n.is_infinite() {
        if n > 0.0 { "Infinity" } else { "-Infinity" }.into()
    } else if n == n.trunc() && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

fn to_num(doc: &Doc, v: &Val) -> f64 {
    match v {
        Val::Num(n) => *n,
        Val::Bool(b) => f64::from(u8::from(*b)),
        v => to_str(doc, v).trim().parse().unwrap_or(f64::NAN),
    }
}

fn to_bool(v: &Val) -> bool {
    match v {
        Val::Bool(b) => *b,
        Val::Num(n) => *n != 0.0 && !n.is_nan(),
        Val::Str(s) => !s.is_empty(),
        Val::Nodes(n) => !n.is_empty(),
    }
}

impl X<'_> {
    fn peek(&self) -> Option<&Tok> {
        self.t.get(self.i)
    }
    fn eat(&mut self, t: &Tok) -> bool {
        let yes = self.peek() == Some(t);
        self.i += usize::from(yes);
        yes
    }
    fn word(&mut self, w: &str) -> bool {
        let yes = matches!(self.peek(), Some(Tok::Name(n)) if n == w);
        self.i += usize::from(yes);
        yes
    }

    fn or(&mut self, ctx: &Ref, pos: usize, size: usize) -> Result<Val, String> {
        let mut l = self.and(ctx, pos, size)?;
        while self.word("or") {
            let r = self.and(ctx, pos, size)?;
            l = Val::Bool(to_bool(&l) || to_bool(&r));
        }
        Ok(l)
    }
    fn and(&mut self, ctx: &Ref, pos: usize, size: usize) -> Result<Val, String> {
        let mut l = self.eq(ctx, pos, size)?;
        while self.word("and") {
            let r = self.eq(ctx, pos, size)?;
            l = Val::Bool(to_bool(&l) && to_bool(&r));
        }
        Ok(l)
    }
    fn compare(&self, op: &str, a: &Val, b: &Val) -> bool {
        // node sets compare through their string values, any pair
        let strs = |v: &Val| -> Vec<String> {
            match v {
                Val::Nodes(n) => n.iter().map(|r| self.doc.string_value(r)).collect(),
                v => vec![to_str(self.doc, v)],
            }
        };
        let numeric = matches!(a, Val::Num(_)) || matches!(b, Val::Num(_)) || matches!(op, "<" | ">" | "<=" | ">=");
        let boolean = matches!(a, Val::Bool(_)) || matches!(b, Val::Bool(_));
        if boolean && matches!(op, "=" | "!=") {
            let eq = to_bool(a) == to_bool(b);
            return if op == "=" { eq } else { !eq };
        }
        for x in strs(a) {
            for y in strs(b) {
                let ok = if numeric {
                    let (p, q) =
                        (x.trim().parse::<f64>().unwrap_or(f64::NAN), y.trim().parse::<f64>().unwrap_or(f64::NAN));
                    match op {
                        "=" => p == q,
                        "!=" => p != q,
                        "<" => p < q,
                        ">" => p > q,
                        "<=" => p <= q,
                        _ => p >= q,
                    }
                } else if op == "=" {
                    x == y
                } else {
                    x != y
                };
                if ok {
                    return true;
                }
            }
        }
        false
    }
    fn eq(&mut self, ctx: &Ref, pos: usize, size: usize) -> Result<Val, String> {
        let mut l = self.add(ctx, pos, size)?;
        while let Some(Tok::Op(op)) = self.peek().cloned() {
            if !matches!(op.as_str(), "=" | "!=" | "<" | ">" | "<=" | ">=") {
                break;
            }
            self.i += 1;
            let r = self.add(ctx, pos, size)?;
            l = Val::Bool(self.compare(&op, &l, &r));
        }
        Ok(l)
    }
    fn add(&mut self, ctx: &Ref, pos: usize, size: usize) -> Result<Val, String> {
        let mut l = self.mul(ctx, pos, size)?;
        while let Some(Tok::Op(op)) = self.peek().cloned() {
            if op != "+" && op != "-" {
                break;
            }
            self.i += 1;
            let r = self.mul(ctx, pos, size)?;
            let (a, b) = (to_num(self.doc, &l), to_num(self.doc, &r));
            l = Val::Num(if op == "+" { a + b } else { a - b });
        }
        Ok(l)
    }
    fn mul(&mut self, ctx: &Ref, pos: usize, size: usize) -> Result<Val, String> {
        let mut l = self.union(ctx, pos, size)?;
        loop {
            let op = if self.eat(&Tok::Star) {
                "*"
            } else if self.word("div") {
                "/"
            } else if self.word("mod") {
                "%"
            } else {
                break;
            };
            let r = self.union(ctx, pos, size)?;
            let (a, b) = (to_num(self.doc, &l), to_num(self.doc, &r));
            l = Val::Num(match op {
                "*" => a * b,
                "/" => a / b,
                _ => a % b,
            });
        }
        Ok(l)
    }
    fn union(&mut self, ctx: &Ref, pos: usize, size: usize) -> Result<Val, String> {
        let l = self.unary(ctx, pos, size)?;
        if self.peek() != Some(&Tok::Pipe) {
            return Ok(l);
        }
        let mut all = match l {
            Val::Nodes(n) => n,
            _ => return Err("Invalid type".into()),
        };
        while self.eat(&Tok::Pipe) {
            match self.unary(ctx, pos, size)? {
                Val::Nodes(n) => all.extend(n),
                _ => return Err("Invalid type".into()),
            }
        }
        Ok(Val::Nodes(self.sorted(all)))
    }
    fn unary(&mut self, ctx: &Ref, pos: usize, size: usize) -> Result<Val, String> {
        if matches!(self.peek(), Some(Tok::Op(o)) if o == "-") {
            self.i += 1;
            let v = self.unary(ctx, pos, size)?;
            return Ok(Val::Num(-to_num(self.doc, &v)));
        }
        self.path(ctx, pos, size)
    }

    /// Document order, without repeats.
    fn sorted(&self, mut v: Vec<Ref>) -> Vec<Ref> {
        let key = |r: &Ref| -> (Vec<usize>, usize) {
            match r {
                Ref::Root => (vec![], 0),
                Ref::Node(p) => (p.clone(), 0),
                Ref::Attr(p, i) => (p.clone(), i + 1),
            }
        };
        v.sort_by_key(key);
        v.dedup_by_key(|r| key(r));
        v
    }

    fn path(&mut self, ctx: &Ref, pos: usize, size: usize) -> Result<Val, String> {
        let mut cur: Vec<Ref>;
        match self.peek().cloned() {
            Some(Tok::Slash) => {
                self.i += 1;
                cur = vec![Ref::Root];
                // a lone `/`
                if !self.step_next() {
                    return Ok(Val::Nodes(cur));
                }
            }
            Some(Tok::DSlash) => {
                self.i += 1;
                let mut all = vec![Ref::Root];
                self.doc.descendants(&Ref::Root, &mut all);
                cur = all;
            }
            Some(Tok::Num(n)) => {
                self.i += 1;
                return Ok(Val::Num(n));
            }
            Some(Tok::Str(s)) => {
                self.i += 1;
                return Ok(Val::Str(s));
            }
            Some(Tok::LParen) => {
                self.i += 1;
                let v = self.or(ctx, pos, size)?;
                if !self.eat(&Tok::RParen) {
                    return Err("Invalid expression".into());
                }
                return Ok(v);
            }
            Some(Tok::Name(n))
                if self.t.get(self.i + 1) == Some(&Tok::LParen)
                    && !matches!(n.as_str(), "text" | "comment" | "node" | "processing-instruction") =>
            {
                self.i += 2;
                return self.call(&n, ctx, pos, size);
            }
            _ => cur = vec![ctx.clone()],
        }
        // steps
        let mut first = true;
        loop {
            if !first {
                if self.eat(&Tok::Slash) {
                } else if self.eat(&Tok::DSlash) {
                    let mut all = vec![];
                    for r in &cur {
                        all.push(r.clone());
                        self.doc.descendants(r, &mut all);
                    }
                    cur = self.sorted(all);
                } else {
                    break;
                }
            }
            first = false;
            if !self.step_next() {
                break;
            }
            cur = self.step(cur, ctx)?;
        }
        Ok(Val::Nodes(self.sorted(cur)))
    }

    /// Whether a step starts here.
    fn step_next(&self) -> bool {
        matches!(self.peek(), Some(Tok::Name(_) | Tok::Star | Tok::At | Tok::Dot | Tok::DotDot | Tok::Axis(_)))
    }

    fn step(&mut self, from: Vec<Ref>, _ctx: &Ref) -> Result<Vec<Ref>, String> {
        // the axis and node test
        let mut axis = "child".to_string();
        match self.peek().cloned() {
            Some(Tok::Dot) => {
                self.i += 1;
                return self.preds(from, "self", &Test::Any);
            }
            Some(Tok::DotDot) => {
                self.i += 1;
                return self.preds(from, "parent", &Test::Any);
            }
            Some(Tok::At) => {
                self.i += 1;
                axis = "attribute".into();
            }
            Some(Tok::Axis(a)) => {
                self.i += 1;
                axis = a;
            }
            _ => {}
        }
        let test = match self.peek().cloned() {
            Some(Tok::Star) => {
                self.i += 1;
                Test::Star
            }
            Some(Tok::Name(n)) => {
                self.i += 1;
                if self.t.get(self.i) == Some(&Tok::LParen)
                    && matches!(n.as_str(), "text" | "comment" | "node" | "processing-instruction")
                {
                    self.i += 1;
                    if !self.eat(&Tok::RParen) {
                        if let Some(Tok::Str(_)) = self.peek() {
                            self.i += 1;
                        }
                        self.eat(&Tok::RParen);
                    }
                    match n.as_str() {
                        "text" => Test::Text,
                        "comment" => Test::Comment,
                        "processing-instruction" => Test::Pi,
                        _ => Test::Any,
                    }
                } else {
                    if let Some((p, _)) = n.split_once(':')
                        && p != "xml"
                        && !self.ns.iter().any(|(q, _)| q == p)
                    {
                        return Err("Undefined namespace prefix".into());
                    }
                    Test::Name(n)
                }
            }
            _ => return Err("Invalid expression".into()),
        };
        self.preds(from, &axis, &test)
    }

    fn preds(&mut self, from: Vec<Ref>, axis: &str, test: &Test) -> Result<Vec<Ref>, String> {
        // the predicates' tokens are read once per context node
        let start = self.i;
        let mut end = start;
        let mut out = vec![];
        for r in &from {
            let mut cands: Vec<Ref> = match axis {
                "child" => self.doc.children(r),
                "descendant" => {
                    let mut v = vec![];
                    self.doc.descendants(r, &mut v);
                    v
                }
                "descendant-or-self" => {
                    let mut v = vec![r.clone()];
                    self.doc.descendants(r, &mut v);
                    v
                }
                "self" => vec![r.clone()],
                "parent" => self.doc.parent(r).into_iter().collect(),
                "ancestor" | "ancestor-or-self" => {
                    let mut v = if axis == "ancestor-or-self" { vec![r.clone()] } else { vec![] };
                    let mut c = r.clone();
                    while let Some(p) = self.doc.parent(&c) {
                        v.push(p.clone());
                        c = p;
                    }
                    v
                }
                // (a namespace declaration is no attribute)
                "attribute" => match r {
                    Ref::Node(p) => match self.doc.get(p) {
                        Some(Node::Elem { attrs, .. }) => attrs
                            .iter()
                            .enumerate()
                            .filter(|(_, (n, _))| n != "xmlns" && !n.starts_with("xmlns:"))
                            .map(|(i, _)| Ref::Attr(p.clone(), i))
                            .collect(),
                        _ => vec![],
                    },
                    _ => vec![],
                },
                // the namespaces in scope: each prefix's nearest declaration, a node of its URI
                "namespace" => match r {
                    Ref::Node(p) if matches!(self.doc.get(p), Some(Node::Elem { .. })) => {
                        let (mut seen, mut v) = (Vec::<String>::new(), vec![]);
                        for k in (1..=p.len()).rev() {
                            let Some(Node::Elem { attrs, .. }) = self.doc.get(&p[..k]) else { continue };
                            for (i, (n, val)) in attrs.iter().enumerate() {
                                let pre = if n == "xmlns" { Some("") } else { n.strip_prefix("xmlns:") };
                                if let Some(pre) = pre
                                    && !seen.iter().any(|s| s == pre)
                                {
                                    seen.push(pre.to_string());
                                    if !val.is_empty() {
                                        v.push(Ref::Attr(p[..k].to_vec(), i));
                                    }
                                }
                            }
                        }
                        v
                    }
                    _ => vec![],
                },
                "following-sibling" | "preceding-sibling" => match self.doc.parent(r) {
                    Some(par) => {
                        let sibs = self.doc.children(&par);
                        let at = sibs.iter().position(|s| same(s, r)).unwrap_or(0);
                        if axis == "following-sibling" {
                            sibs[at + 1..].to_vec()
                        } else {
                            sibs[..at].iter().rev().cloned().collect()
                        }
                    }
                    None => vec![],
                },
                a => return Err(format!("Unsupported axis {a}")),
            };
            cands.retain(|c| self.matches(c, test, axis));
            self.i = start;
            // predicates, each narrowing the list
            while self.eat(&Tok::LBrack) {
                let mut kept = vec![];
                let size = cands.len();
                let at = self.i;
                for (k, c) in cands.iter().enumerate() {
                    self.i = at;
                    let v = self.or(c, k + 1, size)?;
                    let ok = match v {
                        Val::Num(n) => (n - (k + 1) as f64).abs() < 1e-9,
                        v => to_bool(&v),
                    };
                    if ok {
                        kept.push(c.clone());
                    }
                }
                if cands.is_empty() {
                    // (skip the predicate's tokens)
                    let mut depth = 1;
                    while depth > 0 {
                        match self.peek() {
                            Some(Tok::LBrack) => depth += 1,
                            Some(Tok::RBrack) => depth -= 1,
                            None => return Err("Invalid expression".into()),
                            _ => {}
                        }
                        self.i += 1;
                    }
                    self.i -= 1;
                } else {
                    self.i = self.skip_to_close(at)?;
                }
                if !self.eat(&Tok::RBrack) {
                    return Err("Invalid predicate".into());
                }
                cands = kept;
            }
            end = self.i;
            out.extend(cands);
        }
        if from.is_empty() {
            // read the predicates once, to move past them
            self.i = start;
            while self.eat(&Tok::LBrack) {
                let mut depth = 1;
                while depth > 0 {
                    match self.peek() {
                        Some(Tok::LBrack) => depth += 1,
                        Some(Tok::RBrack) => depth -= 1,
                        None => return Err("Invalid expression".into()),
                        _ => {}
                    }
                    self.i += 1;
                }
            }
            end = self.i;
        }
        self.i = end;
        Ok(out)
    }

    /// The place of the `]` that closes the predicate whose tokens begin at `at`.
    fn skip_to_close(&self, at: usize) -> Result<usize, String> {
        let (mut depth, mut i) = (1, at);
        while i < self.t.len() {
            match self.t[i] {
                Tok::LBrack => depth += 1,
                Tok::RBrack => {
                    depth -= 1;
                    if depth == 0 {
                        return Ok(i);
                    }
                }
                _ => {}
            }
            i += 1;
        }
        Err("Invalid predicate".into())
    }

    fn matches(&self, r: &Ref, test: &Test, axis: &str) -> bool {
        match (test, r) {
            (Test::Any, _) => true,
            (Test::Star, Ref::Attr(..)) => axis == "attribute" || axis == "namespace",
            (Test::Star, Ref::Node(p)) => {
                !matches!(axis, "attribute" | "namespace") && matches!(self.doc.get(p), Some(Node::Elem { .. }))
            }
            (Test::Name(n), Ref::Attr(..)) => match axis {
                "attribute" => self.doc.name(r) == *n,
                "namespace" => self.doc.name(r).strip_prefix("xmlns:") == Some(n.as_str()),
                _ => false,
            },
            // an element by its local name and namespace: a name with no prefix is one in no namespace
            (Test::Name(n), Ref::Node(p)) => match self.doc.get(p) {
                Some(Node::Elem { name, .. })
                    if local(name) == local(n) && !matches!(axis, "attribute" | "namespace") =>
                {
                    let want = match prefix_of(n) {
                        "" => None,
                        "xml" => Some("http://www.w3.org/XML/1998/namespace".to_string()),
                        q => self.ns.iter().find(|(x, _)| x == q).map(|(_, u)| u.clone()),
                    };
                    self.doc.ns_at(p, prefix_of(name)) == want
                }
                _ => false,
            },
            (Test::Text, Ref::Node(p)) => matches!(self.doc.get(p), Some(Node::Text(_) | Node::CData(_))),
            (Test::Comment, Ref::Node(p)) => matches!(self.doc.get(p), Some(Node::Comment(_))),
            (Test::Pi, Ref::Node(p)) => matches!(self.doc.get(p), Some(Node::Pi(..))),
            _ => false,
        }
    }

    fn call(&mut self, name: &str, ctx: &Ref, pos: usize, size: usize) -> Result<Val, String> {
        let mut args = vec![];
        if !self.eat(&Tok::RParen) {
            loop {
                args.push(self.or(ctx, pos, size)?);
                if self.eat(&Tok::Comma) {
                    continue;
                }
                if self.eat(&Tok::RParen) {
                    break;
                }
                return Err("Invalid expression".into());
            }
        }
        let d = self.doc;
        let arg_s = |i: usize| args.get(i).map(|v| to_str(d, v));
        let ctx_s = || d.string_value(ctx);
        Ok(match name {
            "count" => match args.first() {
                Some(Val::Nodes(n)) => Val::Num(n.len() as f64),
                _ => return Err("Invalid type".into()),
            },
            "last" => Val::Num(size as f64),
            "position" => Val::Num(pos as f64),
            "not" => Val::Bool(!args.first().is_some_and(to_bool)),
            "true" => Val::Bool(true),
            "false" => Val::Bool(false),
            "boolean" => Val::Bool(args.first().is_some_and(to_bool)),
            "string" => Val::Str(arg_s(0).unwrap_or_else(ctx_s)),
            "number" => {
                Val::Num(args.first().map_or_else(|| ctx_s().trim().parse().unwrap_or(f64::NAN), |v| to_num(d, v)))
            }
            "name" | "local-name" => Val::Str(match args.first() {
                Some(Val::Nodes(n)) => n.first().map(|r| d.name(r)).unwrap_or_default(),
                _ => d.name(ctx),
            }),
            "concat" => Val::Str(args.iter().map(|v| to_str(d, v)).collect()),
            "contains" => Val::Bool(arg_s(0).unwrap_or_default().contains(&arg_s(1).unwrap_or_default())),
            "starts-with" => Val::Bool(arg_s(0).unwrap_or_default().starts_with(&arg_s(1).unwrap_or_default())),
            "string-length" => Val::Num(arg_s(0).unwrap_or_else(ctx_s).chars().count() as f64),
            "normalize-space" => {
                Val::Str(arg_s(0).unwrap_or_else(ctx_s).split_whitespace().collect::<Vec<_>>().join(" "))
            }
            "substring-before" => {
                let (a, b) = (arg_s(0).unwrap_or_default(), arg_s(1).unwrap_or_default());
                Val::Str(a.find(&b).map(|i| a[..i].to_string()).unwrap_or_default())
            }
            "substring-after" => {
                let (a, b) = (arg_s(0).unwrap_or_default(), arg_s(1).unwrap_or_default());
                Val::Str(a.find(&b).map(|i| a[i + b.len()..].to_string()).unwrap_or_default())
            }
            "substring" => {
                let s: Vec<char> = arg_s(0).unwrap_or_default().chars().collect();
                let from = args.get(1).map_or(1.0, |v| to_num(d, v)).round();
                let len = args.get(2).map_or(f64::INFINITY, |v| to_num(d, v)).round();
                Val::Str(
                    s.iter()
                        .enumerate()
                        .filter(|(i, _)| {
                            let p = (*i + 1) as f64;
                            p >= from && p < from + len
                        })
                        .map(|(_, c)| *c)
                        .collect(),
                )
            }
            "translate" => {
                let (s, from, to): (String, Vec<char>, Vec<char>) = (
                    arg_s(0).unwrap_or_default(),
                    arg_s(1).unwrap_or_default().chars().collect(),
                    arg_s(2).unwrap_or_default().chars().collect(),
                );
                Val::Str(
                    s.chars()
                        .filter_map(|c| match from.iter().position(|f| *f == c) {
                            Some(i) => to.get(i).copied(),
                            None => Some(c),
                        })
                        .collect(),
                )
            }
            "sum" => match args.first() {
                Some(Val::Nodes(n)) => {
                    Val::Num(n.iter().map(|r| d.string_value(r).trim().parse::<f64>().unwrap_or(f64::NAN)).sum())
                }
                _ => return Err("Invalid type".into()),
            },
            "floor" => Val::Num(args.first().map_or(f64::NAN, |v| to_num(d, v)).floor()),
            "ceiling" => Val::Num(args.first().map_or(f64::NAN, |v| to_num(d, v)).ceil()),
            "round" => Val::Num(args.first().map_or(f64::NAN, |v| to_num(d, v)).round()),
            f => return Err(format!("Unregistered function {f}")),
        })
    }
}

enum Test {
    Any,
    Star,
    Name(String),
    Text,
    Comment,
    Pi,
}

/// A name without its namespace prefix.
fn local(n: &str) -> &str {
    n.rsplit_once(':').map_or(n, |(_, l)| l)
}

/// A name's namespace prefix ("" for none).
fn prefix_of(n: &str) -> &str {
    n.split_once(':').map_or("", |(p, _)| p)
}

fn same(a: &Ref, b: &Ref) -> bool {
    match (a, b) {
        (Ref::Root, Ref::Root) => true,
        (Ref::Node(p), Ref::Node(q)) => p == q,
        (Ref::Attr(p, i), Ref::Attr(q, j)) => p == q && i == j,
        _ => false,
    }
}

/// Why an XPath call fails, as Postgres says it.
pub enum XErr {
    /// the document isn't one: "could not parse XML document", with libxml's errors (empty: no detail)
    Doc(String),
    /// "empty XPath expression"
    Empty,
    /// the path: "invalid XPath expression"
    Path(String),
    /// evaluating it: "could not create XPath object"
    Eval(String),
    /// "more than one value returned by column XPath expression"
    Many,
}

/// xpath's document (xmlCtxtReadMemory after its declaration), namespaces checked: its tree, and the warnings
/// libxml gave reading it (as Postgres's WARNING shows them; empty: none).
fn xpath_doc(doc: &str) -> Result<(Doc, String), XErr> {
    let skip = decl(doc).map_or(0, |d| d.len);
    match read(&doc[skip..], true, true) {
        Ok(r) => Ok((Doc { top: r.nodes }, r.warnings)),
        Err(Invalid::Errors(e)) => Err(XErr::Doc(e)),
        Err(Invalid::Decl(d)) => Err(XErr::Doc(d.to_string())),
    }
}

/// `path` evaluated from the document's root: an error in it is the path's, an undefined prefix evaluating's.
fn eval_path(d: &Doc, path: &str, ns: &[(String, String)]) -> Result<Val, XErr> {
    if path.is_empty() {
        return Err(XErr::Empty);
    }
    let toks = lex(path).map_err(XErr::Path)?;
    if toks.is_empty() {
        return Err(XErr::Path("Invalid expression".into()));
    }
    let mut x = X { doc: d, t: toks, i: 0, ns };
    let v = x
        .or(&Ref::Root, 1, 1)
        .map_err(|e| if e == "Undefined namespace prefix" { XErr::Eval(e) } else { XErr::Path(e) })?;
    if x.i < x.t.len() {
        return Err(XErr::Path("Invalid expression".into()));
    }
    Ok(v)
}

/// The result of an XPath on a document: each selected node as XML text, or the one value of an expression that
/// isn't a node set; and the warnings reading the document gave.
pub fn xpath(path: &str, doc: &str, ns: &[(String, String)]) -> Result<(Vec<String>, String), XErr> {
    // (an empty path is refused before the document is read)
    if path.is_empty() {
        return Err(XErr::Empty);
    }
    let (d, warnings) = xpath_doc(doc)?;
    let found = match eval_path(&d, path, ns)? {
        Val::Nodes(n) => n.iter().map(|r| d.output(r)).collect(),
        Val::Str(s) => vec![escape_text(&s)],
        Val::Num(n) => vec![num_text(n)],
        Val::Bool(b) => vec![b.to_string()],
    };
    Ok((found, warnings))
}

/// Whether the XPath selects anything: a value that isn't a node set always does (xml_xpathobjtoxmlarray gives it
/// as one item); and the warnings reading the document gave.
pub fn xpath_exists(path: &str, doc: &str, ns: &[(String, String)]) -> Result<(bool, String), XErr> {
    if path.is_empty() {
        return Err(XErr::Empty);
    }
    let (d, warnings) = xpath_doc(doc)?;
    let any = match eval_path(&d, path, ns)? {
        Val::Nodes(n) => !n.is_empty(),
        _ => true,
    };
    Ok((any, warnings))
}

/// A column of XMLTABLE: its path (None: the column's own name), and whether it keeps XML as it is.
pub struct TableCol {
    pub path: Option<String>,
    pub ordinality: bool,
    pub as_xml: bool,
}

/// XMLTABLE's rows: each node the row path selects, and per column the value its path gives from there (None:
/// nothing). An XML column gets all the nodes its path selects, one written after another, any other just one.
pub fn table(
    row_path: &str,
    doc: &str,
    cols: &[TableCol],
    names: &[String],
    ns: &[(String, String)],
) -> Result<Vec<Vec<Option<String>>>, XErr> {
    let (d, _) = xpath_doc(doc)?;
    let Val::Nodes(rows) = eval_path(&d, row_path, ns)? else {
        return Err(XErr::Path("XPath expression must give nodes".into()));
    };
    let mut out = vec![];
    for (n, r) in rows.iter().enumerate() {
        let mut row = vec![];
        for (c, name) in cols.iter().zip(names) {
            if c.ordinality {
                row.push(Some((n + 1).to_string()));
                continue;
            }
            let path = c.path.clone().unwrap_or_else(|| name.clone());
            let mut cx = X { doc: &d, t: lex(&path).map_err(XErr::Path)?, i: 0, ns };
            let v = cx.or(r, 1, 1).map_err(XErr::Path)?;
            row.push(match v {
                Val::Nodes(found) if found.is_empty() => None,
                Val::Nodes(found) if c.as_xml => Some(found.iter().map(|f| d.output(f)).collect()),
                Val::Nodes(found) if found.len() > 1 => return Err(XErr::Many),
                Val::Nodes(found) => Some(d.string_value(&found[0])),
                // (a string goes in an XML column escaped)
                Val::Str(s) if c.as_xml => Some(escape_text(&s)),
                other => Some(to_str(&d, &other)),
            });
        }
        out.push(row);
    }
    Ok(out)
}
