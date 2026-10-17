//! Schemas, search_path, regclass and the catalog's constraints and indexes: checked against a
//! real Postgres (each script on a fresh database of each, statement by statement, as
//! tests/pgdiff.rs does; skipped when there's no Postgres at host=/tmp), including the
//! introspection queries SQLAlchemy's and Prisma's Postgres drivers send; and schemas as objects
//! of a world, through forks, merges, DIFF, AS OF and reopening.

use chronos::sql::{Session, Value};
use chronos::{Db, MAIN};
use postgres::{Client, NoTls, SimpleQueryMessage};

/// The reference Postgres: its socket in /tmp, on the port PGPORT names (as psql takes it), else 5432.
fn oracle(dbname: &str) -> String {
    let port = std::env::var("PGPORT").unwrap_or_else(|_| "5432".into());
    format!("host=/tmp port={port} dbname={dbname}")
}

type Rows = Result<Vec<Vec<Option<String>>>, String>;

fn chronos(db: &Db, s: &mut Session, q: &str) -> Rows {
    let o = s.run(db, q, &[]).map_err(|e| format!("{} {}", e.code, e.message))?.pop().unwrap();
    Ok(o.rows.iter().map(|r| r.iter().map(|v| (*v != Value::Null).then(|| v.to_text())).collect()).collect())
}

fn postgres(c: &mut Client, q: &str) -> Rows {
    let msgs = c.simple_query(q).map_err(|e| e.code().map_or("?".into(), |c| c.code().to_string()))?;
    Ok(msgs
        .iter()
        .filter_map(|m| match m {
            SimpleQueryMessage::Row(r) => Some((0..r.len()).map(|i| r.get(i).map(String::from)).collect()),
            _ => None,
        })
        .collect())
}

fn same(a: &Rows, b: &Rows) -> bool {
    let cell = |a: &Option<String>, b: &Option<String>| match (a, b) {
        (Some(a), Some(b)) => {
            a == b || (a.as_str(), b.as_str()) == ("true", "t") || (a.as_str(), b.as_str()) == ("false", "f")
        }
        (None, None) => true,
        _ => false,
    };
    match (a, b) {
        (Ok(a), Ok(b)) => {
            a.len() == b.len()
                && a.iter().zip(b).all(|(x, y)| x.len() == y.len() && x.iter().zip(y).all(|(p, q)| cell(p, q)))
        }
        (Err(a), Err(b)) => a[..2] == b[..2], // the same class of error
        _ => false,
    }
}

/// Runs each script on a fresh Chronos database and a fresh Postgres database, comparing
/// statement by statement.
fn check(scripts: &[&[&str]]) {
    static ONE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
    let Ok(mut admin) = Client::connect(&oracle("postgres"), NoTls) else {
        eprintln!("skipped: no Postgres at host=/tmp");
        return;
    };
    let mut failures = vec![];
    for (n, script) in scripts.iter().enumerate() {
        // one statement per call: DROP DATABASE can't run in the implicit transaction of a batch
        admin.batch_execute("drop database if exists chronos_schemas with (force)").unwrap();
        admin.batch_execute("create database chronos_schemas").unwrap();
        let mut pg = Client::connect(&oracle("chronos_schemas"), NoTls).unwrap();
        pg.batch_execute("set timezone = 'UTC'").unwrap();
        let db = Db::new();
        let mut s = Session::new(MAIN);
        for q in *script {
            let (a, b) = (chronos(&db, &mut s, q), postgres(&mut pg, q));
            if !same(&a, &b) {
                failures.push(format!("script {n}: {q}\n  chronos:    {a:?}\n  postgres: {b:?}"));
            }
        }
    }
    let _ = admin.batch_execute("drop database if exists chronos_schemas with (force)");
    assert!(failures.is_empty(), "{} differences:\n{}", failures.len(), failures.join("\n"));
}

#[test]
fn schemas_and_search_path() {
    check(&[&[
        "create schema app",
        "create schema app",
        "create schema if not exists app",
        "create schema pg_nope",
        "create table app.t (id int primary key, v text unique)",
        "create table t (id int primary key, w text)",
        "insert into app.t values (1, 'a'), (2, 'b')",
        "insert into t values (1, 'public')",
        "select * from app.t order by id",
        "select t.v, p.w from app.t join public.t p using (id)",
        "update app.t set v = t.v || '!' where t.id = 1 returning *",
        "delete from app.t where id = 2 returning v",
        "select * from nope.t",
        "create table nope.t (id int)",
        "create table app.t (id int)",
        "show search_path",
        "select current_schema()",
        "set search_path to app, public",
        "show search_path",
        "select current_schema(), current_schemas(false)",
        "select * from t order by id",
        "select * from public.t",
        "create table u (id int primary key, t_id int references t, n int)",
        "insert into u values (1, 1, 5)",
        "insert into u values (2, 9, 5)",
        "select table_schema, table_name, table_type from information_schema.tables where table_schema in ('app', 'public') order by 1, 2",
        "create view v as select id, v from t",
        "select * from app.v",
        "create sequence s start 10",
        "select nextval('s'), nextval('app.s')",
        "create index u_n on u (n)",
        "select indexname from pg_indexes where schemaname = 'app' order by 1",
        "drop index app.u_n",
        "set search_path = public",
        "select * from t",
        "select * from u",
        "select count(*) from app.u",
        "select nspname from pg_namespace where nspname in ('app', 'public') order by 1",
        "select schema_name from information_schema.schemata where schema_name in ('app', 'public') order by 1",
        "drop schema app",
        "drop schema app cascade",
        "select * from app.t",
        "drop schema if exists app",
        "drop schema app",
        "select * from t",
    ]]);
}

