//! The keyword forms of the XML functions: XMLELEMENT, XMLFOREST, XMLPI, XMLROOT, XMLPARSE, XMLSERIALIZE, XMLEXISTS.

use super::*;

impl Parser {
    /// `expr [AS name]` of an XMLATTRIBUTES or XMLFOREST list: the name, as written or as the column's.
    fn xml_named(&mut self) -> Result<(String, Expr)> {
        let e = self.expr()?;
        let name = if self.kw("as") {
            self.name()?
        } else {
            match &e {
                Expr::Col(_, c) => c.clone(),
                _ => return err(SYNTAX, "unnamed XML attribute value must be a column reference"),
            }
        };
        Ok((name, e))
    }

    /// After the word and before its `(`.
    pub(super) fn xml_form(&mut self, w: &str) -> Result<Expr> {
        use funcs::Scalar as S;
        self.expect_sym("(")?;
        let lit = |s: &str| Expr::Lit(Value::Text(s.into()));
        let call = |f: S, args: Vec<Expr>| Expr::Func(Func::Scalar(f), args);
        let out = match w {
            "xmlparse" | "xmlserialize" => {
                let mode = if self.kw("document") {
                    "document"
                } else {
                    self.expect_kw("content")?;
                    "content"
                };
                let e = self.expr()?;
                if w == "xmlparse" {
                    // PRESERVE | STRIP WHITESPACE
                    if self.kw("preserve") || self.kw("strip") {
                        self.expect_kw("whitespace")?;
                    }
                    call(S::XmlParse, vec![e, lit(mode)])
                } else {
                    self.expect_kw("as")?;
                    let t = self.ty()?;
                    // [NO] INDENT
                    let indent = if self.kw("no") {
                        self.expect_kw("indent")?;
                        false
                    } else {
                        self.kw("indent")
                    };
                    let args = vec![e, lit(mode), lit(if indent { "indent" } else { "" })];
                    Expr::Cast(Box::new(call(S::XmlSerialize, args)), t)
                }
            }
            "xmlelement" => {
                self.expect_kw("name")?;
                let name = self.name()?;
                let mut args = vec![lit(&name), Expr::Lit(Value::Int(0))];
                let mut nattr = 0;
                let mut content = vec![];
                while self.sym(",") {
                    if self.is_kw("xmlattributes") && matches!(self.t.get(self.i + 1), Some(Tok::Sym("("))) {
                        self.i += 2;
                        loop {
                            let (n, e) = self.xml_named()?;
                            // (an attribute named twice is refused as Postgres refuses it)
                            if args.iter().skip(2).step_by(2).any(|a| matches!(a, Expr::Lit(Value::Text(x)) if *x == n))
                            {
                                return err(SYNTAX, format!("XML attribute name \"{n}\" appears more than once"));
                            }
                            args.push(lit(&n));
                            args.push(e);
                            nattr += 1;
                            if !self.sym(",") {
                                break;
                            }
                        }
                        self.expect_sym(")")?;
                    } else {
                        content.push(self.expr()?);
                    }
                }
                args[1] = Expr::Lit(Value::Int(nattr));
                args.extend(content);
                call(S::XmlElement, args)
            }
            "xmlforest" => {
                let mut args = vec![];
                loop {
                    let (n, e) = self.xml_named()?;
                    args.push(lit(&n));
                    args.push(e);
                    if !self.sym(",") {
                        break;
                    }
                }
                call(S::XmlForest, args)
            }
            "xmlpi" => {
                self.expect_kw("name")?;
                let name = self.name()?;
                let mut args = vec![lit(&name)];
                if self.sym(",") {
                    args.push(self.expr()?);
                }
                call(S::XmlPi, args)
            }
            "xmlroot" => {
                let x = self.expr()?;
                self.expect_sym(",")?;
                self.expect_kw("version")?;
                let version = if self.kw("no") {
                    self.expect_kw("value")?;
                    Expr::Lit(Value::Null)
                } else {
                    self.expr()?
                };
                // STANDALONE YES, NO or NO VALUE; left out, the value's own stays
                let mut standalone = lit("omitted");
                if self.sym(",") {
                    self.expect_kw("standalone")?;
                    standalone = if self.kw("yes") {
                        lit("yes")
                    } else if self.kw("no") {
                        if self.kw("value") { lit("no value") } else { lit("no") }
                    } else {
                        return self.fail("expected YES, NO or NO VALUE");
                    };
                }
                call(S::XmlRoot, vec![x, version, standalone])
            }
            _ => {
                // XMLEXISTS(path PASSING [BY REF | BY VALUE] doc [BY REF | BY VALUE])
                let path = self.expr()?;
                self.expect_kw("passing")?;
                let by = |p: &mut Parser| {
                    if p.kw("by") {
                        let _ = p.kw("ref") || p.kw("value");
                    }
                };
                by(self);
                let doc = self.expr()?;
                by(self);
                call(S::XmlExistsFn, vec![path, doc])
            }
        };
        self.expect_sym(")")?;
        Ok(out)
    }
}

