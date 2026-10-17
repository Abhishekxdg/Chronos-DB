//! `chronos import <folder> <table> data.csv --schema schema.csv`: a copy of one Postgres table.
//!
//! Made on the Postgres side with:
//!   psql -c "\copy <table> to 'data.csv' csv header"
//!   psql --csv -t -c "<SCHEMA_QUERY, with the table's name for {table}>" > schema.csv
//! schema.csv has one row per column: name, data_type, is_nullable (YES/NO), in the primary key (t/f).
//!
//!   schema.csv ─> columns ─> no primary key? ─> refuse
//!                         ─> CREATE TABLE ... (inside BEGIN, so a failure leaves no table)
//!   data.csv ──> RFC 4180 records ─> header must name the schema's columns
//!            ──> each field: empty = NULL, "" = empty text, else coerced to its column's type
//!                bad row or value ─> ROLLBACK, error naming line and column
//!            ──> INSERT per row ─> COMMIT ─> "imported N rows"

use chronos::Db;
use chronos::sql::{Session, Type, Value, coerce};

/// The query that makes schema.csv, with the table's name in place of `{table}` (docs/sql.md).
pub const SCHEMA_QUERY: &str = "select c.column_name, c.data_type, c.is_nullable, \
exists (select 1 from information_schema.table_constraints tc \
join information_schema.key_column_usage k using (constraint_schema, constraint_name) \
where tc.table_name = c.table_name and tc.table_schema = c.table_schema \
and tc.constraint_type = 'PRIMARY KEY' and k.column_name = c.column_name) \
from information_schema.columns c where c.table_name = '{table}' and c.table_schema = current_schema() order by c.ordinal_position";

/// How to use `chronos import`, with the two psql commands that make its files.
pub fn help() -> String {
    format!(
        "usage: chronos import <folder> <table> <data.csv> --schema <schema.csv>\n   \
         or: chronos import <folder> postgres://user:password@host:port/database [--dry-run] (a whole database)\n\n\
         Make the two files from Postgres (put your table's name in place of TABLE):\n  \
         psql -c \"\\copy TABLE to 'data.csv' csv header\"\n  \
         psql --csv -t -c \"{}\" > schema.csv",
        SCHEMA_QUERY.replace("{table}", "TABLE")
    )
}

#[derive(Debug)]
struct Column {
    name: String,
    ty: Type,
    not_null: bool,
    key: bool,
}

/// Imports `data` into a new table `table` on `branch`. Returns what to print (the row count,
/// and a warning per column imported as text), or an error naming the file, line and column.
pub fn run(db: &Db, branch: &str, table: &str, data: &str, schema: &str) -> Result<String, String> {
    let read = |p: &str| std::fs::read_to_string(p).map_err(|e| format!("{p}: {e}"));
    let (cols, warnings) = columns(&read(schema)?).map_err(|e| format!("{schema}: {e}"))?;
    let text = read(data)?;

    let mut s = Session::new(branch);
    let mut sql = |q: &str, params: &[Value]| s.run(db, q, params).map(|_| ()).map_err(|e| e.described());
    if chronos::sql::tables(db, branch).map_err(|e| e.message)?.iter().any(|t| t.name == table) {
        return Err(format!("table \"{table}\" already exists on {branch}; import into a new table name"));
    }
    let mut defs: Vec<String> = cols
        .iter()
        .map(|c| {
            let mut d = format!("{} {}", ident(&c.name), c.ty.name());
            if c.not_null {
                d += " not null";
            }
            d
        })
        .collect();
    let key: Vec<String> = cols.iter().filter(|c| c.key).map(|c| ident(&c.name)).collect();
    defs.push(format!("primary key ({})", key.join(", ")));
    sql("begin", &[])?;
    let loaded = (|| {
        sql(&format!("create table {} ({})", ident(table), defs.join(", ")), &[])?;
        load(&text, &cols, table, &mut sql).map_err(|e| format!("{data}: {e}"))
    })();
    let n = match loaded {
        Ok(n) => n,
        Err(e) => {
            let _ = sql("rollback", &[]);
            return Err(e);
        }
    };
    sql("commit", &[])?;
    let mut out = warnings;
    out.push(format!("imported {n} row{} into {table}", if n == 1 { "" } else { "s" }));
    Ok(out.join("\n"))
}