#[test]
fn schema_objects() {
    check(&[&[
        "create schema app",
        "create type app.mood as enum ('sad', 'ok', 'happy')",
        "create table app.p (id int primary key, m app.mood)",
        "insert into app.p values (1, 'ok'), (2, 'happy')",
        "select id from app.p where m > 'sad' order by id",
        "create function app.twice(x int) returns int language sql as $$ select x * 2 $$",
        "select app.twice(21)",
        "select twice(21)",
        "set search_path to app, public",
        "select twice(4)",
        "create table q (id serial primary key, m mood default 'sad')",
        "insert into q (m) values (default), ('happy')",
        "select id, m from app.q order by id",
        "create materialized view mv as select count(*) as n from p",
        "select n from app.mv",
        "reset search_path",
        "select table_schema, table_name, column_name, udt_name from information_schema.columns where table_schema = 'app' and table_name = 'p' order by 3",
        "drop schema app cascade",
        "select count(*) from information_schema.tables where table_schema = 'app'",
    ]]);
}

#[test]
fn with_in_subqueries() {
    check(&[&[
        "create table n (id int primary key, v int)",
        "insert into n values (1, 10), (2, 20), (3, 30)",
        "select * from (with big as (select * from n where v > 10) select id from big) as q order by id",
        "select (with m as (select max(v) as x from n) select x from m)",
        "select id from n where id in (with w as (select 2 as k) select k from w)",
        "select exists (with w as (select 1) select * from w)",
        "with a as (select 1 as x) select * from (with b as (select x + 1 as y from a) select y from b) q",
        "insert into n with w as (select 4 as id, 40 as v) select * from w",
        "select count(*) from n",
    ]]);
}

#[test]
fn regclass() {
    check(&[&[
        "create schema app",
        "create table users (id int primary key, name text)",
        "create table app.items (id int primary key)",
        "select 'users'::regclass, 'public.users'::regclass::text, 'app.items'::regclass",
        "select 'users'::regclass::oid > 0, 'users'::regclass::oid = (select oid from pg_class where relname = 'users')",
        "select relname from pg_class where oid = 'app.items'::regclass",
        "select attname from pg_attribute where attrelid = 'users'::regclass and attnum > 0 order by attnum",
        "select to_regclass('users')::text, to_regclass('nope'), to_regclass('app.items')",
        "select 'nope'::regclass",
        "select 'users_pkey'::regclass",
        "select c.oid::regclass from pg_class c where c.relname = 'items'",
        "set search_path to app, public",
        "select 'items'::regclass, 'users'::regclass, cast('app.items' as regclass)",
        "select c.oid::regclass from pg_class c where c.relname = 'items'",
    ]]);
}

#[test]
fn constraint_and_index_catalogs() {
    check(&[&[
        "create schema app",
        "create table orgs (id int primary key, name text not null unique)",
        "create table app.users (id int primary key, org int references orgs on delete cascade, email text, age int check (age >= 0), \
         constraint users_email_key unique (email))",
        "create table lines (ord int, line int, qty int check (qty > 0 and qty < 1000), primary key (ord, line))",
        "create table ships (id int primary key, ord int, line int, foreign key (ord, line) references lines on delete set null)",
        "create index users_age_idx on app.users (age)",
        "create unique index orgs_lower_name on orgs (lower(name))",
        "create index ships_partial on ships (ord) where line > 0",
        // the audit's query: indexes joined to the constraints they back
        // (the user's schemas only: Postgres also counts its own catalogs')
        "select count(*) from pg_catalog.pg_index i join pg_catalog.pg_constraint c on c.conindid = i.indexrelid \
         join pg_catalog.pg_namespace n on n.oid = c.connamespace where n.nspname in ('public', 'app')",
        "select c.conname, i.indisprimary, i.indisunique from pg_index i join pg_constraint c on c.conindid = i.indexrelid \
         and c.conrelid = i.indrelid join pg_namespace n on n.oid = c.connamespace where n.nspname in ('public', 'app') order by 1",
        "select conname, contype, conkey, confkey, confdeltype, confupdtype, confmatchtype, conrelid::regclass::text, \
         confrelid::regclass::text from pg_constraint c join pg_namespace n on n.oid = c.connamespace \
         where n.nspname in ('public', 'app') order by 1",
        "select conname, pg_get_constraintdef(oid), pg_get_constraintdef(oid, true) from pg_constraint \
         where connamespace in (select oid from pg_namespace where nspname in ('public', 'app')) order by 1",
        "select c.relname, i.indisunique, i.indisprimary, i.indnatts, pg_get_indexdef(i.indexrelid), pg_get_indexdef(i.indexrelid, 1, true) \
         from pg_index i join pg_class c on c.oid = i.indexrelid join pg_class t on t.oid = i.indrelid \
         join pg_namespace n on n.oid = t.relnamespace where n.nspname in ('public', 'app') order by 1",
        "select a.attname from pg_index i join pg_attribute a on a.attrelid = i.indrelid and a.attnum = any(i.indkey) \
         where i.indexrelid = 'lines_pkey'::regclass order by 1",
        "select schemaname, tablename, indexname, indexdef from pg_indexes where schemaname in ('public', 'app') order by 3",
        "select constraint_schema, constraint_name, table_name, constraint_type, is_deferrable, initially_deferred \
         from information_schema.table_constraints where constraint_schema in ('public', 'app') \
         and constraint_name not like '%not_null' order by 2",
        "select constraint_name, table_schema, table_name, column_name, ordinal_position, position_in_unique_constraint \
         from information_schema.key_column_usage where table_schema in ('public', 'app') order by 1, 5",
        "select constraint_schema, constraint_name, unique_constraint_schema, unique_constraint_name, match_option, update_rule, \
         delete_rule from information_schema.referential_constraints order by 2",
        "select constraint_name, table_schema, table_name, column_name from information_schema.constraint_column_usage \
         where constraint_schema in ('public', 'app') order by 1, 4",
        "select constraint_name, check_clause from information_schema.check_constraints \
         where constraint_schema in ('public', 'app') and constraint_name not like '%not_null' order by 1",
        "select relname, relkind, relhasindex, relchecks from pg_class c join pg_namespace n on n.oid = c.relnamespace \
         where n.nspname in ('public', 'app') order by 1",
        "select c.relname, a.amname from pg_class c join pg_am a on a.oid = c.relam where c.relname in ('users_age_idx', 'orgs_pkey') order by 1",
    ]]);
}

