//! SQL to XML mapping: `table_to_xml`, `query_to_xml`, `schema_to_xml` (with their forest forms), the schemas
//! (`*_xmlschema`) and both together (`*_and_xmlschema`), as Postgres writes them.

use super::*;

const XSI: &str = "xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\"";
const XSD: &str = "xmlns:xsd=\"http://www.w3.org/2001/XMLSchema\"";

/// A column as the mapping sees it.
struct XCol {
    name: String,
    ty: Type,
    chars: Option<(u32, bool)>,
    domain: Option<String>,
}

/// What a mapping reads: columns and rows, and what it is called.
struct Src {
    cols: Vec<XCol>,
    rows: Vec<Vec<Value>>,
    /// the name of the root (or of each row of a forest) in the data
    name: String,
    /// the type-name suffix: `.catalog.schema.table`, or empty for a query
    qual: String,
}

/// A SQL identifier as an XML name: a character a name can't have is `_xHHHH_`.
fn xml_name(s: &str) -> String {
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        let ok = c.is_alphabetic() || c == '_' || (i > 0 && (c.is_ascii_digit() || c == '-' || c == '.'));
        if ok && !(i == 0 && s.to_lowercase().starts_with("xml")) {
            out.push(c);
        } else {
            out.push_str(&format!("_x{:04X}_", c as u32));
        }
    }
    out
}

fn esc(s: &str) -> String {
    xmlfn::escape_text(s)
}

