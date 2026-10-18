//! What ORMs and admin tools read from the catalog, as Postgres answers it: `attrelid = 'customers'::regclass` (psql's
//! own per-table queries name the table by number and are answered separately), `unnest(conkey)` joined to
//! `pg_attribute` inside a correlated subquery, `pg_stat_user_tables`, `pg_roles` for the session's user,
//! `pg_get_indexdef(oid, 0, true)` and `information_schema.check_constraints`. Postgres's answers are compared in
//! tests/pgdiff.rs (`catalog_introspection_by_name`); these run without one.

use chronos::sql::{Session, Value};
use chronos::{Db, MAIN};

/// What a query gives back, a row a string, its columns joined with `|` and booleans as `t`/`f`
/// the way psql prints them.
fn rows(s: &mut Session, db: &Db, q: &str) -> Vec<String> {
    let out = s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {} {}", e.code, e.message));
    let text = |v: &Value| match v {
        Value::Bool(b) => (if *b { "t" } else { "f" }).to_string(),
        v => v.to_text(),
    };
    out.last().unwrap().rows.iter().map(|r| r.iter().map(text).collect::<Vec<_>>().join("|")).collect()
}

/// Statements run in order in one session: a query's rows, or the SQLSTATE of the statement
/// Postgres refuses.
fn script(steps: &[(&str, Result<&[&str], &str>)]) {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    for (q, want) in steps {
        match want {
            Ok(rows_want) => assert_eq!(&rows(&mut s, &db, q), rows_want, "{q}"),
            Err(code) => {
                let e = s.run(&db, q, &[]).expect_err(q);
                assert_eq!(&e.code, code, "{q}: {}", e.message);
            }
        }
    }
}