/// The oids of the tables SQLAlchemy asks about (it sends them as a list).
const OIDS: &str = "(select c.oid from pg_catalog.pg_class c join pg_catalog.pg_namespace n on n.oid = c.relnamespace \
                    where n.nspname in ('public', 'app') and c.relkind = 'r')";

const SCHEMA: [&str; 7] = [
    "create schema app",
    "create table orgs (id int primary key, name text not null unique)",
    "create table app.users (id int primary key, org int references orgs on delete cascade, email text, age int check (age >= 0), \
     constraint users_email_key unique (email))",
    "create table lines (ord int, line int, qty int, primary key (ord, line))",
    "create table ships (id int primary key, ord int, line int, note text, foreign key (ord, line) references lines on delete set null)",
    "create index ships_ord_idx on ships (ord, line)",
    "create index users_lower_email on app.users (lower(email)) where age > 17",
];

#[test]
fn sqlalchemy_introspection() {
    // SQLAlchemy 2.0's PGDialect reflection queries, as it renders them (bound values inlined)
    let pk = |contype: &str| {
        format!(
            "SELECT attr.conrelid, array_agg(CAST(attr.attname AS TEXT) ORDER BY attr.ord) AS cols, attr.conname, \
             min(attr.description) AS description, max(attr.indnkeyatts) AS indnkeyatts, bool_and(attr.indnullsnotdistinct) AS indnullsnotdistinct \
             FROM (SELECT con.conrelid AS conrelid, con.conname AS conname, con.indnkeyatts AS indnkeyatts, \
             con.indnullsnotdistinct AS indnullsnotdistinct, con.description AS description, con.ord AS ord, pg_catalog.pg_attribute.attname AS attname \
             FROM pg_catalog.pg_attribute JOIN (SELECT pg_catalog.pg_constraint.conrelid AS conrelid, pg_catalog.pg_constraint.conname AS conname, \
             unnest(pg_catalog.pg_index.indkey) AS attnum, generate_subscripts(pg_catalog.pg_index.indkey, 1) AS ord, \
             pg_catalog.pg_index.indnkeyatts AS indnkeyatts, pg_catalog.pg_index.indnullsnotdistinct AS indnullsnotdistinct, \
             pg_catalog.pg_description.description AS description \
             FROM pg_catalog.pg_constraint JOIN pg_catalog.pg_index ON pg_catalog.pg_constraint.conindid = pg_catalog.pg_index.indexrelid \
             LEFT OUTER JOIN pg_catalog.pg_description ON pg_catalog.pg_description.objoid = pg_catalog.pg_constraint.oid \
             WHERE pg_catalog.pg_constraint.contype = '{contype}' AND pg_catalog.pg_constraint.conrelid IN {OIDS}) AS con \
             ON pg_catalog.pg_attribute.attnum = con.attnum AND pg_catalog.pg_attribute.attrelid = con.conrelid \
             WHERE con.conrelid IN {OIDS}) AS attr GROUP BY attr.conrelid, attr.conname ORDER BY attr.conrelid, attr.conname"
        )
    };
    let named = |q: String, cols: &str| format!("SELECT {cols} FROM ({q}) AS q ORDER BY 1, 2");
    let fks = "SELECT pg_catalog.pg_class.relname, pg_catalog.pg_constraint.conname, CASE WHEN (pg_catalog.pg_constraint.oid IS NOT NULL) \
               THEN pg_catalog.pg_get_constraintdef(pg_catalog.pg_constraint.oid, true) END AS anon_1, nsp_ref.nspname, pg_catalog.pg_description.description \
               FROM pg_catalog.pg_class LEFT OUTER JOIN pg_catalog.pg_constraint ON pg_catalog.pg_class.oid = pg_catalog.pg_constraint.conrelid \
               AND pg_catalog.pg_constraint.contype = 'f' LEFT OUTER JOIN pg_catalog.pg_class AS cls_ref ON cls_ref.oid = pg_catalog.pg_constraint.confrelid \
               LEFT OUTER JOIN pg_catalog.pg_namespace AS nsp_ref ON cls_ref.relnamespace = nsp_ref.oid \
               LEFT OUTER JOIN pg_catalog.pg_description ON pg_catalog.pg_description.objoid = pg_catalog.pg_constraint.oid \
               JOIN pg_catalog.pg_namespace ON pg_catalog.pg_namespace.oid = pg_catalog.pg_class.relnamespace \
               WHERE pg_catalog.pg_class.relkind = ANY (ARRAY['r', 'p', 'f']) AND pg_catalog.pg_namespace.nspname = 'app' \
               AND pg_catalog.pg_class.relname IN ('users') ORDER BY pg_catalog.pg_class.relname, pg_catalog.pg_constraint.conname";
    let visible_fks = fks
        .replace("pg_catalog.pg_namespace.nspname = 'app'", "pg_catalog.pg_table_is_visible(pg_catalog.pg_class.oid) AND pg_catalog.pg_namespace.nspname != 'information_schema'")
        .replace("IN ('users')", "IN ('ships', 'orgs', 'lines')");
    let checks = "SELECT pg_catalog.pg_class.relname, pg_catalog.pg_constraint.conname, CASE WHEN (pg_catalog.pg_constraint.oid IS NOT NULL) \
                  THEN pg_catalog.pg_get_constraintdef(pg_catalog.pg_constraint.oid, true) END AS anon_1, pg_catalog.pg_description.description \
                  FROM pg_catalog.pg_class LEFT OUTER JOIN pg_catalog.pg_constraint ON pg_catalog.pg_class.oid = pg_catalog.pg_constraint.conrelid \
                  AND pg_catalog.pg_constraint.contype = 'c' LEFT OUTER JOIN pg_catalog.pg_description ON pg_catalog.pg_description.objoid = pg_catalog.pg_constraint.oid \
                  JOIN pg_catalog.pg_namespace ON pg_catalog.pg_namespace.oid = pg_catalog.pg_class.relnamespace \
                  WHERE pg_catalog.pg_class.relkind = ANY (ARRAY['r', 'p', 'f']) AND pg_catalog.pg_namespace.nspname = 'app' \
                  AND pg_catalog.pg_class.relname IN ('users') ORDER BY pg_catalog.pg_class.relname, pg_catalog.pg_constraint.conname";
    let indexes = format!(
        "SELECT pg_catalog.pg_index.indrelid, cls_idx.relname AS relname_index, pg_catalog.pg_index.indisunique, \
         pg_catalog.pg_constraint.conrelid IS NOT NULL AS has_constraint, pg_catalog.pg_index.indoption, cls_idx.reloptions, pg_catalog.pg_am.amname, \
         CASE WHEN (pg_catalog.pg_index.indpred IS NOT NULL) THEN pg_catalog.pg_get_expr(pg_catalog.pg_index.indpred, pg_catalog.pg_index.indrelid) END AS filter_definition, \
         pg_catalog.pg_index.indnkeyatts, pg_catalog.pg_index.indnullsnotdistinct, idx_cols.elements, idx_cols.elements_is_expr \
         FROM pg_catalog.pg_index JOIN pg_catalog.pg_class AS cls_idx ON pg_catalog.pg_index.indexrelid = cls_idx.oid \
         JOIN pg_catalog.pg_am ON cls_idx.relam = pg_catalog.pg_am.oid \
         LEFT OUTER JOIN (SELECT idx_attr.indexrelid AS indexrelid, min(idx_attr.indrelid) AS min_1, \
         array_agg(idx_attr.element ORDER BY idx_attr.ord) AS elements, array_agg(idx_attr.is_expr ORDER BY idx_attr.ord) AS elements_is_expr \
         FROM (SELECT idx.indexrelid AS indexrelid, idx.indrelid AS indrelid, idx.ord AS ord, \
         CASE WHEN (idx.attnum = 0) THEN pg_catalog.pg_get_indexdef(idx.indexrelid, idx.ord + 1, true) \
         ELSE CAST(pg_catalog.pg_attribute.attname AS TEXT) END AS element, idx.attnum = 0 AS is_expr \
         FROM (SELECT pg_catalog.pg_index.indexrelid AS indexrelid, pg_catalog.pg_index.indrelid AS indrelid, \
         unnest(pg_catalog.pg_index.indkey) AS attnum, generate_subscripts(pg_catalog.pg_index.indkey, 1) AS ord \
         FROM pg_catalog.pg_index WHERE NOT pg_catalog.pg_index.indisprimary AND pg_catalog.pg_index.indrelid IN {OIDS}) AS idx \
         LEFT OUTER JOIN pg_catalog.pg_attribute ON pg_catalog.pg_attribute.attnum = idx.attnum AND pg_catalog.pg_attribute.attrelid = idx.indrelid \
         WHERE idx.indrelid IN {OIDS}) AS idx_attr GROUP BY idx_attr.indexrelid) AS idx_cols ON pg_catalog.pg_index.indexrelid = idx_cols.indexrelid \
         LEFT OUTER JOIN pg_catalog.pg_constraint ON pg_catalog.pg_index.indrelid = pg_catalog.pg_constraint.conrelid \
         AND pg_catalog.pg_index.indexrelid = pg_catalog.pg_constraint.conindid AND pg_catalog.pg_constraint.contype = ANY (ARRAY['p', 'u', 'x']) \
         WHERE pg_catalog.pg_index.indrelid IN {OIDS} AND NOT pg_catalog.pg_index.indisprimary \
         ORDER BY pg_catalog.pg_index.indrelid, cls_idx.relname"
    );
    let columns = "SELECT pg_catalog.pg_class.relname, pg_catalog.pg_attribute.attname AS name, \
                   pg_catalog.format_type(pg_catalog.pg_attribute.atttypid, pg_catalog.pg_attribute.atttypmod) AS format_type, \
                   (SELECT pg_catalog.pg_get_expr(pg_catalog.pg_attrdef.adbin, pg_catalog.pg_attrdef.adrelid) AS pg_get_expr_1 FROM pg_catalog.pg_attrdef \
                   WHERE pg_catalog.pg_attrdef.adrelid = pg_catalog.pg_attribute.attrelid AND pg_catalog.pg_attrdef.adnum = pg_catalog.pg_attribute.attnum \
                   AND pg_catalog.pg_attribute.atthasdef) AS \"default\", pg_catalog.pg_attribute.attnotnull AS not_null, pg_catalog.pg_class.relname AS table_name, \
                   pg_catalog.pg_description.description AS comment, pg_catalog.pg_attribute.attgenerated AS generated, \
                   (SELECT json_build_object('always', pg_catalog.pg_attribute.attidentity = 'a', 'start', pg_catalog.pg_sequence.seqstart) AS json_build_object_1 \
                   FROM pg_catalog.pg_sequence WHERE pg_catalog.pg_attribute.attidentity != '' AND pg_catalog.pg_sequence.seqrelid = \
                   CAST(CAST(pg_catalog.pg_get_serial_sequence(CAST(CAST(pg_catalog.pg_attribute.attrelid AS REGCLASS) AS TEXT), pg_catalog.pg_attribute.attname) AS REGCLASS) AS OID)) AS identity_options \
                   FROM pg_catalog.pg_class LEFT OUTER JOIN pg_catalog.pg_attribute ON pg_catalog.pg_class.oid = pg_catalog.pg_attribute.attrelid \
                   AND pg_catalog.pg_attribute.attnum > 0 AND NOT pg_catalog.pg_attribute.attisdropped \
                   LEFT OUTER JOIN pg_catalog.pg_description ON pg_catalog.pg_description.objoid = pg_catalog.pg_attribute.attrelid \
                   AND pg_catalog.pg_description.objsubid = pg_catalog.pg_attribute.attnum \
                   JOIN pg_catalog.pg_namespace ON pg_catalog.pg_namespace.oid = pg_catalog.pg_class.relnamespace \
                   WHERE pg_catalog.pg_class.relkind = ANY (ARRAY['r', 'p', 'f', 'v', 'm']) AND pg_catalog.pg_namespace.nspname = 'app' \
                   AND pg_catalog.pg_class.relname IN ('users') ORDER BY pg_catalog.pg_class.relname, pg_catalog.pg_attribute.attnum";
    let mut script: Vec<String> = SCHEMA.iter().map(|s| s.to_string()).collect();
    script.extend([
        "SELECT pg_catalog.pg_namespace.nspname FROM pg_catalog.pg_namespace WHERE pg_catalog.pg_namespace.nspname NOT LIKE 'pg_%' \
         ORDER BY pg_catalog.pg_namespace.nspname"
            .to_string(),
        "SELECT pg_catalog.pg_class.relname FROM pg_catalog.pg_class JOIN pg_catalog.pg_namespace ON pg_catalog.pg_namespace.oid = pg_catalog.pg_class.relnamespace \
         WHERE pg_catalog.pg_class.relkind = ANY (ARRAY['r', 'p']) AND pg_catalog.pg_class.relpersistence != 't' AND pg_catalog.pg_namespace.nspname = 'app'"
            .to_string(),
        "SELECT pg_catalog.pg_class.relname FROM pg_catalog.pg_class JOIN pg_catalog.pg_namespace ON pg_catalog.pg_namespace.oid = pg_catalog.pg_class.relnamespace \
         WHERE pg_catalog.pg_class.relkind = ANY (ARRAY['r', 'p']) AND pg_catalog.pg_class.relpersistence != 't' \
         AND pg_catalog.pg_table_is_visible(pg_catalog.pg_class.oid) AND pg_catalog.pg_namespace.nspname != 'pg_catalog' ORDER BY 1"
            .to_string(),
        named(pk("p"), "q.conrelid::regclass::text, q.cols, q.conname, q.description, q.indnkeyatts, q.indnullsnotdistinct"),
        named(pk("u"), "q.conrelid::regclass::text, q.cols, q.conname, q.description, q.indnkeyatts, q.indnullsnotdistinct"),
        fks.to_string(),
        visible_fks,
        checks.to_string(),
        named(indexes, "q.indrelid::regclass::text, q.relname_index, q.indisunique, q.has_constraint, q.reloptions, q.amname, \
                        q.filter_definition, q.indnkeyatts, q.elements, q.elements_is_expr"),
        columns.to_string(),
    ]);
    let script: Vec<&str> = script.iter().map(String::as_str).collect();
    check(&[&script]);
}

