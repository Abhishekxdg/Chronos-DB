//! UPDATE ... FROM and DELETE ... USING run as one join (the FROM side once, a hash join on the
//! WHERE's equalities), not a correlated subquery per target row: a WITH query grouping a big
//! table joined to a few of its rows took minutes.

use chronos::sql::Session;
use chronos::{Db, MAIN};

fn col(s: &mut Session, db: &Db, q: &str) -> Vec<String> {
    s.run(db, q, &[])
        .unwrap()
        .pop()
        .unwrap()
        .rows
        .iter()
        .map(|r| r.iter().map(|v| v.to_text()).collect::<Vec<_>>().join("|"))
        .collect()
}

fn leads(db: &Db, s: &mut Session, n: usize) {
    col(s, db, "create table leads (id int primary key, source text, status text, score int, budget numeric)");
    col(
        s,
        db,
        &format!(
            "insert into leads select g, 'src' || (g % 8), (array['new','contacted','won','lost'])[1 + g % 4], 0, g % 1000 \
             from generate_series(1, {n}) g"
        ),
    );
}

/// The join form on 100k rows: once per statement, well within the timeout.
#[test]
fn update_from_a_with_query_is_fast() {
    let db = Db::new();
    let mut s = Session::new(MAIN);
    leads(&db, &mut s, 100_000);
    col(&mut s, &db, "set statement_timeout = '20s'");
    let t = std::time::Instant::now();
    col(
        &mut s,
        &db,
        "with r as (select source, count(*) as n from leads group by source) \
         update leads set score = r.n % 100 from r where r.source = leads.source and leads.id <= 4000",
    );
    col(
        &mut s,
        &db,
        "with w as (select source, sum(case when status = 'won' then 1 else 0 end)::float / count(*) as rate from leads group by source), \
         m as (select 'new' as status, 10 as score union all select 'contacted', 30 union all select 'won', 100 union all select 'lost', 0), \
         c as (select l.id, (m.score * (1 + w.rate))::int as ns from leads l join m on l.status = m.status join w on l.source = w.source) \
         update leads set score = c.ns from c where leads.id = c.id",
    );
    // the bound is for optimized builds; an unoptimized one is several times slower
    let bound = std::time::Duration::from_secs(if cfg!(debug_assertions) { 90 } else { 10 });
    assert!(t.elapsed() < bound, "took {:?}", t.elapsed());
    assert_eq!(col(&mut s, &db, "select count(*) from leads where score = 0"), ["25000"]); // 'lost'
}

/// Same rows as the correlated form; rows without a match untouched.
#[test]
fn update_from_matches_the_correlated_form() {
    let db = Db::new();
    let mut s = Session::new(MAIN);
    leads(&db, &mut s, 2000);
    col(&mut s, &db, "create table b (id int primary key, score int)");
    col(&mut s, &db, "insert into b select id, score from leads");
    col(
        &mut s,
        &db,
        "update b set score = (select count(*) from leads s where s.source = (select source from leads x where x.id = b.id)) % 7 where b.id <= 500",
    );
    let n = col(
        &mut s,
        &db,
        "with r as (select source, count(*) as n from leads group by source) \
         update leads set score = r.n % 7 from r where r.source = leads.source and leads.id <= 500",
    );
    assert!(n.is_empty());
    assert_eq!(
        col(&mut s, &db, "select count(*) from leads l join b on b.id = l.id where b.score = l.score"),
        ["2000"]
    );
    assert_eq!(col(&mut s, &db, "select count(*) from leads where id > 500 and score <> 0"), ["0"]);
}

/// Unmatched rows, a target-only predicate, several matches (one change), an alias, RETURNING.
#[test]
fn update_from_semantics() {
    let db = Db::new();
    let mut s = Session::new(MAIN);
    col(&mut s, &db, "create table t (id int primary key, k text, v int)");
    col(&mut s, &db, "insert into t values (1, 'a', 0), (2, 'b', 0), (3, 'z', 0), (4, 'a', 0)");
    col(&mut s, &db, "create table f (k text, w int)");
    col(&mut s, &db, "insert into f values ('a', 5), ('b', 6), ('b', 6)");
    let out = s.run(&db, "update t as x set v = f.w from f where f.k = x.k and x.id < 4", &[]).unwrap();
    assert_eq!(out.last().unwrap().tag, "UPDATE 2");
    assert_eq!(col(&mut s, &db, "select id, v from t order by id"), ["1|5", "2|6", "3|0", "4|0"]);
    assert_eq!(
        col(&mut s, &db, "update t set v = f.w + 1 from f where t.k = f.k and t.id = 4 returning id, v"),
        ["4|6"]
    );
    // a unique violation is still one
    col(&mut s, &db, "create table u (id int primary key, n int unique)");
    col(&mut s, &db, "insert into u values (1, 1), (2, 2)");
    assert_eq!(s.run(&db, "update u set n = f.w from f where f.w = 5", &[]).unwrap_err().code, "23505");
}

#[test]
fn delete_using() {
    let db = Db::new();
    let mut s = Session::new(MAIN);
    leads(&db, &mut s, 20_000);
    col(&mut s, &db, "set statement_timeout = '20s'");
    let out = s
        .run(
            &db,
            "with r as (select source from leads group by source having source in ('src1', 'src2')) \
             delete from leads l using r where r.source = l.source and l.id <= 1000",
            &[],
        )
        .unwrap();
    assert_eq!(out.last().unwrap().tag, "DELETE 250");
    assert_eq!(col(&mut s, &db, "select count(*) from leads"), ["19750"]);
}