#[test]
fn catalog_introspection() {
    script(&[
        (
            r"CREATE TABLE customers (id serial PRIMARY KEY, email text NOT NULL UNIQUE, name varchar(100), created_at timestamptz DEFAULT now(), balance numeric(10,2) DEFAULT 0)",
            Ok(&[]),
        ),
        (
            r"CREATE TABLE orders (id serial PRIMARY KEY, customer_id integer NOT NULL REFERENCES customers(id) ON DELETE CASCADE, total numeric(12,2) NOT NULL CHECK (total >= 0), status text DEFAULT 'new', placed_at date)",
            Ok(&[]),
        ),
        (
            r"CREATE TABLE order_items (order_id integer, line_no integer, sku text NOT NULL, qty integer DEFAULT 1, PRIMARY KEY (order_id, line_no), FOREIGN KEY (order_id) REFERENCES orders(id))",
            Ok(&[]),
        ),
        (r"CREATE INDEX orders_customer_idx ON orders (customer_id)", Ok(&[])),
        (r"CREATE UNIQUE INDEX orders_status_placed_idx ON orders (status, placed_at)", Ok(&[])),
        (r"CREATE VIEW big_orders AS SELECT id, total FROM orders WHERE total > 100", Ok(&[])),
        (r"create table tt (id int, arr int[])", Ok(&[])),
        (r"create table uu (k int, v text)", Ok(&[])),
        (r"insert into tt values (1, array[10,20]), (2, array[30])", Ok(&[])),
        (r"insert into uu values (10,'a'),(20,'b'),(30,'c')", Ok(&[])),
        (
            r"SELECT constraint_name, check_clause FROM information_schema.check_constraints WHERE constraint_schema = current_schema() AND constraint_name NOT LIKE '%not_null' ORDER BY constraint_name",
            Ok(&[r"orders_total_check|((total >= (0)::numeric))"]),
        ),
        (
            r"SELECT a.attname, format_type(a.atttypid, a.atttypmod) FROM pg_attribute a WHERE a.attrelid = 'customers'::regclass AND a.attnum > 0 ORDER BY a.attnum",
            Ok(&[
                r"id|integer",
                r"email|text",
                r"name|character varying(100)",
                r"created_at|timestamp with time zone",
                r"balance|numeric(10,2)",
            ]),
        ),
        (
            r"SELECT conname, pg_get_constraintdef(oid) FROM pg_constraint WHERE conrelid = 'orders'::regclass ORDER BY conname",
            Ok(&[
                r"orders_customer_id_fkey|FOREIGN KEY (customer_id) REFERENCES customers(id) ON DELETE CASCADE",
                r"orders_pkey|PRIMARY KEY (id)",
                r"orders_total_check|CHECK ((total >= (0)::numeric))",
            ]),
        ),
        (r"SELECT rolsuper, rolcanlogin FROM pg_roles WHERE rolname = current_user", Ok(&[r"t|t"])),
        (
            r"SELECT relname FROM pg_stat_user_tables WHERE schemaname = current_schema() ORDER BY relname",
            Ok(&[r"customers", r"order_items", r"orders", r"tt", r"uu"]),
        ),
        (
            r"SELECT a.attname, pg_catalog.format_type(a.atttypid, a.atttypmod), (SELECT pg_catalog.pg_get_expr(d.adbin, d.adrelid, true) FROM pg_catalog.pg_attrdef d WHERE d.adrelid = a.attrelid AND d.adnum = a.attnum AND a.atthasdef), a.attnotnull, (SELECT c.collname FROM pg_catalog.pg_collation c, pg_catalog.pg_type t WHERE c.oid = a.attcollation AND t.oid = a.atttypid AND a.attcollation <> t.typcollation) AS attcollation, a.attidentity, a.attgenerated FROM pg_catalog.pg_attribute a WHERE a.attrelid = 'customers'::regclass AND a.attnum > 0 AND NOT a.attisdropped ORDER BY a.attnum",
            Ok(&[
                r"id|integer|nextval('customers_id_seq'::regclass)|t|||",
                r"email|text||t|||",
                r"name|character varying(100)||f|||",
                r"created_at|timestamp with time zone|now()|f|||",
                r"balance|numeric(10,2)|0|f|||",
            ]),
        ),
        (
            r"SELECT c2.relname, i.indisprimary, i.indisunique, i.indisclustered, i.indisvalid, pg_catalog.pg_get_indexdef(i.indexrelid, 0, true), pg_catalog.pg_get_constraintdef(con.oid, true), contype, condeferrable, condeferred, i.indisreplident, c2.reltablespace FROM pg_catalog.pg_class c, pg_catalog.pg_class c2, pg_catalog.pg_index i LEFT JOIN pg_catalog.pg_constraint con ON (conrelid = i.indrelid AND conindid = i.indexrelid AND contype IN ('p','u','x')) WHERE c.oid = 'orders'::regclass AND c.oid = i.indrelid AND i.indexrelid = c2.oid ORDER BY i.indisprimary DESC, c2.relname",
            Ok(&[
                r"orders_pkey|t|t|f|t|CREATE UNIQUE INDEX orders_pkey ON orders USING btree (id)|PRIMARY KEY (id)|p|f|f|f|0",
                r"orders_customer_idx|f|f|f|t|CREATE INDEX orders_customer_idx ON orders USING btree (customer_id)|||||f|0",
                r"orders_status_placed_idx|f|t|f|t|CREATE UNIQUE INDEX orders_status_placed_idx ON orders USING btree (status, placed_at)|||||f|0",
            ]),
        ),
        (
            r"SELECT conname, conrelid::pg_catalog.regclass AS ontable, pg_catalog.pg_get_constraintdef(oid, true) AS condef FROM pg_catalog.pg_constraint r WHERE r.conrelid = 'orders'::regclass AND r.contype = 'f' ORDER BY 1",
            Ok(&[
                r"orders_customer_id_fkey|orders|FOREIGN KEY (customer_id) REFERENCES customers(id) ON DELETE CASCADE",
            ]),
        ),
        (
            r"SELECT conname, conrelid::pg_catalog.regclass AS ontable, pg_catalog.pg_get_constraintdef(oid, true) AS condef FROM pg_catalog.pg_constraint WHERE confrelid = 'customers'::regclass AND contype = 'f' ORDER BY 1",
            Ok(&[
                r"orders_customer_id_fkey|orders|FOREIGN KEY (customer_id) REFERENCES customers(id) ON DELETE CASCADE",
            ]),
        ),
        (
            r#"SELECT a.attname, pg_catalog.format_type(a.atttypid, a.atttypmod), pg_catalog.pg_get_expr(d.adbin, d.adrelid) AS "default", a.attnotnull, pgd.description AS comment FROM pg_catalog.pg_attribute a LEFT JOIN pg_catalog.pg_attrdef d ON (a.attrelid = d.adrelid AND a.attnum = d.adnum) LEFT JOIN pg_catalog.pg_description pgd ON (pgd.objoid = a.attrelid AND pgd.objsubid = a.attnum) WHERE a.attrelid = 'customers'::regclass AND a.attnum > 0 AND NOT a.attisdropped ORDER BY a.attnum"#,
            Ok(&[
                r"id|integer|nextval('customers_id_seq'::regclass)|t|",
                r"email|text||t|",
                r"name|character varying(100)||f|",
                r"created_at|timestamp with time zone|now()|f|",
                r"balance|numeric(10,2)|0|f|",
            ]),
        ),
        (
            r"SELECT a.attname FROM pg_index i JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = ANY(i.indkey) WHERE i.indrelid = 'order_items'::regclass AND i.indisprimary ORDER BY a.attnum",
            Ok(&[r"order_id", r"line_no"]),
        ),
        (
            r"SELECT c.conname, array(SELECT attname FROM unnest(c.conkey) WITH ORDINALITY cols(colid, arridx) JOIN pg_attribute AS ca ON cols.colid = ca.attnum WHERE ca.attrelid = c.conrelid ORDER BY cols.arridx), c.contype, (SELECT fkc.relname || '.' || fka.attname FROM pg_attribute AS fka JOIN pg_class AS fkc ON fka.attrelid = fkc.oid WHERE fka.attrelid = c.confrelid AND fka.attnum = c.confkey[1]), cl.reloptions FROM pg_constraint AS c JOIN pg_class AS cl ON c.conrelid = cl.oid WHERE cl.relname = 'orders' AND pg_catalog.pg_table_is_visible(cl.oid) ORDER BY c.conname",
            Ok(&[
                r"orders_customer_id_fkey|{customer_id}|f|customers.id|",
                r"orders_pkey|{id}|p||",
                r"orders_total_check|{total}|c||",
            ]),
        ),
        (
            r"SELECT a.attname AS column_name, NOT (a.attnotnull OR (t.typtype = 'd' AND t.typnotnull)) AS is_nullable, pg_get_expr(ad.adbin, ad.adrelid) AS column_default, CASE WHEN collname = 'default' THEN NULL ELSE collname END AS collation, a.attidentity != '' AS is_autofield, description AS comment FROM pg_attribute a LEFT JOIN pg_attrdef ad ON a.attrelid = ad.adrelid AND a.attnum = ad.adnum LEFT JOIN pg_collation co ON a.attcollation = co.oid JOIN pg_type t ON a.atttypid = t.oid JOIN pg_class c ON a.attrelid = c.oid JOIN pg_namespace n ON c.relnamespace = n.oid LEFT JOIN pg_description d ON d.objoid = a.attrelid AND d.objsubid = a.attnum WHERE c.relkind IN ('f','m','p','r','v') AND c.relname = 'customers' AND n.nspname NOT IN ('pg_catalog','pg_toast') AND pg_catalog.pg_table_is_visible(c.oid) AND a.attnum > 0 AND NOT a.attisdropped ORDER BY a.attnum",
            Ok(&[
                r"id|f|nextval('customers_id_seq'::regclass)||f|",
                r"email|f|||f|",
                r"name|t|||f|",
                r"created_at|t|now()||f|",
                r"balance|t|0||f|",
            ]),
        ),
        (
            r"select replace(pg_get_indexdef(indexrelid, 0, false), current_schema() || '.', 'S.') from pg_index where indrelid = 'orders'::regclass order by 1",
            Ok(&[
                r"CREATE INDEX orders_customer_idx ON S.orders USING btree (customer_id)",
                r"CREATE UNIQUE INDEX orders_pkey ON S.orders USING btree (id)",
                r"CREATE UNIQUE INDEX orders_status_placed_idx ON S.orders USING btree (status, placed_at)",
            ]),
        ),
        (
            r"select pg_get_indexdef(indexrelid, 0, true) from pg_index where indrelid = 'orders'::regclass order by 1",
            Ok(&[
                r"CREATE INDEX orders_customer_idx ON orders USING btree (customer_id)",
                r"CREATE UNIQUE INDEX orders_pkey ON orders USING btree (id)",
                r"CREATE UNIQUE INDEX orders_status_placed_idx ON orders USING btree (status, placed_at)",
            ]),
        ),
        (
            r"select relname, seq_scan, n_live_tup from pg_stat_user_tables where schemaname = current_schema() order by 1",
            // (counted, flushed at each transaction's end: tt and uu hold the rows inserted above)
            Ok(&[r"customers|0|0", r"order_items|0|0", r"orders|0|0", r"tt|0|2", r"uu|0|3"]),
        ),
        (
            r"select relname from pg_stat_all_tables where schemaname = current_schema() order by 1",
            Ok(&[r"customers", r"order_items", r"orders", r"tt", r"uu"]),
        ),
        (
            r"select relname, heap_blks_read from pg_statio_user_tables where schemaname = current_schema() order by 1",
            Ok(&[r"customers|0", r"order_items|0", r"orders|0", r"tt|0", r"uu|0"]),
        ),
        (
            r"select c.conname, (select array_agg(a.attname order by k.n) from unnest(c.conkey) with ordinality k(col, n) join pg_attribute a on a.attnum = k.col and a.attrelid = c.conrelid) from pg_constraint c where conname in ('customers_pkey','order_items_pkey','orders_pkey') order by 1",
            Ok(&[r"customers_pkey|{id}", r"order_items_pkey|{order_id,line_no}", r"orders_pkey|{id}"]),
        ),
        (
            r"select id, (select count(*) from unnest(arr) x(k) join uu on uu.k = x.k) from tt order by 1",
            Ok(&[r"1|2", r"2|1"]),
        ),
    ]);
}

