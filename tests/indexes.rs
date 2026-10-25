//! B-tree indexes (CREATE INDEX): the same answers as without them, kept up through every kind
//! of write, on branches, across merges and across a reopen.

use chronos::sql::{Output, Value, run};
use chronos::{Db, MAIN, Resolve};

fn on(db: &Db, branch: &str, q: &str) -> Output {
    run(db, branch, q, &[]).unwrap_or_else(|e| panic!("{q}: {e}")).pop().unwrap()
}

fn sql(db: &Db, q: &str) -> Output {
    on(db, MAIN, q)
}

fn fails(db: &Db, q: &str) -> (&'static str, String) {
    let e = run(db, MAIN, q, &[]).expect_err(q);
    (e.code, e.message)
}

fn rows(o: &Output) -> Vec<String> {
    o.rows
        .iter()
        .map(|r| {
            r.iter().map(|v| if *v == Value::Null { "null".into() } else { v.to_text() }).collect::<Vec<_>>().join("|")
        })
        .collect()
}

/// How many index entries an index has on a branch.
fn entries(db: &Db, branch: &str, table: &str, index: &str) -> usize {
    db.scan(branch, &format!("_sonos_idx/{table}/{index}/"), usize::MAX)
        .unwrap()
        .iter()
        .filter(|(k, _)| k.starts_with(&format!("_sonos_idx/{table}/{index}/")))
        .count()
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

const TABLE: &str =
    "create table t (id int primary key, a int, b text, c double precision, d date, e boolean, big bigint)";
const INDEXES: &str = "create index t_a on t (a);
    create index t_ba on t (b, a);
    create index on t (c);
    create index t_d on t (d);
    create index t_e_big on t (e, big)";

fn value(r: &mut Rng, col: &str) -> String {
    if r.below(8) == 0 {
        return "null".into();
    }
    match col {
        "a" => format!("{}", r.below(41) as i64 - 20),
        "b" => format!("'{}'", ["", "a", "ab", "abc", "b", "ba", "z", "é", "A"][r.below(9) as usize]),
        "c" => format!("{}", (r.below(401) as f64 - 200.0) / 8.0),
        "d" => format!("'2026-0{}-{:02}'", 1 + r.below(3), 1 + r.below(28)),
        "e" => ["true", "false"][r.below(2) as usize].into(),
        _ => format!("{}", r.below(1_000_000_007) as i64 - 500_000_000),
    }
}

fn row(r: &mut Rng, id: u64) -> String {
    let v: Vec<String> = ["a", "b", "c", "d", "e", "big"].iter().map(|c| value(r, c)).collect();
    format!("({id}, {})", v.join(", "))
}

/// A random condition an index could answer (or partly answer), plus sometimes one it can't.
fn condition(r: &mut Rng) -> String {
    let ops = ["=", "<", "<=", ">", ">="];
    let op = ops[r.below(5) as usize];
    let col = ["a", "b", "c", "d", "big"][r.below(5) as usize];
    let x = loop {
        let x = value(r, col);
        if x != "null" || r.below(20) == 0 {
            break x;
        }
    };
    let one = match r.below(9) {
        0 => format!("{x} {op} {col}"), // constant first
        1 => format!("{col} between {x} and {}", value(r, col)),
        2 => format!("{col} in ({x}, {}, {})", value(r, col), value(r, col)),
        3 => format!("{col} is null"),
        4 => format!("b like '{}%'", ["a", "ab", "b", "é", "z", "A", "a_"][r.below(7) as usize]),
        5 if col == "d" => format!("d < {x}::date + {}", r.below(20)), // worked out once
        5 => format!("{col} {op} ({x})"),
        6 => format!("a in (select a from t where b = {})", value(r, "b")),
        _ => format!("{col} {op} {x}"),
    };
    match r.below(5) {
        0 => format!("{one} and b = {}", value(r, "b")),
        1 => format!("{one} and e = {}", value(r, "e")),
        2 => format!("{one} and a > {} and a <= {}", value(r, "a"), value(r, "a")),
        3 => format!("{one} or a = 3"),
        _ => one,
    }
}

/// A random ORDER BY ... LIMIT query an index could answer in order. It selects only the ordered
/// columns: rows tied on them may come in any order, but the values can't.
fn ordered_query(r: &mut Rng) -> String {
    let dir = ["", " asc", " desc"][r.below(3) as usize];
    let (lim, off) = (r.below(12), r.below(4));
    let tail = |r: &mut Rng| if r.below(3) == 0 { format!(" offset {off}") } else { String::new() }.to_string() + "";
    let t = tail(r);
    match r.below(7) {
        0 => format!("select a from t order by a{dir} limit {lim}{t}"),
        1 => format!("select a from t where {} order by a{dir} limit {lim}{t}", condition(r)),
        2 => format!("select b, a from t where b = {} order by a{dir} limit {lim}{t}", value(r, "b")),
        3 => format!(
            "select big from t where e = {} and big > {} order by big{dir} limit {lim}",
            value(r, "e"),
            value(r, "big")
        ),
        4 => format!("select c from t where c <= {} order by c{dir} limit {lim}{t}", value(r, "c")),
        5 => format!("select b, a from t order by b{dir}, a{dir} limit {lim}{t}"),
        // a key the index doesn't keep: ties on the indexed one must all be read
        _ => format!("select a, id from t where a is not null order by a{dir}, id limit {lim}{t}"),
    }
}

fn same_answers(r: &mut Rng, with: &Db, without: &Db, branch: &str, n: usize) {
    for i in 0..n {
        let q =
            if i % 3 == 2 { ordered_query(r) } else { format!("select id from t where {} order by id", condition(r)) };
        let (a, b) = (run(with, branch, &q, &[]), run(without, branch, &q, &[]));
        match (a, b) {
            (Ok(mut a), Ok(mut b)) => assert_eq!(rows(&a.pop().unwrap()), rows(&b.pop().unwrap()), "{q}"),
            (Err(a), Err(b)) => assert_eq!(a.code, b.code, "{q}"),
            (a, b) => panic!("{q}: with index {:?}, without {:?}", a.map(|_| ()), b.map(|_| ())),
        }
    }
}

#[test]
fn indexes_give_the_same_answers_as_scans() {
    for seed in 1..=8 {
        differential(seed);
    }
}

fn differential(seed: u64) {
    let mut r = Rng(0x5eed * seed);
    let (with, without) = (Db::new(), Db::new());
    for db in [&with, &without] {
        sql(db, TABLE);
    }
    sql(&with, INDEXES);
    let writes = |r: &mut Rng, branch: &str, n: usize| {
        for _ in 0..n {
            let id = r.below(300);
            let q = match r.below(6) {
                0 | 1 => format!("insert into t values {} on conflict (id) do nothing", row(r, id)),
                2 => format!("update t set a = {}, b = {} where id = {id}", value(r, "a"), value(r, "b")),
                3 => format!("update t set c = c + 1, d = {} where a > {}", value(r, "d"), value(r, "a")),
                4 => format!("delete from t where id = {id}"),
                _ => format!("update t set big = {}, e = {} where id % 7 = {}", value(r, "big"), value(r, "e"), id % 7),
            };
            let (a, b) = (run(&with, branch, &q, &[]), run(&without, branch, &q, &[]));
            assert_eq!(a.is_ok(), b.is_ok(), "{q}");
        }
    };
    let rows_sql: Vec<String> = (0..200).map(|i| row(&mut r, i)).collect();
    for db in [&with, &without] {
        sql(db, &format!("insert into t values {}", rows_sql.join(", ")));
    }
    same_answers(&mut r, &with, &without, MAIN, 300);
    writes(&mut r, MAIN, 300);
    same_answers(&mut r, &with, &without, MAIN, 300);

    // a fork: its own writes, then main's, then a merge resolved each way
    for (i, how) in [Resolve::Fail, Resolve::Ours, Resolve::Theirs].into_iter().enumerate() {
        let b = format!("f{i}");
        for db in [&with, &without] {
            db.fork(MAIN, &b).unwrap();
        }
        writes(&mut r, &b, 80);
        same_answers(&mut r, &with, &without, &b, 100);
        writes(&mut r, MAIN, 40);
        same_answers(&mut r, &with, &without, MAIN, 100);
        let (a, z) = (with.merge(&b, how), without.merge(&b, how));
        assert_eq!(a.is_ok(), z.is_ok(), "merge {how:?}: {a:?} vs {z:?}");
        if a.is_err() {
            for db in [&with, &without] {
                db.discard(&b).unwrap();
            }
        }
        same_answers(&mut r, &with, &without, MAIN, 200);
    }
    // every row has exactly one entry per index, whatever the merges did
    let n = sql(&with, "select count(*) from t").rows[0][0].to_text().parse::<usize>().unwrap();
    for ix in ["t_a", "t_ba", "t_c_idx", "t_d", "t_e_big"] {
        assert_eq!(entries(&with, MAIN, "t", ix), n, "seed {seed}: {ix}");
    }
}

#[test]
fn an_index_made_on_a_branch_merges_whole() {
    let db = Db::new();
    sql(&db, "create table p (id int primary key, age int)");
    sql(&db, "insert into p values (1, 10), (2, 20)");
    db.fork(MAIN, "x").unwrap();
    on(&db, "x", "create index p_age on p (age)");
    on(&db, "x", "insert into p values (3, 30)");
    sql(&db, "insert into p values (4, 40)"); // main writes a row the branch never saw
    db.merge("x", Resolve::Fail).unwrap();
    assert_eq!(entries(&db, MAIN, "p", "p_age"), 4);
    assert_eq!(rows(&sql(&db, "select id from p where age > 15 order by id")), ["2", "3", "4"]);
    // and dropped on a branch
    db.fork(MAIN, "y").unwrap();
    on(&db, "y", "drop index p_age");
    db.merge("y", Resolve::Fail).unwrap();
    assert_eq!(entries(&db, MAIN, "p", "p_age"), 0);
}

#[test]
fn entries_follow_writes_and_are_hidden() {
    let db = Db::new();
    sql(&db, "create table p (id int primary key, age int, name text)");
    sql(&db, "insert into p values (1, 30, 'a'), (2, 40, 'b'), (3, null, 'c')");
    sql(&db, "create index p_age on p (age)");
    assert_eq!(entries(&db, MAIN, "p", "p_age"), 3, "built for rows already there, nulls included");
    sql(&db, "update p set age = 50 where id = 1");
    sql(&db, "delete from p where id = 2");
    db.put(MAIN, "p/4", r#"{"age": 45, "name": "d"}"#).unwrap(); // JSON writes keep indexes too
    assert_eq!(entries(&db, MAIN, "p", "p_age"), 3);
    assert_eq!(rows(&sql(&db, "select id from p where age >= 45 order by id")), ["1", "4"]);
    assert_eq!(rows(&sql(&db, "select id from p where age < 50")), ["4"]);
    assert_eq!(rows(&sql(&db, "select id from p where 45 = age")), ["4"]);

    // forks share the index and diff doesn't show its entries
    db.fork(MAIN, "x").unwrap();
    on(&db, "x", "update p set age = 10 where id = 4");
    assert_eq!(rows(&on(&db, "x", "select id from p where age < 20")), ["4"]);
    assert!(rows(&sql(&db, "select id from p where age < 20")).is_empty());
    let d = db.diff("x").unwrap();
    assert_eq!(d.len(), 1, "one row: its index entries are the database's own, not shown");
    assert_eq!(rows(&on(&db, "x", "diff")).len(), 1, "SQL's DIFF hides index entries");
    db.merge("x", Resolve::Fail).unwrap();
    assert_eq!(rows(&sql(&db, "select id from p where age < 20")), ["4"]);
}

#[test]
fn create_and_drop_rules() {
    let db = Db::new();
    sql(&db, "create table p (id int primary key, age int, doc jsonb, name text)");
    assert_eq!(sql(&db, "create index p_age on p (age)").tag, "CREATE INDEX");
    assert_eq!(fails(&db, "create index p_age on p (name)").0, "42P07");
    assert_eq!(sql(&db, "create index if not exists p_age on p (name)").tag, "CREATE INDEX");
    assert_eq!(fails(&db, "create index on p (nope)").0, "42703");
    assert_eq!(fails(&db, "create index on nope (age)").0, "42P01");
    assert_eq!(fails(&db, "create index on p (doc)").0, "0A000");
    assert_eq!(fails(&db, "create index on p using gin (name)").0, "0A000");
    sql(&db, "create index concurrently on p using btree (name desc nulls last, age asc)");
    sql(&db, "insert into p values (1, 5, null, 'x')");
    assert_eq!(entries(&db, MAIN, "p", "p_name_age_idx"), 1, "a name is made up when none is given");
    assert_eq!(sql(&db, "drop index p_name_age_idx").tag, "DROP INDEX");
    assert_eq!(entries(&db, MAIN, "p", "p_name_age_idx"), 0);
    assert_eq!(fails(&db, "drop index p_name_age_idx").0, "42704");
    assert_eq!(sql(&db, "drop index if exists p_name_age_idx").tag, "DROP INDEX");
    // dropping a column drops its indexes; renaming a table moves them
    sql(&db, "alter table p drop column age");
    assert_eq!(entries(&db, MAIN, "p", "p_age"), 0);
    sql(&db, "create index p_n on p (name)");
    sql(&db, "alter table p rename to q");
    assert_eq!(entries(&db, MAIN, "q", "p_n"), 1);
    assert_eq!(rows(&sql(&db, "select id from q where name >= 'x'")), ["1"]);
    sql(&db, "drop table q");
    assert_eq!(entries(&db, MAIN, "q", "p_n"), 0);
}

#[test]
fn unique_indexes() {
    let db = Db::new();
    sql(&db, "create table u (id int primary key, email text)");
    sql(&db, "insert into u values (1, 'a'), (2, 'a')");
    assert_eq!(fails(&db, "create unique index u_email on u (email)").0, "23505", "existing duplicates");
    sql(&db, "delete from u where id = 2");
    sql(&db, "create unique index u_email on u (email)");
    assert_eq!(fails(&db, "insert into u values (3, 'a')").0, "23505");
    sql(&db, "insert into u values (3, null), (4, null)"); // nulls never clash
    assert_eq!(rows(&sql(&db, "select id from u where email > 'A'")), ["1"]);
    sql(&db, "drop index u_email");
    sql(&db, "insert into u values (5, 'a')"); // the constraint went with it
}

#[test]
fn texts_compared_with_numbers_still_answer_right() {
    // SQL compares '05' with 5 as numbers, so a text index can't answer it: it must still be right
    let db = Db::new();
    sql(&db, "create table s (id int primary key, code text)");
    sql(&db, "insert into s values (1, '05'), (2, '10'), (3, '9')");
    sql(&db, "create index s_code on s (code)");
    assert_eq!(rows(&sql(&db, "select id from s where code = 5")), ["1"]);
    assert_eq!(rows(&sql(&db, "select id from s where code > 8 order by id")), ["2", "3"]);
    assert_eq!(rows(&sql(&db, "select id from s where code > '8' order by id")), ["3"]);
    // and an integer column against a fraction
    sql(&db, "create table n (id int primary key, x int)");
    sql(&db, "insert into n values (1, 5), (2, 6)");
    sql(&db, "create index n_x on n (x)");
    assert_eq!(rows(&sql(&db, "select id from n where x > 5.5")), ["2"]);
    assert_eq!(rows(&sql(&db, "select id from n where x <= 5.5")), ["1"]);
}

#[test]
fn big_ranges_and_reopen() {
    let dir = std::env::temp_dir().join(format!("chronos-idx-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    {
        let db = Db::open(&dir).unwrap();
        sql(&db, "create table big (id int primary key, v int)");
        let vals: Vec<String> = (0..5000).map(|i| format!("({i}, {})", i % 1000)).collect();
        sql(&db, &format!("insert into big values {}", vals.join(", ")));
        sql(&db, "create index big_v on big (v)");
        // a range over most of the table falls back to a scan and still answers right
        assert_eq!(sql(&db, "select count(*) from big where v >= 10").rows[0][0], Value::Int(4950));
        assert_eq!(sql(&db, "select count(*) from big where v between 10 and 12").rows[0][0], Value::Int(15));
        db.checkpoint().unwrap();
        sql(&db, "update big set v = -1 where id < 3");
    }
    let db = Db::open(&dir).unwrap();
    assert_eq!(rows(&sql(&db, "select id from big where v < 0 order by id")), ["0", "1", "2"]);
    assert_eq!(entries(&db, MAIN, "big", "big_v"), 5000);
    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
}

fn plan(db: &Db, q: &str, params: &[Value]) -> Vec<String> {
    let o = run(db, MAIN, q, params).unwrap_or_else(|e| panic!("{q}: {e}")).pop().unwrap();
    assert_eq!(o.tag, "EXPLAIN");
    assert_eq!(o.columns.as_ref().unwrap()[0].0, "QUERY PLAN");
    rows(&o)
}

#[test]
fn explain_shows_how_rows_are_found() {
    let db = Db::new();
    sql(&db, "create table t (id int primary key, v int, name text); create table o (id int primary key, t_id int)");
    let vals: Vec<String> = (0..3000).map(|i| format!("({i}, {}, 'n{}')", i % 500, i % 97)).collect();
    sql(&db, &format!("insert into t values {}", vals.join(", ")));
    sql(&db, "insert into o values (1, 5), (2, 7); create index t_v on t (v)");

    assert_eq!(
        plan(&db, "explain select id from t where v between 10 and 12", &[]),
        ["Index Scan using t_v on t  (rows=18)", "  Index Cond: (v >= 10) AND (v <= 12)"]
    );
    assert_eq!(
        plan(&db, "explain select * from t where id = 7", &[]),
        ["Index Scan using t_pkey on t", "  Index Cond: (id = 7)"]
    );
    assert_eq!(plan(&db, "explain select * from t where v > 5", &[]), ["Seq Scan on t"], "most of the table: a scan");
    assert_eq!(
        plan(&db, "explain select * from t where 3 > v and name = 'n1'", &[]),
        ["Index Scan using t_v on t  (rows=18)", "  Index Cond: (v < 3)"]
    );
    assert_eq!(
        plan(&db, "explain select name, count(*) from t group by name order by 2 desc limit 3", &[]),
        ["Limit", "  ->  Sort", "        ->  HashAggregate", "              ->  Seq Scan on t"]
    );
    // a join looks the other table up once per row
    let p = plan(&db, "explain select * from t join o on o.t_id = t.id where t.v < 3", &[]);
    assert_eq!(p[0], "Inner Join");
    assert!(p.iter().any(|l| l.contains("on o  (loops=")), "{p:?}");
    assert!(p.iter().any(|l| l.ends_with("(t_id = <joined row>)")), "{p:?}");
    // parameters, and Postgres's option list
    assert_eq!(
        plan(&db, "explain (analyze false, format text) select id from t where v = $1", &[Value::Int(4)])[0],
        "Index Scan using t_v on t  (rows=6)"
    );
    let p = plan(&db, "explain analyze select count(*) from t where v > 490", &[]);
    assert_eq!(p[0], "Aggregate  (actual rows=1)");
    assert_eq!(p[1], "  ->  Index Scan using t_v on t  (actual rows=54)");
    assert!(p.last().unwrap().starts_with("Execution Time: "));
    // EXPLAIN alone never writes; EXPLAIN ANALYZE does, as in Postgres
    assert_eq!(plan(&db, "explain delete from t where v = 499", &[])[0], "Delete on t");
    assert_eq!(sql(&db, "select count(*) from t where v = 499").rows[0][0], Value::Int(6));
    assert_eq!(plan(&db, "explain analyze delete from t where v = 499", &[])[0], "Delete on t  (actual rows=6)");
    assert_eq!(sql(&db, "select count(*) from t where v = 499").rows[0][0], Value::Int(0));
    // (the other formats too, as Postgres writes them)
    let json = sql(&db, "explain (format json) select * from t where id = 7").rows[0][0].to_text();
    assert!(json.contains("\"Node Type\": \"Index Scan"), "{json}");
    assert_eq!(fails(&db, "explain create table x (id int)").0, "0A000");
}

#[test]
fn order_by_limit_reads_the_index_in_order() {
    let db = Db::new();
    sql(&db, "create table ev (id int primary key, at int, kind text, note text)");
    let vals: Vec<String> =
        (0..5000).map(|i| format!("({i}, {}, '{}', 'n')", (i * 7919) % 10007, ["a", "b", "c"][i % 3])).collect();
    sql(&db, &format!("insert into ev values {}", vals.join(", ")));
    sql(&db, "insert into ev values (9000, null, 'a', 'n')");
    sql(&db, "create index ev_at on ev (at); create index ev_kind_at on ev (kind, at)");
    let p = plan(&db, "explain select id from ev order by at desc limit 3", &[]);
    assert_eq!(p, ["Limit", "  ->  Sort", "        ->  Index Scan Backward using ev_at on ev  (rows=3)"]);
    // nulls come first in DESC, last in ASC, as in Postgres
    assert_eq!(rows(&sql(&db, "select at from ev order by at desc limit 2")), ["null", "10006"]);
    assert_eq!(rows(&sql(&db, "select at from ev order by at limit 2")), ["0", "5"]);
    // an equality on the first index column, then the order of the next
    let p = plan(&db, "explain analyze select at from ev where kind = 'b' order by at desc limit 2", &[]);
    assert!(p[2].contains("Index Scan Backward using ev_kind_at on ev  (actual rows=2)"), "{p:?}");
    assert_eq!(rows(&sql(&db, "select at from ev where kind = 'b' order by at desc limit 2")), ["10005", "9995"]);
    // a WHERE the index can't answer is checked on each row read, until enough rows pass
    assert_eq!(rows(&sql(&db, "select at from ev where id % 1000 = 0 order by at limit 3 offset 1")).len(), 3);
    // an output name standing for another column isn't the column
    let p = plan(&db, "explain select note as at from ev order by at limit 1", &[]);
    assert!(p.iter().all(|l| !l.contains("Index Scan")), "{p:?}");
}

/// An index on an expression serves conditions on that expression; a partial index serves
/// queries that ask for its WHERE too, and holds entries only for those rows.
#[test]
fn expression_and_partial_indexes() {
    let db = Db::new();
    sql(&db, "create table u (id int primary key, email text, deleted boolean, score int)");
    let vals: Vec<String> =
        (1..=3000).map(|i| format!("({i}, 'User{i}@Mail.com', {}, {})", i % 10 == 0, i % 97)).collect();
    sql(&db, &format!("insert into u values {}", vals.join(",")));
    sql(&db, "create index u_lower on u (lower(email))");
    sql(&db, "create index u_live_score on u (score) where not deleted");
    assert_eq!(entries(&db, MAIN, "u", "u_lower"), 3000);
    assert_eq!(entries(&db, MAIN, "u", "u_live_score"), 2700, "only rows its WHERE holds for");

    let q = "select id from u where lower(email) = 'user42@mail.com'";
    assert_eq!(rows(&sql(&db, q)), ["42"]);
    assert!(
        plan(&db, &format!("explain {q}"), &[])[0].contains("Index Scan using u_lower"),
        "{:?}",
        plan(&db, &format!("explain {q}"), &[])
    );

    let q = "select count(*) from u where score = 5 and not deleted";
    assert_eq!(
        rows(&sql(&db, q)),
        [rows(&sql(&db, "select count(*) from u where score + 0 = 5 and not deleted"))[0].clone()]
    );
    assert!(plan(&db, &format!("explain {q}"), &[]).iter().any(|l| l.contains("u_live_score")));
    // without its WHERE, the partial index would miss the deleted rows: not used
    let q = "select count(*) from u where score = 5";
    assert!(!plan(&db, &format!("explain {q}"), &[]).iter().any(|l| l.contains("u_live_score")));
    assert_eq!(rows(&sql(&db, q)), [rows(&sql(&db, "select count(*) from u where score + 0 = 5"))[0].clone()]);

    // kept up by writes: an update moves a row into and out of the partial index
    sql(&db, "update u set deleted = true where id = 5");
    sql(&db, "update u set email = 'NEW@x.io' where id = 7");
    assert_eq!(entries(&db, MAIN, "u", "u_live_score"), 2699);
    assert_eq!(rows(&sql(&db, "select id from u where lower(email) = 'new@x.io'")), ["7"]);
    // renames and drops follow the columns their text names
    sql(&db, "alter table u rename column email to mail");
    assert_eq!(rows(&sql(&db, "select id from u where lower(mail) = 'new@x.io'")), ["7"]);
    sql(&db, "alter table u drop column deleted");
    assert_eq!(fails(&db, "drop index u_live_score").0, "42704", "the index on a dropped column went with it");
    assert_eq!(fails(&db, "create index bad on u ((random() * id))").0, "42P17");
    assert_eq!(fails(&db, "create unique index bad on u (score) where score > 3").0, "23505", "two rows score 5");
}

#[test]
fn joins_start_from_a_few_rows_in_hand() {
    // a CTE's or recursive step's rows are known: the join starts there and looks the big table
    // up by index, whichever way the FROM is written
    let db = Db::new();
    sql(&db, "create table e (id int primary key, src int, dst int)");
    sql(&db, "insert into e select g, g / 4, g from generate_series(1, 20000) g");
    sql(&db, "create index on e (src)");
    let p = plan(&db, "explain with few as (select 5 as n) select e.dst from e join few on e.src = few.n", &[]);
    assert!(p.iter().any(|l| l.contains("Index Scan using e_src_idx")), "{p:?}");
    let q = "with recursive sub(n) as (select 1000 union all select e.dst from e join sub on e.src = sub.n) select count(*) from sub";
    assert_eq!(rows(&sql(&db, q)), ["21"]); // 1000, its 4 children, their 16
    let p = plan(&db, &format!("explain {q}"), &[]);
    assert!(p.iter().any(|l| l.contains("Index Scan using e_src_idx")), "{p:?}");
}

#[test]
fn spatial_indexes_find_what_a_scan_finds() {
    let db = Db::new();
    sql(&db, "create table p (id int primary key, g geography)");
    sql(&db, "create table q (id int primary key, g geography)");
    // a grid over the whole world, every 2 degrees
    sql(
        &db,
        "insert into p select i, ST_MakePoint(-180 + (i % 180) * 2, -89 + (i / 180) * 2) from generate_series(0, 16199) i",
    );
    sql(&db, "insert into q select id, g from p");
    sql(&db, "create index on p using gist (g)");
    for (at, meters) in
        [("0 0", 300_000), ("179.5 10", 400_000), ("-179 -45", 250_000), ("10 88.5", 500_000), ("-122.4 37.8", 1)]
    {
        let q =
            |t: &str| format!("select id from {t} where ST_DWithin(g, 'POINT({at})'::geography, {meters}) order by id");
        assert_eq!(rows(&sql(&db, &q("p"))), rows(&sql(&db, &q("q"))), "near {at}");
    }
    let p = plan(&db, "explain select id from p where ST_DWithin(g, 'POINT(0 0)'::geography, 1000)", &[]);
    assert!(p[0].starts_with("Index Scan using p_g_idx"), "{p:?}");
    assert_eq!(fails(&db, "create index on p using gist (id)").0, "0A000");
    assert_eq!(fails(&db, "create index on p using spgist (g)").0, "0A000");
}

#[test]
fn ranges_on_two_index_columns() {
    // the second column's range is checked on each index entry, before the row is read
    let db = Db::new();
    sql(&db, "create table g (id int primary key, a int, b float, c text)");
    sql(&db, "insert into g select i, i % 97, (i % 13) * 1.5 - 4, 'r' || i from generate_series(1, 5000) i");
    sql(&db, "create table h (id int primary key, a int, b float, c text)");
    sql(&db, "insert into h select * from g");
    sql(&db, "insert into g values (9001, 10, null, 'n'), (9002, null, 2.0, 'm')");
    sql(&db, "insert into h values (9001, 10, null, 'n'), (9002, null, 2.0, 'm')");
    sql(&db, "create index on g (a, b)");
    for w in [
        "a between 10 and 20 and b > 0 and b <= 6",
        "a >= 90 and b < -1",
        "a < 5 and b >= 2",
        "a > 3 and a < 8 and b between -1 and 1.5",
    ] {
        let q = |t: &str| format!("select id from {t} where {w} order by id");
        assert_eq!(rows(&sql(&db, &q("g"))), rows(&sql(&db, &q("h"))), "{w}");
    }
}

#[test]
fn gin_indexes_find_what_a_scan_finds() {
    let db = Db::new();
    let words = [
        "cat", "cats", "dog", "rat", "race", "fat", "lazy", "running", "runner", "the", "and", "quick", "brown", "fox",
    ];
    let mut values = vec![];
    for i in 0..3000 {
        let body: Vec<&str> = (0..6).map(|j| words[(i * 7 + j * 11 + i / 13) % words.len()]).collect();
        values.push(format!("({i}, '{}')", body.join(" ")));
    }
    for t in ["d", "e"] {
        sql(&db, &format!("create table {t} (id int primary key, body text, tsv tsvector)"));
        sql(&db, &format!("insert into {t} (id, body) values {}", values.join(", ")));
        sql(&db, &format!("update {t} set tsv = to_tsvector('english', body)"));
    }
    sql(&db, "create index d_body on d using gin (to_tsvector('english', body))");
    sql(&db, "create index on d using gin (tsv)");
    let queries = [
        "cat & rat",
        "cat | dog",
        "run:*",
        "fat <-> cat",
        "quick & !fox",
        "!cat",
        "the",
        "lazi & (race | fox)",
        "zebra",
    ];
    let same = |db: &Db| {
        for q in queries {
            for (lhs, rhs) in
                [("to_tsvector('english', body)", "to_tsquery('english', $1)"), ("tsv", "to_tsquery('english', $1)")]
            {
                let sql_for = |t: &str| format!("select id from {t} where {lhs} @@ {rhs} order by id");
                let p = [Value::Text(q.into())];
                let (a, b) = (run(db, MAIN, &sql_for("d"), &p).unwrap(), run(db, MAIN, &sql_for("e"), &p).unwrap());
                assert_eq!(rows(&a[0]), rows(&b[0]), "{lhs} @@ {q}");
            }
        }
    };
    same(&db);
    let p = plan(
        &db,
        "explain select id from d where to_tsvector('english', body) @@ to_tsquery('english', 'cat & rat')",
        &[],
    );
    assert!(p[0].starts_with("Bitmap Index Scan using d_body"), "{p:?}");
    let p = plan(&db, "explain select id from d where tsv @@ 'rat'", &[]);
    assert!(p[0].starts_with("Bitmap Index Scan using d_tsv_idx"), "{p:?}");
    // kept up by writes, and by a merge
    for t in ["d", "e"] {
        sql(
            &db,
            &format!(
                "update {t} set body = 'zebra crossing', tsv = to_tsvector('english', 'zebra crossing') where id % 10 = 3"
            ),
        );
        sql(&db, &format!("delete from {t} where id % 10 = 4"));
    }
    same(&db);
    db.fork(MAIN, "w").unwrap();
    for t in ["d", "e"] {
        run(
            &db,
            "w",
            &format!("update {t} set body = 'cat rat', tsv = to_tsvector('english', 'cat rat') where id < 50"),
            &[],
        )
        .unwrap();
    }
    db.merge("w", chronos::Resolve::Fail).unwrap();
    same(&db);
    assert_eq!(fails(&db, "create index on d using gin (body)").0, "0A000", "gin needs a tsvector");
    sql(&db, "drop index d_body");
    same(&db);
}

#[test]
fn trigram_indexes_find_what_a_scan_finds() {
    let db = Db::new();
    let words = [
        "Hotel",
        "hotel",
        "Motel",
        "hostel",
        "Hôtel",
        "Paris",
        "Berlin",
        "grand",
        "one",
        "Paris-Hilton",
        "café",
        "ot%el",
        "abc_d",
        "x",
        "12345",
        "Straße",
    ];
    let mut values = vec![];
    for i in 0..3000 {
        let name: Vec<&str> = (0..1 + i % 3).map(|j| words[(i * 7 + j * 11 + i / 13) % words.len()]).collect();
        values.push(format!("({i}, '{}')", name.join(" ")));
    }
    values.push("(3000, null)".into());
    values.push("(3001, '')".into());
    for t in ["d", "e"] {
        sql(&db, &format!("create table {t} (id int primary key, name text)"));
        sql(&db, &format!("insert into {t} values {}", values.join(", ")));
    }
    sql(&db, "create index d_name_trgm on d using gin (name gin_trgm_ops)");
    sql(&db, "create index d_lower_trgm on d using gist (lower(name) gist_trgm_ops)");
    let conds = [
        "name % 'hotel'",
        "'hotel' % name",
        "name % 'motel paris'",
        "name % 'hôtel'",
        "name % 'x'",
        "name % ''",
        "name like '%otel%'",
        "name like '%Hotel%'",
        "name like '%otel Par%'",
        "name like 'grand%'",
        "name like '%c_d%'",
        "name like '%ot\\%el%'",
        "name like '%ab%'",
        "name like '%hôt%'",
        "name like '%straße%'",
        "name not like '%otel%'",
        "name ilike '%HOTEL%'",
        "name ilike '%HÔT%'",
        "name ilike '%paris-hil%'",
        "lower(name) like '%hôtel%'",
        "lower(name) like '%345%' and id > 100",
    ];
    let same = |db: &Db| {
        for c in conds {
            let q = |t: &str| format!("select id from {t} where {c} order by id");
            assert_eq!(rows(&sql(db, &q("d"))), rows(&sql(db, &q("e"))), "{c}");
        }
    };
    same(&db);
    for (c, ix) in [
        ("name % 'hotel'", "d_name_trgm"),
        ("name like '%otel%'", "d_name_trgm"),
        ("name ilike '%paris%'", "d_name_trgm"),
        ("lower(name) like '%paris%'", "d_lower_trgm"),
    ] {
        let p = plan(&db, &format!("explain select id from d where {c}"), &[]);
        assert!(p[0].starts_with(&format!("Bitmap Index Scan using {ix}")), "{c}: {p:?}");
    }
    // a pattern with no three letters in a row reads every row
    let p = plan(&db, "explain select id from d where name like '%ab%'", &[]);
    assert!(!p[0].starts_with("Bitmap Index Scan"), "{p:?}");
    assert_eq!(
        rows(&sql(
            &db,
            "select indexdef from pg_indexes where indexname in ('d_name_trgm', 'd_lower_trgm') order by 1"
        )),
        [
            "CREATE INDEX d_lower_trgm ON public.d USING gist (lower(name) gist_trgm_ops)",
            "CREATE INDEX d_name_trgm ON public.d USING gin (name gin_trgm_ops)",
        ]
    );
    // kept up by writes, and by a merge
    for t in ["d", "e"] {
        sql(&db, &format!("update {t} set name = 'Grand Hotel Paris' where id % 10 = 3"));
        sql(&db, &format!("delete from {t} where id % 10 = 4"));
    }
    same(&db);
    db.fork(MAIN, "w").unwrap();
    for t in ["d", "e"] {
        on(&db, "w", &format!("update {t} set name = 'motel one' where id < 50"));
        on(&db, "w", &format!("insert into {t} values (5000, 'Hotel Berlin')"));
    }
    db.merge("w", Resolve::Fail).unwrap();
    same(&db);
    assert_eq!(fails(&db, "create index on d using gin (id gin_trgm_ops)").0, "42804");
    assert_eq!(fails(&db, "create unique index on d using gin (name gin_trgm_ops)").0, "0A000");
    sql(&db, "drop index d_name_trgm");
    same(&db);
}

#[test]
fn text_key_ranges_read_a_slice_of_the_table() {
    let dir = std::env::temp_dir().join(format!("chronos-textkey-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Db::open(&dir).unwrap();
    sql(&db, "create table d (k text primary key, n int)");
    // enough rows that scans split into parts; keys that share starts with the bounds
    let key = |i: i64| format!("{:x}", i * 2_654_435_761 % 1_000_003);
    let vals: Vec<String> = (0..20_000).map(|i| format!("('{}', {i})", key(i))).collect();
    sql(&db, &format!("insert into d values {}", vals.join(", ")));
    sql(&db, "insert into d values ('a', -1), ('ab', -2), ('ab\u{10ffff}', -3), ('é', -4), ('b', -5)");
    db.checkpoint().unwrap();
    // changes since the checkpoint are read too
    sql(
        &db,
        "insert into d values ('ab0', -6), ('abz', -7); delete from d where k = 'b'; update d set n = -8 where k = 'a'",
    );
    for cond in [
        "k >= 'ab' and k < 'ac'",
        "k > 'ab' and k <= 'ac'",
        "k between 'a' and 'b'",
        "k > 'f'",
        "k <= '1'",
        "k >= 'a'",
        "k < 'a'",
        "k >= 'c' and k < 'b'",
        "k >= ''",
        "k > 'e' and n % 2 = 0",
    ] {
        let q = format!("select k, n from d where {cond} order by k");
        // the same condition, hidden from the planner: a scan
        let scan = format!("select k, n from d where {} order by k", cond.replace("k ", "k || '' "));
        assert_eq!(rows(&sql(&db, &q)), rows(&sql(&db, &scan)), "{cond}");
        let count = format!("select count(*), sum(n) from d where {cond}");
        assert_eq!(rows(&sql(&db, &count)), rows(&sql(&db, &count.replace("k ", "k || '' "))), "{cond}");
    }
    assert_eq!(
        plan(&db, "explain select * from d where k >= 'ab' and k < 'ac'", &[]),
        ["Index Scan using d_pkey on d", "  Index Cond: (k >= 'ab') AND (k < 'ac')"]
    );
    // in a join, and in writes
    assert_eq!(
        rows(&sql(&db, "select count(*) from d x join d y on y.k = x.k where x.k > 'f'")),
        rows(&sql(&db, "select count(*) from d where k || '' > 'f'"))
    );
    let before = sql(&db, "select count(*) from d where k || '' < 'b'").rows[0][0].clone();
    assert_eq!(sql(&db, "delete from d where k < 'b'").tag, format!("DELETE {}", before.to_text()));
    assert_eq!(rows(&sql(&db, "select count(*) from d where k || '' < 'b'")), ["0"]);
    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
}