/// Inserts every record after the header. Returns the row count.
// ponytail: one INSERT per row, parsed each time (~seconds per million rows); batch rows per
// statement if big imports get slow. The whole import is one transaction held in memory.
fn load(
    text: &str,
    cols: &[Column],
    table: &str,
    sql: &mut impl FnMut(&str, &[Value]) -> Result<(), String>,
) -> Result<usize, String> {
    let mut records = Records::new(text);
    let Some((_, header)) = records.next().transpose()? else { return Err("empty file: expected a header row".into()) };
    let header: Vec<String> = header.into_iter().map(Option::unwrap_or_default).collect();
    let order: Vec<&Column> = header
        .iter()
        .map(|h| {
            cols.iter().find(|c| c.name == *h).ok_or_else(|| format!("line 1: column \"{h}\" isn't in the schema"))
        })
        .collect::<Result<_, _>>()?;
    if let Some(c) = cols.iter().find(|c| !header.contains(&c.name)) {
        return Err(format!("line 1: the header has no column \"{}\"", c.name));
    }
    let names: Vec<String> = order.iter().map(|c| ident(&c.name)).collect();
    let marks: Vec<String> = (1..=order.len()).map(|i| format!("${i}")).collect();
    let insert = format!("insert into {} ({}) values ({})", ident(table), names.join(", "), marks.join(", "));

    let mut n = 0;
    for rec in records {
        let (line, fields) = rec?;
        if fields.len() != order.len() {
            return Err(format!("line {line}: {} fields, but the header has {}", fields.len(), order.len()));
        }
        let mut params = Vec::with_capacity(fields.len());
        for (f, c) in fields.into_iter().zip(&order) {
            let v = match f {
                None if c.not_null || c.key => {
                    return Err(format!("line {line}, column {}: empty, but the column is NOT NULL", c.name));
                }
                None => Value::Null,
                Some(s) => coerce(Value::Text(s), c.ty)
                    .map_err(|e| format!("line {line}, column {}: {}", c.name, e.message))?,
            };
            params.push(v);
        }
        sql(&insert, &params).map_err(|e| format!("line {line}: {e}"))?;
        n += 1;
    }
    Ok(n)
}

/// Reads schema.csv. Returns the columns and a warning per column whose type becomes text.
fn columns(text: &str) -> Result<(Vec<Column>, Vec<String>), String> {
    let (mut cols, mut warnings) = (vec![], vec![]);
    for rec in Records::new(text) {
        let (line, f) = rec?;
        let f: Vec<String> = f.into_iter().map(Option::unwrap_or_default).collect();
        let [name, pg, nullable, key] = &f[..] else {
            return Err(format!("line {line}: expected 4 fields (name, type, nullable, key), got {}", f.len()));
        };
        let ty = pg_type(pg).unwrap_or_else(|| {
            warnings.push(format!("warning: column {name} has type {pg}, imported as text"));
            Type::Text
        });
        let key = matches!(key.as_str(), "t" | "true");
        cols.push(Column { name: name.clone(), ty, not_null: nullable == "NO", key });
    }
    match cols.iter().filter(|c| c.key).count() {
        _ if cols.is_empty() => {
            Err("no columns: is the table name in the query right, and is the table in your current schema?".into())
        }
        0 => Err("the table has no primary key; Chronos keys every row by one".into()),
        _ => Ok((cols, warnings)),
    }
}

/// Postgres's `information_schema` data_type, as a Chronos type; None for types kept as text.
fn pg_type(t: &str) -> Option<Type> {
    Some(match t {
        "smallint" => Type::SmallInt,
        "integer" => Type::Int,
        "bigint" => Type::BigInt,
        "real" | "double precision" => Type::Float,
        "numeric" => Type::Numeric,
        "text" | "character varying" | "character" => Type::Text,
        "uuid" => Type::Uuid,
        "inet" => Type::Inet,
        "cidr" => Type::Cidr,
        "macaddr" => Type::MacAddr,
        "macaddr8" => Type::MacAddr8,
        "boolean" => Type::Bool,
        "json" => Type::JsonText,
        "jsonb" => Type::Json,
        "date" => Type::Date,
        "timestamp without time zone" => Type::Timestamp,
        "timestamp with time zone" => Type::TimestampTz,
        "interval" => Type::Interval,
        "time without time zone" => Type::Time,
        _ => return None,
    })
}