pub(super) fn base64(b: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in b.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

/// A value as the mapping writes it (dates and times in ISO 8601, booleans as true and false, bytes in base64).
fn shown(v: &Value, c: &XCol) -> String {
    match (v, c.ty) {
        (Value::Bool(b), _) => b.to_string(),
        (Value::Bytea(b), _) => base64(b),
        (v, _) => xmlfn::value_text(v),
    }
}

/// The element names and values of one row.
fn fields(src: &Src, row: &[Value], nulls: bool) -> String {
    let mut s = String::new();
    for (c, v) in src.cols.iter().zip(row) {
        let n = xml_name(&c.name);
        match v {
            Value::Null if nulls => s.push_str(&format!("  <{n} xsi:nil=\"true\"/>\n")),
            Value::Null => {}
            v => s.push_str(&format!("  <{n}>{}</{n}>\n", esc(&shown(v, c)))),
        }
    }
    s
}

fn tag_head(name: &str, ns: &str, extra: &str) -> String {
    let mut h = format!("<{name} {XSI}");
    if !ns.is_empty() {
        h.push_str(&format!(" xmlns=\"{}\"", esc(ns).replace('"', "&quot;")));
    }
    h.push_str(extra);
    h
}

/// The location of the schema in a document that holds it.
fn location(ns: &str) -> String {
    if ns.is_empty() {
        " xsi:noNamespaceSchemaLocation=\"#\"".into()
    } else {
        format!(" xsi:schemaLocation=\"{} #\"", esc(ns))
    }
}

/// A source's rows as a table (`<root>` with `<row>`s) or a forest (each row an element of its own).
fn data(src: &Src, nulls: bool, forest: bool, ns: &str, extra: &str) -> String {
    let mut s = String::new();
    if !forest {
        s.push_str(&tag_head(&src.name, ns, extra));
        s.push_str(">\n\n");
    }
    for row in &src.rows {
        if forest {
            s.push_str(&tag_head(&src.name, ns, ""));
            s.push_str(">\n");
        } else {
            s.push_str("<row>\n");
        }
        s.push_str(&fields(src, row, nulls));
        s.push_str(&format!("</{}>\n\n", if forest { &src.name } else { "row" }));
    }
    if !forest {
        s.push_str(&format!("</{}>\n", src.name));
    }
    s
}

// ---- the schema ----

fn int_type(name: &str, base: &str, max: &str, min: &str) -> String {
    format!(
        "<xsd:simpleType name=\"{name}\">\n  <xsd:restriction base=\"{base}\">\n    <xsd:maxInclusive value=\"{max}\"/>\n    <xsd:minInclusive value=\"{min}\"/>\n  </xsd:restriction>\n</xsd:simpleType>\n\n"
    )
}

fn string_type(name: &str, base: &str) -> String {
    format!(
        "<xsd:simpleType name=\"{name}\">\n  <xsd:restriction base=\"{base}\">\n  </xsd:restriction>\n</xsd:simpleType>\n\n"
    )
}

fn plain_type(name: &str, base: &str) -> String {
    format!(
        "<xsd:simpleType name=\"{name}\">\n  <xsd:restriction base=\"{base}\"></xsd:restriction>\n</xsd:simpleType>\n\n"
    )
}

fn pattern_type(name: &str, base: &str, pattern: &str) -> String {
    format!(
        "<xsd:simpleType name=\"{name}\">\n  <xsd:restriction base=\"{base}\">\n    <xsd:pattern value=\"{pattern}\"/>\n  </xsd:restriction>\n</xsd:simpleType>\n\n"
    )
}

/// The XML Schema type of a column: its name, and the definitions it needs, in the order they are made (a domain's
/// base type first).
fn schema_type(c: &XCol, cat: &str) -> (String, Vec<(String, String)>) {
    const TIME: &str = "\\p{Nd}{2}:\\p{Nd}{2}:\\p{Nd}{2}(.\\p{Nd}+)?";
    const DATE: &str = "\\p{Nd}{4}-\\p{Nd}{2}-\\p{Nd}{2}";
    const ZONE: &str = "(\\+|-)\\p{Nd}{2}:\\p{Nd}{2}";
    let one = |n: &str, def: String| (n.to_string(), vec![(n.to_string(), def)]);
    let (name, defs) = match c.ty {
        Type::Int => one("INTEGER", int_type("INTEGER", "xsd:int", "2147483647", "-2147483648")),
        Type::SmallInt => one("SMALLINT", int_type("SMALLINT", "xsd:short", "32767", "-32768")),
        Type::BigInt => one("BIGINT", int_type("BIGINT", "xsd:long", "9223372036854775807", "-9223372036854775808")),
        Type::Float4 => one("REAL", plain_type("REAL", "xsd:float")),
        Type::Float => one("DOUBLE", plain_type("DOUBLE", "xsd:double")),
        Type::Bool => one("BOOLEAN", plain_type("BOOLEAN", "xsd:boolean")),
        Type::Numeric => one("NUMERIC", "<xsd:simpleType name=\"NUMERIC\">\n</xsd:simpleType>\n\n".to_string()),
        Type::Time => one("TIME", pattern_type("TIME", "xsd:time", TIME)),
        Type::TimeTz => one("TIME_WTZ", pattern_type("TIME_WTZ", "xsd:time", &format!("{TIME}{ZONE}"))),
        Type::Timestamp => one("TIMESTAMP", pattern_type("TIMESTAMP", "xsd:dateTime", &format!("{DATE}T{TIME}"))),
        Type::TimestampTz => {
            one("TIMESTAMP_WTZ", pattern_type("TIMESTAMP_WTZ", "xsd:dateTime", &format!("{DATE}T{TIME}{ZONE}")))
        }
        Type::Date => one("DATE", pattern_type("DATE", "xsd:date", DATE)),
        Type::Xml => (
            "XML".to_string(),
            vec![(
                "XML".to_string(),
                "<xsd:complexType mixed=\"true\">\n  <xsd:sequence>\n    <xsd:any name=\"element\" minOccurs=\"0\" maxOccurs=\"unbounded\" processContents=\"skip\"/>\n  </xsd:sequence>\n</xsd:complexType>\n\n".to_string(),
            )],
        ),
        Type::Bytea => {
            let n = format!("UDT.{cat}.pg_catalog.bytea");
            (n.clone(), vec![(n.clone(), string_type(&n, "xsd:base64Binary"))])
        }
        // a varchar or char column is text here, told by its length
        Type::Text | Type::Name if c.chars.is_some() => {
            let n = if c.chars.is_some_and(|(_, fixed)| fixed) { "CHAR" } else { "VARCHAR" };
            one(n, string_type(n, "xsd:string"))
        }
        t => {
            let n = format!("UDT.{cat}.pg_catalog.{}", t.name());
            (n.clone(), vec![(n.clone(), string_type(&n, "xsd:string"))])
        }
    };
    match &c.domain {
        Some(d) => {
            let dn = format!("Domain.{cat}.public.{}", rel_part(d));
            let mut all = defs;
            all.push((
                dn.clone(),
                format!("<xsd:simpleType name=\"{dn}\">\n  <xsd:restriction base=\"{name}\"/>\n</xsd:simpleType>\n\n"),
            ));
            (dn, all)
        }
        None => (name, defs),
    }
}

fn schema_head(ns: &str) -> String {
    let mut h = format!("<xsd:schema\n    {XSD}");
    if !ns.is_empty() {
        h.push_str(&format!(
            "\n    targetNamespace=\"{}\"\n    elementFormDefault=\"qualified\"",
            esc(ns).replace('"', "&quot;")
        ));
    }
    h.push_str(">\n\n");
    h
}

/// The simple types of several sources' columns, each once, in the order a column first needs it.
fn type_defs(srcs: &[&Src], cat: &str) -> String {
    let mut seen: Vec<String> = vec![];
    let mut out = String::new();
    for s in srcs {
        for c in &s.cols {
            for (n, def) in schema_type(c, cat).1 {
                if !seen.contains(&n) {
                    seen.push(n);
                    out.push_str(&def);
                }
            }
        }
    }
    out
}

fn element_lines(src: &Src, nulls: bool, cat: &str) -> String {
    let mut s = String::new();
    for c in &src.cols {
        let occurs = if nulls { "nillable=\"true\"" } else { "minOccurs=\"0\"" };
        s.push_str(&format!(
            "    <xsd:element name=\"{}\" type=\"{}\" {occurs}></xsd:element>\n",
            xml_name(&c.name),
            schema_type(c, cat).0
        ));
    }
    s
}

/// A table's or query's schema.
fn xmlschema(src: &Src, nulls: bool, forest: bool, ns: &str, cat: &str) -> String {
    let q = &src.qual;
    let mut s = schema_head(ns);
    s.push_str(&type_defs(&[src], cat));
    s.push_str(&format!(
        "<xsd:complexType name=\"RowType{q}\">\n  <xsd:sequence>\n{}  </xsd:sequence>\n</xsd:complexType>\n\n",
        element_lines(src, nulls, cat)
    ));
    if forest {
        s.push_str(&format!("<xsd:element name=\"{}\" type=\"RowType{q}\"/>\n\n", src.name));
    } else {
        s.push_str(&format!(
            "<xsd:complexType name=\"TableType{q}\">\n  <xsd:sequence>\n    <xsd:element name=\"row\" type=\"RowType{q}\" minOccurs=\"0\" maxOccurs=\"unbounded\"/>\n  </xsd:sequence>\n</xsd:complexType>\n\n<xsd:element name=\"{}\" type=\"TableType{q}\"/>\n\n",
            src.name
        ));
    }
    s.push_str("</xsd:schema>");
    s
}

/// A document of data with the schema in it.
fn both(src: &Src, nulls: bool, forest: bool, ns: &str, cat: &str) -> String {
    let schema = xmlschema(src, nulls, forest, ns, cat);
    if forest {
        return format!("{schema}\n\n{}", data(src, nulls, true, ns, ""));
    }
    let body = data(src, nulls, false, ns, &location(ns));
    // the root and its blank line, the schema, a blank line, and the rest
    let at = body.find(">\n\n").map_or(0, |i| i + 3);
    format!("{}{schema}\n\n{}", &body[..at], &body[at..])
}

// ---- sources ----

fn query_src(ctx: &Ctx, sql: &str, forest: bool) -> Result<Src> {
    let Some(db) = ctx.core.db() else { return err(UNSUPPORTED, "an XML mapping needs a running database") };
    // (the session's zone and settings are put back after the query: the values are written in them)
    let kept = time::zone_state();
    let ran = run(&db, &ctx.world, sql, &[]);
    time::restore_zone(kept);
    let out =
        ran?.pop().ok_or_else(|| SqlError { code: "42601", message: "a query was expected".into(), info: None })?;
    let cols = out
        .columns
        .unwrap_or_default()
        .into_iter()
        .map(|(name, ty)| XCol { name, ty, chars: None, domain: None })
        .collect();
    Ok(Src { cols, rows: out.rows, name: if forest { "row" } else { "table" }.into(), qual: String::new() })
}

fn table_src(ctx: &Ctx, table: &str, cat: &str) -> Result<Src> {
    let v = ctx.core.view_tx(&ctx.world)?;
    let t = need_table(&v, table)?;
    let Some(db) = ctx.core.db() else { return err(UNSUPPORTED, "an XML mapping needs a running database") };
    let kept = time::zone_state();
    let ran = run(&db, &ctx.world, &format!("select * from {}", ident_rel(&t.name)), &[]);
    time::restore_zone(kept);
    let out = ran?.pop().unwrap_or_else(|| Output::tag("SELECT"));
    let cols = t
        .cols
        .iter()
        .filter(|c| !c.hidden)
        .map(|c| XCol {
            name: c.name.clone(),
            ty: c.ty,
            chars: c.chars.or_else(|| c.domain.as_deref().and_then(|d| domain_chars(&v, d))),
            domain: c.domain.clone(),
        })
        .collect();
    let (schema, rel) = match t.name.split_once('.') {
        Some((s, r)) => (s.to_string(), r.to_string()),
        None => ("public".into(), t.name.clone()),
    };
    Ok(Src { cols, rows: out.rows, name: xml_name(&rel), qual: format!(".{cat}.{schema}.{rel}") })
}

/// A domain over varchar or char is read as one: (0, fixed).
fn domain_chars(v: &View, domain: &str) -> Option<(u32, bool)> {
    let raw = v.get(&type_key(domain)).ok()??;
    let j: Json = serde_json::from_str(&raw).ok()?;
    let base = j["domain"]["base"].as_str()?.to_lowercase();
    if base.starts_with("varchar") || base.starts_with("character varying") {
        Some((0, false))
    } else if base.starts_with("char") || base.starts_with("bpchar") {
        Some((0, true))
    } else {
        None
    }
}

/// The tables of a schema, in the order they were made (their names when that is not known).
fn schema_tables(ctx: &Ctx, schema: &str) -> Result<Vec<String>> {
    let v = ctx.core.view_tx(&ctx.world)?;
    let mut names: Vec<String> = schemas(&v)?
        .into_iter()
        .filter(|t| t.name.split_once('.').map_or("public", |(s, _)| s) == schema)
        .map(|t| t.name)
        .collect();
    names.sort();
    Ok(names)
}

/// A cursor's columns and the rows FETCH FORWARD `count` would give (moving it as that does; none moved for its
/// schema), as cursor_to_xml reads them: a cursor DECLAREd in the session, or a portal PL/pgSQL opened.
fn cursor_src(ctx: &Ctx, name: &str, count: Option<i64>, forest: bool) -> Result<Src> {
    let mut st = ctx.state.lock().unwrap();
    let Some(c) = st.portals.get_mut(name) else {
        return err("34000", format!("cursor \"{name}\" does not exist"));
    };
    let mut rows = vec![];
    if let Some(n) = count {
        let (at, to) = walk_cursor(c.pos, c.rows.len(), Move::Fwd(Some(n.max(0))));
        c.pos = to;
        rows = at.into_iter().map(|i| c.rows[i].clone()).collect();
    }
    let cols =
        (c.columns.iter()).map(|(name, ty)| XCol { name: name.clone(), ty: *ty, chars: None, domain: None }).collect();
    Ok(Src { cols, rows, name: if forest { "row" } else { "table" }.into(), qual: String::new() })
}

/// Whether `name` is one of the mapping functions.
pub(super) fn handles(name: &str) -> bool {
    (name.starts_with("table_to_xml")
        || name.starts_with("query_to_xml")
        || name.starts_with("schema_to_xml")
        || matches!(name, "cursor_to_xml" | "cursor_to_xmlschema"))
        && (name.ends_with("xml") || name.ends_with("xmlschema"))
}

pub(super) fn call(ctx: &Ctx, name: &str, a: &[Value]) -> Result<Value> {
    // (the database's name; the regression suite's own under CHRONOS_PG_EXACT)
    let cat = if pg_exact() { "regression".to_string() } else { ctx.home.clone() };
    let flag = |v: &Value| matches!(v, Value::Bool(true));
    // cursor_to_xml(cursor, count, nulls, tableforest, targetns) and cursor_to_xmlschema(cursor, nulls, ...)
    if name.starts_with("cursor_") {
        let cursor = a[0].to_text();
        let (count, rest) = match name {
            "cursor_to_xml" => match coerce(a[1].clone(), Type::BigInt)? {
                Value::Int(n) => (Some(n), &a[2..]),
                _ => return Ok(Value::Null),
            },
            _ => (None, &a[1..]),
        };
        let (nulls, forest, ns) = (flag(&rest[0]), flag(&rest[1]), rest[2].to_text());
        let src = cursor_src(ctx, &cursor, count, forest)?;
        return Ok(Value::Text(match count {
            Some(_) => data(&src, nulls, forest, &ns, ""),
            None => xmlschema(&src, nulls, forest, &ns, &cat),
        }));
    }
    let (nulls, forest, ns) = (flag(&a[1]), flag(&a[2]), a[3].to_text());
    let kind = name.split('_').next().unwrap_or("");
    let (what, schema_only, with_data) = match name {
        n if n.ends_with("_and_xmlschema") => ("both", false, true),
        n if n.ends_with("_xmlschema") => ("schema", true, false),
        _ => ("xml", false, true),
    };
    let _ = (what, with_data);
    if kind == "schema" {
        return schema_call(ctx, name, &a[0].to_text(), nulls, forest, &ns, &cat);
    }
    let src =
        if kind == "query" { query_src(ctx, &a[0].to_text(), forest)? } else { table_src(ctx, &a[0].to_text(), &cat)? };
    Ok(Value::Text(if schema_only {
        xmlschema(&src, nulls, forest, &ns, &cat)
    } else if name.ends_with("_and_xmlschema") {
        both(&src, nulls, forest, &ns, &cat)
    } else {
        data(&src, nulls, forest, &ns, "")
    }))
}

fn schema_call(ctx: &Ctx, name: &str, schema: &str, nulls: bool, forest: bool, ns: &str, cat: &str) -> Result<Value> {
    let srcs: Vec<Src> = schema_tables(ctx, schema)?.iter().map(|t| table_src(ctx, t, cat)).collect::<Result<_>>()?;
    let refs: Vec<&Src> = srcs.iter().collect();
    let xml_schema = || {
        let mut s = schema_head(ns);
        s.push_str(&type_defs(&refs, cat));
        let st = format!("SchemaType.{cat}.{schema}");
        if forest {
            s.push_str(&format!("<xsd:complexType name=\"{st}\">\n  <xsd:sequence>\n"));
            for t in &srcs {
                s.push_str(&format!(
                    "    <xsd:element name=\"{}\" type=\"RowType{}\" minOccurs=\"0\" maxOccurs=\"unbounded\"/>\n",
                    t.name, t.qual
                ));
            }
            s.push_str("  </xsd:sequence>\n</xsd:complexType>\n\n");
        } else {
            s.push_str(&format!("<xsd:complexType name=\"{st}\">\n  <xsd:all>\n"));
            for t in &srcs {
                s.push_str(&format!("    <xsd:element name=\"{}\" type=\"TableType{}\"/>\n", t.name, t.qual));
            }
            s.push_str("  </xsd:all>\n</xsd:complexType>\n\n");
        }
        s.push_str(&format!("<xsd:element name=\"{}\" type=\"{st}\"/>\n\n</xsd:schema>", xml_name(schema)));
        s
    };
    if name.ends_with("_xmlschema") && !name.ends_with("_and_xmlschema") {
        return Ok(Value::Text(xml_schema()));
    }
    let extra = if name.ends_with("_and_xmlschema") { location(ns) } else { String::new() };
    let mut s = format!("{}>\n\n", tag_head(&xml_name(schema), ns, &extra));
    if name.ends_with("_and_xmlschema") {
        s.push_str(&xml_schema());
        s.push_str("\n\n");
    }
    for t in &srcs {
        if forest {
            for row in &t.rows {
                s.push_str(&format!("<{}>\n{}</{}>\n\n", t.name, fields(t, row, nulls), t.name));
            }
            s.push('\n');
        } else {
            s.push_str(&format!("<{}>\n\n", t.name));
            for row in &t.rows {
                s.push_str(&format!("<row>\n{}</row>\n\n", fields(t, row, nulls)));
            }
            s.push_str(&format!("</{}>\n\n", t.name));
        }
    }
    s.push_str(&format!("</{}>\n", xml_name(schema)));
    Ok(Value::Text(s))
}