/// An enum in public is named with its schema by the prepared statements' type lists, format_type and regtype when
/// public is off the search_path (pg_dump's empty one, or just pg_catalog), and bare when it is back, as Postgres
/// names it (pgdiff's scripts can't hold objects in public).
#[test]
fn a_public_enum_off_the_search_path_is_named_with_its_schema() {
    script(&[
        ("create type ci_mood as enum ('a')", Ok(&[])),
        (r#"create type "CiMood" as enum ('a')"#, Ok(&[])),
        ("create table ci_t (m ci_mood)", Ok(&[])),
        (r#"prepare cip (ci_mood, "CiMood", int) as select $1"#, Ok(&[])),
        (
            "select parameter_types, result_types from pg_prepared_statements",
            Ok(&[r#"{ci_mood,"\"CiMood\"",integer}|{ci_mood}"#]),
        ),
        ("set search_path = ''", Ok(&[])),
        (
            "select parameter_types, result_types from pg_catalog.pg_prepared_statements",
            Ok(&[r#"{public.ci_mood,"public.\"CiMood\"",integer}|{public.ci_mood}"#]),
        ),
        (
            "select pg_catalog.format_type(atttypid, atttypmod) from pg_catalog.pg_attribute \
             where attrelid = 'public.ci_t'::pg_catalog.regclass and attnum = 1",
            Ok(&["public.ci_mood"]),
        ),
        (
            r#"select 'public.ci_mood'::pg_catalog.regtype::text, 'public."CiMood"'::pg_catalog.regtype::text"#,
            Ok(&[r#"public.ci_mood|public."CiMood""#]),
        ),
        ("set search_path = pg_catalog", Ok(&[])),
        (
            "select parameter_types from pg_prepared_statements",
            Ok(&[r#"{public.ci_mood,"public.\"CiMood\"",integer}"#]),
        ),
        ("reset search_path", Ok(&[])),
        (
            "select parameter_types, 'ci_mood'::regtype::text from pg_prepared_statements",
            Ok(&[r#"{ci_mood,"\"CiMood\"",integer}|ci_mood"#]),
        ),
    ]);
}

/// A table made from the type lists of a statement taking a vector keeps them: a regtype[] column reads `vector`
/// as the type Chronos has, where it refuses a name of no type.
#[test]
fn a_kept_type_list_naming_a_vector() {
    script(&[
        ("prepare kvp (vector(3), int) as select $1", Ok(&[])),
        ("create table kvt as select parameter_types, result_types from pg_prepared_statements", Ok(&[])),
        ("insert into kvt values ('{vector,int4}')", Ok(&[])),
        ("insert into kvt values ('{nosuch}')", Err("42704")),
        ("select parameter_types, result_types from kvt", Ok(&["{vector,integer}|{vector}", "{vector,integer}|"])),
    ]);
}

/// A temporary table made from the type lists of a statement taking an enum keeps them: the enum is the session's,
/// read there, though the table lives in the temporary tables' database (once 42704; Postgres succeeds).
#[test]
fn a_temp_table_keeps_a_type_list_naming_an_enum() {
    script(&[
        ("create type tt_mood as enum ('a')", Ok(&[])),
        ("prepare ttp (tt_mood, int) as select $1", Ok(&[])),
        ("create temp table ttk as select parameter_types from pg_prepared_statements", Ok(&[])),
        ("insert into ttk values ('{tt_mood,int4}')", Ok(&[])),
        ("insert into ttk values ('{nosuch}')", Err("42704")),
        ("select parameter_types from ttk", Ok(&["{tt_mood,integer}", "{tt_mood,integer}"])),
    ]);
}

/// A table made from a catalog's regproc column keeps a regproc column, and DIFF ... AS SQL writes one Chronos reads
/// back: replayed into another database, the table and its row are as they were.
#[test]
fn a_kept_regproc_column_replays_from_diff_as_sql() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    rows(&mut s, &db, "create world rpb");
    let mut w = Session::new("rpb");
    rows(&mut w, &db, "create table rpz as select typname, typinput from pg_type where typname in ('int4', 'bool')");
    rows(&mut w, &db, "insert into rpz values ('sum', 2108)");
    let sql = rows(&mut w, &db, "diff world rpb as sql");
    assert!(sql.iter().any(|l| l.contains("typinput regproc")), "{sql:?}");
    let (other, mut o) = (Db::new(), Session::new(MAIN));
    for stmt in &sql {
        o.run(&other, stmt, &[]).unwrap_or_else(|e| panic!("{stmt}: {} {}", e.code, e.message));
    }
    let q = "select typname, typinput, typinput::oid from rpz order by typinput";
    assert_eq!(rows(&mut o, &other, q), ["int4|int4in|42", "bool|boolin|1242", "sum|pg_catalog.sum|2108"]);
}

/// A regproc of a function made here is the same function's oid in every world, whatever else each has made: a
/// merge carries it, and `DIFF ... AS SQL` replays it into a database that has other objects.
#[test]
fn a_regproc_of_a_function_made_here_is_the_same_in_every_world() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    rows(&mut s, &db, "create function rpm_f(int) returns int language sql as 'select 1'");
    rows(&mut s, &db, "create function rpm_g() returns int language sql as 'select 2'");
    rows(&mut s, &db, "create table rpm (id int primary key, f regproc, g regprocedure)");
    rows(&mut s, &db, "create world rpmw");
    // the parent makes objects of its own; the world writes regprocs of the functions both have
    rows(&mut s, &db, "create schema rpm_s");
    rows(&mut s, &db, "create table rpm_other (a int primary key)");
    let mut w = Session::new("rpmw");
    rows(&mut w, &db, "create table rpm_mine (a int)");
    rows(&mut w, &db, "insert into rpm values (1, 'rpm_f', 'rpm_g()'), (2, 'rpm_g', 'rpm_f(int)')");
    let sql = rows(&mut w, &db, "diff world rpmw as sql");
    rows(&mut s, &db, "merge world rpmw");
    let q = "select id, f, g, f = 'rpm_f'::regproc from rpm order by id";
    assert_eq!(rows(&mut s, &db, q), ["1|rpm_f|rpm_g()|t", "2|rpm_g|rpm_f(integer)|f"]);
    // replayed into a database that has other objects
    let (other, mut o) = (Db::new(), Session::new(MAIN));
    rows(&mut o, &other, "create table rpm_first (a int primary key, b int unique)");
    rows(&mut o, &other, "create function rpm_f(int) returns int language sql as 'select 1'");
    rows(&mut o, &other, "create function rpm_g() returns int language sql as 'select 2'");
    rows(&mut o, &other, "create table rpm (id int primary key, f regproc, g regprocedure)");
    for stmt in sql.iter().filter(|l| l.contains("into rpm ")) {
        o.run(&other, stmt, &[]).unwrap_or_else(|e| panic!("{stmt}: {} {}", e.code, e.message));
    }
    assert_eq!(rows(&mut o, &other, q), ["1|rpm_f|rpm_g()|t", "2|rpm_g|rpm_f(integer)|f"]);
}

/// A built-in function looked up in pg_proc by its oid or its name reads that row of the 3,286 alone: once for each
/// row of a table (a correlated subquery, as introspection queries do), and once a query, as pg_dump asks for each
/// function. Each such lookup made every built-in row again (about 60 ms each in a debug build: 18 s for these).
#[test]
fn built_in_function_lookups_read_their_rows_alone() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    rows(&mut s, &db, "create table fl (id int primary key, f regproc)");
    rows(&mut s, &db, "insert into fl select g, (1000 + g)::regproc from generate_series(1, 200) g");
    let t = std::time::Instant::now();
    let got = rows(&mut s, &db, "select count((select proname from pg_proc p where p.oid = fl.f)) from fl");
    assert_eq!(got, ["108"], "the built-in functions among oids 1001-1200");
    for o in 1001..1101 {
        rows(&mut s, &db, &format!("select proname from pg_proc where oid = {o}"));
    }
    let took = t.elapsed();
    assert!(took < std::time::Duration::from_secs(4), "{took:?}");
}

#[test]
fn a_temporary_table_is_in_the_catalog() {
    script(&[
        ("create table t (b text)", Ok(&[])),
        ("create temporary table t (a int primary key)", Ok(&[])),
        // found first by its bare name, as pg_temp is first on the search_path; the world's by its schema's
        ("select 't'::regclass = 'pg_temp.t'::regclass, 't'::regclass = 'public.t'::regclass", Ok(&["t|f"])),
        ("select 'public.t'::regclass::text", Ok(&["public.t"])),
        ("select to_regclass('t') is not null", Ok(&["t"])),
        (
            "select c.relname, c.relpersistence, n.nspname, pg_table_is_visible(c.oid) from pg_class c \
             join pg_namespace n on n.oid = c.relnamespace where c.relname in ('t', 't_pkey') order by 1, 2",
            Ok(&["t|p|public|f", "t|t|pg_temp_3|t", "t_pkey|t|pg_temp_3|t"]),
        ),
        ("select attname from pg_attribute where attrelid = 't'::regclass and attnum > 0", Ok(&["a"])),
        ("drop table t", Ok(&[])),
        ("select 't'::regclass::text", Ok(&["t"])),
    ]);
}

#[test]
fn an_update_or_delete_needs_the_replica_identity_its_publications_need() {
    script(&[
        ("create table keyed (a int primary key, b int)", Ok(&[])),
        ("create table plain (a int)", Ok(&[])),
        ("create publication by_b for table keyed where (b > 1)", Ok(&[])),
        ("create publication everything for table plain", Ok(&[])),
        // a WHERE that reads a column outside the replica identity, for a publication that publishes updates
        ("update keyed set b = 2", Err("42P10")),
        ("delete from keyed", Err("42P10")),
        ("alter table keyed replica identity full", Ok(&[])),
        ("update keyed set b = 2", Ok(&[])),
        // no replica identity at all
        ("update plain set a = 1", Err("55000")),
        ("delete from plain", Err("55000")),
        ("insert into plain values (1)", Ok(&[])),
        ("alter publication everything set (publish = 'insert')", Ok(&[])),
        ("delete from plain", Ok(&[])),
    ]);
}

#[test]
fn casts_written_as_calls_and_of_arrays() {
    script(&[
        (
            "select regclass('pg_class')::text, regtype('int4')::text, regoper('||/')::text",
            Ok(&["pg_class|integer|||/"]),
        ),
        (
            "select array[true, false]::text[], format('%s, %s', variadic array[true, null]::text[])",
            Ok(&["{true,false}|true, "]),
        ),
        ("select pg_char_to_encoding('utf-8'), pg_char_to_encoding('nonsense')", Ok(&["6|-1"])),
        ("select count(*) from pg_conversion where conproc = 'koi8r_to_mic'::regproc", Ok(&["1"])),
        ("select to_regrole('foo.bar') is null, to_regnamespace('foo.bar') is null", Ok(&["t|t"])),
        ("select 'ng_catalog.int4'::regtype", Err("3F000")),
        ("select to_regtype('ng_catalog.int4') is null", Ok(&["t"])),
    ]);
}

#[test]
fn storage_parameters_are_checked_and_kept() {
    script(&[
        (
            "create table r (i int, s text) with (FiLLFaCToR = 30, autovacuum_enabled = false, toast.vacuum_truncate = false)",
            Ok(&[]),
        ),
        ("select reloptions from pg_class where relname = 'r'", Ok(&["{fillfactor=30,autovacuum_enabled=false}"])),
        // SET keeps the others first, in their order; RESET takes away
        ("alter table r set (fillfactor = 31, autovacuum_analyze_scale_factor = 0.3)", Ok(&[])),
        (
            "select reloptions from pg_class where relname = 'r'",
            Ok(&["{autovacuum_enabled=false,fillfactor=31,autovacuum_analyze_scale_factor=0.3}"]),
        ),
        ("alter table r reset (fillfactor, autovacuum_enabled, autovacuum_analyze_scale_factor)", Ok(&[])),
        ("select reloptions is null from pg_class where relname = 'r'", Ok(&["t"])),
        ("alter table r reset (fillfactor = 12)", Err("42601")),
        ("create table r2 (i int) with (fillfactor = 2)", Err("22023")),
        ("create table r2 (i int) with (autovacuum_analyze_scale_factor = -10.0)", Err("22023")),
        ("create table r2 (i int) with (fillfactor = 'string')", Err("22023")),
        ("create table r2 (i int) with (fillfactor = 30, fillfactor = 40)", Err("22023")),
        ("create table r2 (i int) with (nope.fillfactor = 30)", Err("22023")),
        ("create table r2 (i int) with (not_an_option = 1)", Err("22023")),
        // an index's: its access method's
        ("create index r_s on r (s) with (fillfactor = 30)", Ok(&[])),
        ("alter index r_s set (fillfactor = 40)", Ok(&[])),
        ("select reloptions from pg_class where relname = 'r_s'", Ok(&["{fillfactor=40}"])),
        ("create index r_s2 on r (s) with (fillfactor = 1)", Err("22023")),
        ("create index r_s2 on r (s) with (deduplicate_items = maybe)", Err("22023")),
    ]);
}

#[test]
fn a_role_has_a_comment() {
    script(&[
        ("create role commented", Ok(&[])),
        ("comment on role commented is 'a role'", Ok(&[])),
        ("select shobj_description('commented'::regrole, 'pg_authid')", Ok(&["a role"])),
        ("alter role commented rename to commented2", Ok(&[])),
        ("select shobj_description('commented2'::regrole, 'pg_authid')", Ok(&["a role"])),
        ("comment on role commented2 is ''", Ok(&[])),
        ("select shobj_description('commented2'::regrole, 'pg_authid') is null", Ok(&["t"])),
        ("comment on role nobody is 'x'", Err("42704")),
    ]);
}

#[test]
fn create_table_as_needs_no_insert_on_the_table_it_makes() {
    script(&[
        ("create role maker", Ok(&[])),
        ("create schema m", Ok(&[])),
        ("grant all on schema m to public", Ok(&[])),
        ("alter default privileges for role maker revoke insert on tables from maker", Ok(&[])),
        ("set role maker", Ok(&[])),
        ("create table m.made (a) as select generate_series(1, 3)", Ok(&[])),
        ("insert into m.made values (4)", Err("42501")),
        ("select count(*) from m.made", Ok(&["3"])),
    ]);
}

#[test]
fn a_catalog_table_takes_grants() {
    script(&[
        ("create role reader", Ok(&[])),
        ("grant select on pg_proc to reader", Ok(&[])),
        ("grant select (prosrc) on pg_proc to reader", Ok(&[])),
        ("grant select (rolname, rolsuper) on pg_authid to reader", Ok(&[])),
        ("grant select (nope) on pg_proc to reader", Err("42703")),
    ]);
}

#[test]
fn admin_option_reaches_through_any_membership() {
    script(&[
        ("create role boss createrole", Ok(&[])),
        ("set role boss", Ok(&[])),
        // boss made manager, so holds ADMIN OPTION on it (by a grant without INHERIT); manager made worker
        ("create role manager createrole", Ok(&[])),
        ("set role manager", Ok(&[])),
        ("create role worker", Ok(&[])),
        ("reset role", Ok(&[])),
        ("set role boss", Ok(&[])),
        ("select pg_has_role('boss', 'worker', 'usage with admin option')", Ok(&["t"])),
        ("drop role worker", Ok(&[])),
    ]);
}

#[test]
fn a_table_of_values_of_no_fixed_length_has_a_toast_table() {
    script(&[
        ("create table toasted (a text, c text storage plain)", Ok(&[])),
        ("create table fixed (a int, b timestamptz)", Ok(&[])),
        (
            "select relname, reltoastrelid <> 0 from pg_class where relname in ('toasted', 'fixed') order by 1",
            Ok(&["fixed|f", "toasted|t"]),
        ),
        ("select pg_typeof(datname)::text from pg_database limit 1", Ok(&["name"])),
    ]);
}

#[test]
fn a_publication_has_a_comment() {
    script(&[
        ("create publication commented", Ok(&[])),
        ("comment on publication commented is 'test publication'", Ok(&[])),
        ("select obj_description(p.oid, 'pg_publication') from pg_publication p", Ok(&["test publication"])),
        ("comment on publication nope is 'x'", Err("42704")),
    ]);
}

/// `pg_constraint`'s one-letter columns are of Postgres's `"char"` type (OID 18), not text, and so is `x::"char"`:
/// Prisma's schema engine reads `contype` only as one, and failed `prisma db push` on any database with a CHECK
/// constraint (its constraints query, as Prisma 6.2 sends it, is below).
#[test]
fn the_catalogs_one_letter_columns_are_char() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    rows(
        &mut s,
        &db,
        "create table parent (id int primary key); create table child (id int primary key, p int references parent on delete cascade, n int check (n > 0))",
    );
    let q = "SELECT schemainfo.nspname AS namespace, tableinfo.relname AS table_name, constr.conname AS constraint_name, \
             constr.contype AS constraint_type, pg_get_constraintdef(constr.oid) AS constraint_definition, \
             constr.condeferrable AS is_deferrable, constr.condeferred AS is_deferred \
             FROM pg_constraint constr JOIN pg_class AS tableinfo ON tableinfo.oid = constr.conrelid \
             JOIN pg_namespace AS schemainfo ON schemainfo.oid = tableinfo.relnamespace \
             WHERE schemainfo.nspname = ANY ($1) AND contype NOT IN ('p', 'u', 'f') \
             ORDER BY namespace, table_name, constr.contype, constraint_name";
    let out = s.run(&db, q, &[Value::Text("{public}".into())]).unwrap().pop().unwrap();
    let ty = |name: &str| out.columns.as_ref().unwrap().iter().find(|(n, _)| n == name).unwrap().1;
    assert_eq!(ty("constraint_type").oid(), 18);
    assert_eq!(out.rows.len(), 1);
    assert_eq!(out.rows[0][3], Value::Text("c".into()));
    let out = s
        .run(&db, "select contype, confdeltype, confupdtype, confmatchtype from pg_constraint where contype = 'f'", &[])
        .unwrap()
        .pop()
        .unwrap();
    assert!(out.columns.as_ref().unwrap().iter().all(|(_, t)| t.oid() == 18));
    assert_eq!(out.rows[0][..2], [Value::Text("f".into()), Value::Text("c".into())]);
    assert_eq!(
        rows(
            &mut s,
            &db,
            r#"select 'abc'::"char", pg_typeof('x'::"char"), pg_typeof(contype) from pg_constraint limit 1"#
        ),
        [r#"a|"char"|"char""#]
    );
}