/// A name as a quoted SQL identifier.
fn ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// RFC 4180 records: (line the record starts on, fields). A field is None when empty and
/// unquoted (how `\copy ... csv` writes NULL), and Some("") when quoted empty.
struct Records<'a> {
    s: &'a [u8],
    i: usize,
    line: usize,
}

impl<'a> Records<'a> {
    fn new(s: &'a str) -> Records<'a> {
        Records { s: s.strip_prefix('\u{feff}').unwrap_or(s).as_bytes(), i: 0, line: 1 }
    }
}

impl Iterator for Records<'_> {
    type Item = Result<(usize, Vec<Option<String>>), String>;

    fn next(&mut self) -> Option<Self::Item> {
        let (s, start) = (self.s, self.line);
        if self.i >= s.len() {
            return None;
        }
        let (mut fields, mut field, mut quoted) = (vec![], Vec::new(), false);
        loop {
            match s.get(self.i) {
                Some(b'"') if field.is_empty() && !quoted => {
                    quoted = true;
                    self.i += 1;
                    loop {
                        match s.get(self.i) {
                            None => return Some(Err(format!("line {start}: a quoted field never ends"))),
                            Some(b'"') if s.get(self.i + 1) == Some(&b'"') => {
                                field.push(b'"');
                                self.i += 2;
                            }
                            Some(b'"') => {
                                self.i += 1;
                                break;
                            }
                            Some(&c) => {
                                self.line += (c == b'\n') as usize;
                                field.push(c);
                                self.i += 1;
                            }
                        }
                    }
                    if !matches!(s.get(self.i), None | Some(b',' | b'\n' | b'\r')) {
                        return Some(Err(format!("line {}: text after a closing quote", self.line)));
                    }
                }
                Some(b',') => {
                    fields.push(take(&mut field, quoted));
                    quoted = false;
                    self.i += 1;
                }
                None | Some(b'\n' | b'\r') => {
                    fields.push(take(&mut field, quoted));
                    if s.get(self.i) == Some(&b'\r') {
                        self.i += 1;
                    }
                    if s.get(self.i) == Some(&b'\n') {
                        self.i += 1;
                    }
                    self.line += 1;
                    return Some(Ok((start, fields)));
                }
                Some(&c) => {
                    field.push(c);
                    self.i += 1;
                }
            }
        }
    }
}

fn take(field: &mut Vec<u8>, quoted: bool) -> Option<String> {
    let f = std::mem::take(field);
    (quoted || !f.is_empty()).then(|| String::from_utf8_lossy(&f).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all(s: &str) -> Vec<(usize, Vec<Option<String>>)> {
        Records::new(s).collect::<Result<_, _>>().unwrap()
    }

    #[test]
    fn csv_records() {
        let some = |s: &str| Some(s.to_string());
        let r = all("id,note\n1,\"a, \"\"b\"\"\"\n2,\n3,\"\"\n4,\"two\r\nlines\"\r\n5,x");
        assert_eq!(r[1], (2, vec![some("1"), some("a, \"b\"")]));
        assert_eq!(r[2], (3, vec![some("2"), None]), "empty unquoted is NULL");
        assert_eq!(r[3], (4, vec![some("3"), some("")]), "quoted empty is empty text");
        assert_eq!(r[4], (5, vec![some("4"), some("two\r\nlines")]));
        assert_eq!(r[5], (7, vec![some("5"), some("x")]), "line numbers count newlines inside quotes");
        assert!(Records::new("a,\"b").next().unwrap().is_err());
        assert!(Records::new("a,\"b\"c").next().unwrap().is_err());
    }

    #[test]
    fn schema_rules() {
        let (cols, warn) = columns("id,integer,NO,t\nname,text,YES,f\ntags,ARRAY,YES,f\n").unwrap();
        assert_eq!(cols.len(), 3);
        assert!(cols[0].key && cols[2].ty == Type::Text);
        assert_eq!(warn, ["warning: column tags has type ARRAY, imported as text"]);
        assert!(columns("id,integer,NO,f\n").unwrap_err().contains("no primary key"));
        assert_eq!(columns("a,integer,NO,t\nb,integer,NO,t\n").unwrap().0.len(), 2, "a key of several columns");
        assert!(columns("a,integer\n").unwrap_err().contains("expected 4 fields"));
        let (cols, warn) = columns("id,integer,NO,t\nprice,numeric,YES,f\n").unwrap();
        assert!(cols[1].ty == Type::Numeric && warn.is_empty(), "numeric stays exact: {warn:?}");
    }
}