#[test]
fn prisma_introspection() {
    // Prisma's Postgres schema describer's queries ($1, the schemas, inlined as an array)
    let schemas = "ARRAY['public', 'app']";
    let namespaces = format!(
        "SELECT namespace.nspname as namespace_name FROM pg_namespace as namespace WHERE namespace.nspname = ANY ( {schemas} ) ORDER BY namespace_name"
    );
    let tables = format!(
        "SELECT tbl.relname AS table_name, namespace.nspname as namespace, (tbl.relhassubclass and tbl.relkind = 'p') as is_partition, \
         (tbl.relhassubclass and tbl.relkind = 'r') as has_subclass, tbl.relrowsecurity as has_row_level_security, reloptions, \
         obj_description(tbl.oid, 'pg_class') as description \
         FROM pg_class AS tbl INNER JOIN pg_namespace AS namespace ON namespace.oid = tbl.relnamespace \
         WHERE ( (tbl.relkind = 'r' AND tbl.relispartition = 'f') OR tbl.relkind = 'p' ) AND namespace.nspname = ANY ( {schemas} ) \
         ORDER BY namespace, table_name"
    );
    let constraints = format!(
        "SELECT schemainfo.nspname AS namespace, tableinfo.relname AS table_name, constr.conname AS constraint_name, \
         constr.contype AS constraint_type, pg_get_constraintdef(constr.oid) AS constraint_definition, \
         constr.condeferrable AS is_deferrable, constr.condeferred AS is_deferred \
         FROM pg_constraint constr JOIN pg_class AS tableinfo ON tableinfo.oid = constr.conrelid \
         JOIN pg_namespace AS schemainfo ON schemainfo.oid = tableinfo.relnamespace \
         WHERE schemainfo.nspname = ANY ( {schemas} ) AND contype NOT IN ('p', 'u', 'f') \
         ORDER BY namespace, table_name, constr.contype, constraint_name"
    );
    let columns = format!(
        "SELECT oid.namespace, info.table_name, info.column_name, format_type(att.atttypid, att.atttypmod) as formatted_type, \
         info.numeric_precision, info.numeric_scale, info.numeric_precision_radix, info.datetime_precision, info.data_type, \
         info.udt_schema as type_schema_name, info.udt_name as full_data_type, pg_get_expr(attdef.adbin, attdef.adrelid) AS column_default, \
         info.is_nullable, info.is_identity, info.character_maximum_length, col_description(att.attrelid, ordinal_position) AS description \
         FROM information_schema.columns info JOIN pg_attribute att ON att.attname = info.column_name \
         JOIN ( SELECT pg_class.oid, relname, pg_namespace.nspname as namespace FROM pg_class JOIN pg_namespace on pg_namespace.oid = pg_class.relnamespace \
         AND pg_namespace.nspname = ANY ( {schemas} ) WHERE reltype > 0 ) as oid on oid.oid = att.attrelid AND relname = info.table_name \
         AND namespace = info.table_schema LEFT OUTER JOIN pg_attrdef attdef ON attdef.adrelid = att.attrelid AND attdef.adnum = att.attnum \
         AND table_schema = namespace WHERE table_schema = ANY ( {schemas} ) ORDER BY namespace, table_name, ordinal_position"
    );
    let fks = format!(
        "SELECT con.oid AS \"con_id\", att2.attname AS \"child_column\", cl.relname AS \"parent_table\", att.attname AS \"parent_column\", \
         con.confdeltype, con.confupdtype, rel_ns.nspname AS \"referenced_schema_name\", conname AS constraint_name, child, parent, \
         table_name, namespace, condeferrable, condeferred \
         FROM (SELECT ns.nspname AS \"namespace\", unnest(con1.conkey) AS \"parent\", unnest(con1.confkey) AS \"child\", \
         cl.relname AS table_name, ns.nspname AS schema_name, generate_subscripts(con1.conkey, 1) AS colidx, con1.oid, con1.confrelid, \
         con1.conrelid, con1.conname, con1.confdeltype, con1.confupdtype, con1.condeferrable AS condeferrable, con1.condeferred AS condeferred \
         FROM pg_class cl join pg_constraint con1 on con1.conrelid = cl.oid join pg_namespace ns on cl.relnamespace = ns.oid \
         WHERE ns.nspname = ANY ( {schemas} ) and con1.contype = 'f' ORDER BY colidx ) con \
         JOIN pg_attribute att on att.attrelid = con.confrelid and att.attnum = con.child \
         JOIN pg_class cl on cl.oid = con.confrelid \
         JOIN pg_attribute att2 on att2.attrelid = con.conrelid and att2.attnum = con.parent \
         JOIN pg_class rel_cl on con.confrelid = rel_cl.oid \
         JOIN pg_namespace rel_ns on rel_cl.relnamespace = rel_ns.oid \
         ORDER BY namespace, table_name, constraint_name, con_id, con.colidx"
    );
    let indexes = format!(
        "WITH rawindex AS ( SELECT indrelid, indexrelid, indisunique, indisprimary, unnest(indkey) AS indkeyid, \
         generate_subscripts(indkey, 1) AS indkeyidx, unnest(indclass) AS indclass, unnest(indoption) AS indoption \
         FROM pg_index WHERE indpred IS NULL AND NOT indisexclusion ) \
         SELECT schemainfo.nspname AS namespace, indexinfo.relname AS index_name, tableinfo.relname AS table_name, \
         columninfo.attname AS column_name, rawindex.indisunique AS is_unique, rawindex.indisprimary AS is_primary_key, \
         rawindex.indkeyidx AS column_index, opclass.opcname AS opclass, opclass.opcdefault AS opcdefault, indexaccess.amname AS index_algo, \
         CASE rawindex.indoption & 1 WHEN 1 THEN 'DESC' ELSE 'ASC' END AS column_order, \
         CASE rawindex.indoption & 2 WHEN 2 THEN true ELSE false END AS nulls_first, pc.condeferrable AS condeferrable, pc.condeferred AS condeferred \
         FROM rawindex INNER JOIN pg_class AS tableinfo ON tableinfo.oid = rawindex.indrelid \
         INNER JOIN pg_class AS indexinfo ON indexinfo.oid = rawindex.indexrelid \
         INNER JOIN pg_namespace AS schemainfo ON schemainfo.oid = tableinfo.relnamespace \
         LEFT JOIN pg_attribute AS columninfo ON columninfo.attrelid = tableinfo.oid AND columninfo.attnum = rawindex.indkeyid \
         INNER JOIN pg_am AS indexaccess ON indexaccess.oid = indexinfo.relam \
         LEFT JOIN pg_opclass AS opclass ON opclass.oid = rawindex.indclass \
         LEFT JOIN pg_constraint pc ON rawindex.indexrelid = pc.conindid AND pc.contype <> 'f' \
         WHERE schemainfo.nspname = ANY ( {schemas} ) ORDER BY namespace, table_name, index_name, column_index"
    );
    let sequences = format!(
        "SELECT sequence_name, sequence_schema AS namespace, start_value::INT8, minimum_value::INT8, maximum_value::INT8, increment::INT8, \
         (CASE cycle_option WHEN 'yes' THEN TRUE ELSE FALSE END) AS cycle, 0::INT8 AS cache_size \
         FROM information_schema.sequences WHERE sequence_schema = ANY ( {schemas} ) ORDER BY sequence_name"
    );
    let views = format!(
        "SELECT viewinfo.table_name as view_name, viewinfo.table_schema as namespace, obj_description(pg_class.oid, 'pg_class') as description \
         FROM information_schema.views viewinfo JOIN pg_class ON pg_class.relname = viewinfo.table_name \
         JOIN pg_namespace ON pg_namespace.oid = pg_class.relnamespace AND pg_namespace.nspname = viewinfo.table_schema \
         WHERE viewinfo.table_schema = ANY ( {schemas} ) ORDER BY view_name"
    );
    let mut script: Vec<String> = SCHEMA.iter().map(|s| s.to_string()).collect();
    script.extend([
        "create sequence app.counter start 5 increment 5".to_string(),
        "create view app.adults as select id, email from app.users where age >= 18".to_string(),
        namespaces,
        tables,
        constraints,
        // columns: without its types' typmod-dependent text (varchar(n) and the like aren't here)
        columns,
        // foreign keys, with the oid left out
        format!(
            "SELECT child_column, parent_table, parent_column, confdeltype, confupdtype, referenced_schema_name, constraint_name, \
             child, parent, table_name, namespace, condeferrable, condeferred FROM ({fks}) AS q"
        ),
        // indexes, with the operator classes (which Chronos doesn't list) left out
        format!(
            "SELECT namespace, index_name, table_name, column_name, is_unique, is_primary_key, column_index, index_algo, column_order, \
             nulls_first, condeferrable, condeferred FROM ({indexes}) AS q"
        ),
        sequences,
        views,
    ]);
    let script: Vec<&str> = script.iter().map(String::as_str).collect();
    check(&[&script]);
}

