//! CHRONOS_QUERY_MEM: every statement's working memory is capped (by default a quarter of the
//! machine's), not only an agent's with max_memory_mb, so a runaway join fails alone with 53200
//! instead of taking the server's memory from every session. Its own process: the variable is
//! read once.

use chronos::sql::{Session, run};
use chronos::{AgentSpec, Can, Db, MAIN};

fn table(db: &Db, t: &str, n: usize, groups: usize) {
    run(db, MAIN, &format!("create table {t} (id int primary key, k int, v text)"), &[]).unwrap();
    for chunk in (0..n).collect::<Vec<_>>().chunks(1000) {
        let rows: Vec<String> = chunk.iter().map(|i| format!("({i}, {}, 'value {i}')", i % groups)).collect();
        run(db, MAIN, &format!("insert into {t} values {}", rows.join(", ")), &[]).unwrap();
    }
}

#[test]
fn every_query_has_a_memory_limit() {
    // SAFETY: set before anything reads it, in this test's own process
    unsafe { std::env::set_var("CHRONOS_QUERY_MEM", "16MB") };
    let db = Db::new();
    table(&db, "a", 2000, 20);
    table(&db, "b", 2000, 20);
    // 200,000 joined rows held: far past 16 MB, run by the database's own user (no agent)
    let join = "select a.id, b.id, a.v from a join b on a.k = b.k";
    let e = run(&db, MAIN, join, &[]).unwrap_err();
    assert_eq!(e.code, "53200", "{}", e.message);
    assert!(e.message.contains("CHRONOS_QUERY_MEM") && e.message.contains("16 MB"), "{}", e.message);
    // what fits runs, and the next statement isn't charged for the refused one
    // (a count over the big join holds its rows while counting: past 16 MB or not, depending on
    // how many cores share it)
    assert_eq!(
        run(&db, MAIN, "select count(*) from a join b on a.id = b.id", &[]).unwrap()[0].rows[0][0].to_text(),
        "2000"
    );
    assert_eq!(run(&db, MAIN, "select id from a where k = 3 order by id limit 2", &[]).unwrap()[0].rows.len(), 2);
    // an agent's own lower max_memory_mb still says so
    db.create_agent("small", &AgentSpec { can: Some(vec![Can::Read]), max_memory_mb: Some(4), ..Default::default() })
        .unwrap();
    let mut s = Session::new(MAIN);
    s.agent = Some("small".into());
    let e = s.run(&db, join, &[]).unwrap_err();
    assert_eq!(e.code, "53200", "{}", e.message);
    assert!(e.message.contains("max_memory_mb") && e.message.contains("4 MB"), "{}", e.message);
}