impl Parser {
    /// XMLTABLE( [XMLNAMESPACES(...),] row PASSING doc COLUMNS ... ) [AS alias [(names)]], as a function source whose
    /// columns and their paths are kept in its name (see `srf_columns` and `srf_rows`).
    pub(super) fn xmltable_source(&mut self) -> Result<Source> {
        self.i += 1; // xmltable
        self.expect_sym("(")?;
        // XMLNAMESPACES('uri' AS prefix, ...): the prefixes the paths may use (a DEFAULT one Postgres refuses)
        let mut namespaces = vec![];
        if self.is_kw("xmlnamespaces") {
            self.i += 1;
            self.expect_sym("(")?;
            loop {
                if self.kw("default") {
                    return err(UNSUPPORTED, "DEFAULT namespace is not supported");
                }
                let uri = match self.next() {
                    Some(Tok::Str(s)) => s,
                    t => {
                        self.put_back(t);
                        return self.fail("expected a namespace URI as a string");
                    }
                };
                self.expect_kw("as")?;
                namespaces.push(serde_json::json!([self.name()?, uri]));
                if !self.sym(",") {
                    break;
                }
            }
            self.expect_sym(")")?;
            self.expect_sym(",")?;
        }
        let row = self.expr()?;
        self.expect_kw("passing")?;
        let by = |p: &mut Parser| {
            if p.kw("by") {
                let _ = p.kw("ref") || p.kw("value");
            }
        };
        by(self);
        let doc = self.expr()?;
        by(self);
        self.expect_kw("columns")?;
        let (mut defs, mut extras) = (vec![], vec![]);
        loop {
            let name = self.name()?;
            if self.kw("for") {
                self.expect_kw("ordinality")?;
                defs.push(format!("{} int", ident(&name)));
                extras.push(serde_json::json!({ "ord": true }));
            } else {
                (self.typmod, self.chars, self.small, self.vdim) = (None, None, false, None);
                let (_, ty) = self.text_of(|p| p.ty().map(drop))?;
                let xml = ty.trim().eq_ignore_ascii_case("xml");
                let (mut path, mut default, mut notnull) = (None, None, false);
                loop {
                    if self.kw("path") {
                        match self.next() {
                            Some(Tok::Str(s)) => path = Some(s),
                            _ => return self.fail("expected a path as a string"),
                        }
                    } else if self.kw("default") {
                        default = Some(self.default_expr()?.1);
                    } else if self.kw("not") {
                        self.expect_kw("null")?;
                        notnull = true;
                    } else if self.kw("null") {
                    } else {
                        break;
                    }
                }
                defs.push(format!("{} {ty}", ident(&name)));
                extras.push(serde_json::json!({ "path": path, "default": default, "notnull": notnull, "xml": xml }));
            }
            if !self.sym(",") {
                break;
            }
        }
        self.expect_sym(")")?;
        self.kw("as");
        let alias = match self.peek() {
            Some(Tok::Word(w)) if !RESERVED.contains(&w.as_str()) && !LATER.contains(&w.as_str()) => self.name()?,
            Some(Tok::Ident(_)) => self.name()?,
            _ => "xmltable".to_string(),
        };
        let names = if self.sym("(") { self.names()? } else { vec![] };
        let name = format!(
            "xmltable{COLDEFS}{}\u{1e}{}\u{1e}{}",
            defs.join(", "),
            serde_json::Value::Array(extras),
            serde_json::Value::Array(namespaces)
        );
        self.calls = true;
        Ok(Source {
            table: alias.clone(),
            alias,
            sub: None,
            names,
            rec: None,
            working: false,
            at: None,
            lateral: true,
            srf: Some(Box::new((name, vec![row, doc]))),
            cte: 0,
            only: false,
            hier: false,
            policed: false,
            bound: false,
        })
    }
}