// ---------------------------------------------------------------- schemas as world data

fn rows(db: &Db, s: &mut Session, q: &str) -> Vec<Vec<String>> {
    let o = s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {e}")).pop().unwrap();
    o.rows
        .iter()
        .map(|r| r.iter().map(|v| if *v == Value::Null { "null".into() } else { v.to_text() }).collect())
        .collect()
}

fn run(db: &Db, s: &mut Session, q: &str) {
    s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {e}"));
}

fn fails(db: &Db, s: &mut Session, q: &str) -> String {
    s.run(db, q, &[]).err().unwrap_or_else(|| panic!("{q} should fail")).code.to_string()
}

#[test]
fn schemas_fork_merge_travel_and_reopen() {
    let dir = std::env::temp_dir().join(format!("chronos-schemas-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Db::open(&dir).unwrap();
    let s = &mut Session::new(MAIN);
    run(&db, s, "create table t (id int primary key, v text)");
    run(&db, s, "insert into t values (1, 'public')");
    let before = rows(&db, s, "select now()::text")[0][0].clone();
    std::thread::sleep(std::time::Duration::from_millis(5));
    // a fork makes a schema and its tables; main doesn't see them until the merge
    run(&db, s, "create world w");
    run(&db, s, "switch world w");
    run(&db, s, "create schema app");
    run(&db, s, "create table app.t (id int primary key, v text, p int references public.t)");
    run(&db, s, "insert into app.t values (1, 'app', 1)");
    run(&db, s, "create view app.v as select v from app.t");
    run(&db, s, "create sequence app.s");
    let diff = rows(&db, s, "diff");
    let changes: Vec<(&str, &str)> = diff.iter().map(|r| (r[0].as_str(), r[2].as_str())).collect();
    assert!(changes.contains(&("app", "create schema")), "{changes:?}");
    assert!(changes.contains(&("app.t", "create table")) && changes.contains(&("app.t", "insert")), "{changes:?}");
    let sql = rows(&db, s, "diff as sql").into_iter().map(|r| r[0].clone()).collect::<Vec<_>>();
    assert_eq!(sql[0], "create schema app;", "{sql:?}");
    assert!(sql.iter().any(|q| q.starts_with("create table app.t (")), "{sql:?}");
    assert!(sql.iter().any(|q| q.starts_with("insert into app.t ")), "{sql:?}");
    run(&db, s, "switch world main");
    assert_eq!(fails(&db, s, "select * from app.t"), "42P01");
    assert_eq!(fails(&db, s, "create table app.x (id int)"), "3F000");
    // the diff as SQL makes the same schema elsewhere
    run(&db, s, "create world replay");
    run(&db, s, "switch world replay");
    for q in &sql {
        run(&db, s, q);
    }
    assert_eq!(rows(&db, s, "select v from app.t"), [["app"]]);
    run(&db, s, "switch world main");
    run(&db, s, "drop world replay");
    run(&db, s, "merge world w");
    assert_eq!(rows(&db, s, "select a.v, t.v from app.t a join t on t.id = a.p"), [["app", "public"]]);
    assert_eq!(rows(&db, s, "select * from app.v"), [["app"]]);
    assert_eq!(rows(&db, s, "select nspname from pg_namespace where nspname = 'app'"), [["app"]]);
    // a schema made on two sides merges as one
    run(&db, s, "create world a");
    run(&db, s, "create world b");
    run(&db, s, "switch world a");
    run(&db, s, "create schema other");
    run(&db, s, "create table other.x (id int primary key)");
    run(&db, s, "switch world b");
    run(&db, s, "create schema other");
    run(&db, s, "create table other.y (id int primary key)");
    run(&db, s, "switch world main");
    run(&db, s, "merge world a");
    run(&db, s, "merge world b");
    assert_eq!(
        rows(&db, s, "select table_name from information_schema.tables where table_schema = 'other' order by 1"),
        [["x"], ["y"]]
    );
    // AS OF: the schema's table wasn't there before
    assert_eq!(fails(&db, s, &format!("select * from app.t as of '{before}'")), "42P01");
    assert_eq!(rows(&db, s, "select count(*) from app.t as of '-0 seconds'"), [["1"]]);
    // search_path lives in the session
    run(&db, s, "set search_path to app, public");
    assert_eq!(rows(&db, s, "select v from t"), [["app"]]);
    drop(db);
    // after reopening, everything is where it was
    let db = Db::open(&dir).unwrap();
    let s = &mut Session::new(MAIN);
    assert_eq!(rows(&db, s, "select v from app.t"), [["app"]]);
    assert_eq!(rows(&db, s, "select v from t"), [["public"]]);
    assert!(rows(&db, s, "select nextval('app.s')")[0][0].parse::<i64>().unwrap() > 0);
    run(&db, s, "drop schema app cascade");
    assert_eq!(fails(&db, s, "select * from app.v"), "42P01");
    assert_eq!(rows(&db, s, "select v from t"), [["public"]]);
    drop(db);
    let db = Db::open(&dir).unwrap();
    let s = &mut Session::new(MAIN);
    assert_eq!(fails(&db, s, "select * from app.t"), "42P01");
    assert_eq!(rows(&db, s, "select count(*) from other.x"), [["0"]]);
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn search_path_changes_within_a_script() {
    let db = Db::new();
    let s = &mut Session::new(MAIN);
    // each statement sees what the ones before it did, as in Postgres
    let out = s
        .run(
            &db,
            "create schema app; set search_path to app, public; create table t (id int primary key); \
             insert into t values (7); select * from app.t; reset search_path; select count(*) from app.t",
            &[],
        )
        .map(|o| o.into_iter().map(|o| o.rows.len()).collect::<Vec<_>>());
    assert_eq!(out, Ok(vec![0, 0, 0, 0, 1, 0, 1]));
    assert_eq!(fails(&db, s, "select * from t"), "42P01"); // it went into app, not public
}
