//! Second review, size caps: values built from untrusted SQL whose working form is many times
//! their text (array literals, JSON, list aggregates, case mapping) are refused at 1 GB with
//! 54000, as Postgres refuses past MaxAllocSize, instead of allocating tens of gigabytes and
//! killing the process. One test per finding; each trigger would crash without its fix.

use chronos::sql::Session;
use chronos::{Db, MAIN};

fn try_one(db: &Db, s: &mut Session, q: &str) -> Result<String, (&'static str, String)> {
    match s.run(db, q, &[]) {
        Ok(out) => Ok(out.last().unwrap().rows.first().map_or(String::new(), |r| r[0].to_text())),
        Err(e) => Err((e.code, e.message)),
    }
}

fn one(db: &Db, s: &mut Session, q: &str) -> String {
    try_one(db, s, q).unwrap_or_else(|(c, m)| panic!("{q:.200}: {c} {m}"))
}

fn code(db: &Db, s: &mut Session, q: &str) -> &'static str {
    match try_one(db, s, q) {
        Ok(v) => panic!("{q:.200}: should fail, gave {v:.100}"),
        Err((c, _)) => c,
    }
}

/// 600 million commas in an array literal were 600 million 32-byte values (a 32 GB vector):
/// refused once the items would pass 1 GB.
#[test]
fn array_literals_are_bounded() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    // (malformed, as Postgres says, at the first comma: no element is gathered)
    assert_eq!(code(&db, &mut s, "select ('{' || repeat(',', 600000000) || '}')::int[] is null"), "22P02");
    assert_eq!(one(&db, &mut s, "select cardinality('{1,2,NULL,\"a,b\"}'::text[])"), "4");
}

/// 175 million `[0],` (700 MB of text) were about 39 GB of serde_json values: refused by
/// counting the nodes before parsing.
#[test]
fn json_input_is_bounded() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    assert_eq!(code(&db, &mut s, "select ('[' || repeat('[0],', 175000000) || '0]')::jsonb is null"), "54000");
    assert_eq!(code(&db, &mut s, "select ('[' || repeat('[0],', 175000000) || '0]')::text::jsonb -> 0"), "54000");
    // commas inside strings aren't nodes
    assert_eq!(one(&db, &mut s, "select length(('[\"' || repeat(',', 10000000) || '\"]')::jsonb ->> 0)"), "10000000");
    assert_eq!(one(&db, &mut s, r#"select '{"a": [1, {"b": "x\"],"}]}'::jsonb -> 'a' -> 1 ->> 'b'"#), "x\"],");
}

/// A 1 MB separator over 20000 rows held 20 GB of items and built a 20 GB string; array_agg
/// and jsonb_agg of 1 MB values likewise: refused at 1 GB of items.
#[test]
fn list_aggregates_are_bounded() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    let rows = "from generate_series(1, 20000) g";
    assert_eq!(code(&db, &mut s, &format!("select length(string_agg(g::text, repeat('x', 1000000))) {rows}")), "54000");
    assert_eq!(code(&db, &mut s, &format!("select array_agg(repeat('x', 1000000)) is null {rows}")), "54000");
    assert_eq!(code(&db, &mut s, &format!("select jsonb_agg(repeat('x', 1000000)) is null {rows}")), "54000");
    assert_eq!(code(&db, &mut s, &format!("select json_agg(repeat('x', 1000000)) is null {rows}")), "54000");
    assert_eq!(
        one(&db, &mut s, "select string_agg(g::text, ',' order by g desc) from generate_series(1, 3) g"),
        "3,2,1"
    );
}

/// upper() of 800 MB of 'ɐ' (2 bytes, its capital 'Ɐ' 3) is 1.2 GB: past the value limit. (A case
/// maps one character to one, as in Postgres, so 1.5 times is the most a text can grow.)
#[test]
fn case_mapping_is_bounded() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    assert_eq!(code(&db, &mut s, "select length(upper(repeat('ɐ', 400000000)))"), "54000");
    assert_eq!(code(&db, &mut s, "select length(lower(repeat('Ⱥ', 400000000)))"), "54000");
    assert_eq!(one(&db, &mut s, "select upper('aΐ') || lower('İB')"), "Aΐib");
    assert_eq!(one(&db, &mut s, "select octet_length(upper(repeat('ɐ', 1000)))"), "3000");
}
