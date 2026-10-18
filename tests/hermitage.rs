//! Isolation anomalies, as Hermitage (github.com/ept/hermitage) tests them: the same scripts of two or three
//! interleaved transactions over `test (id, value)`, at READ COMMITTED and at REPEATABLE READ, each step's answer
//! checked, waits included (a step that must wait for another transaction's row doesn't answer until it ends).
//! The expected answers are Postgres's, and with `CHRONOS_TEST_PG` set (CI sets it) every script also runs on that
//! Postgres, so they are checked there too. SERIALIZABLE is refused (tests/row_locks.rs), so it has no scripts here.
//!
//! What Postgres prevents, and so must Chronos: G0 (dirty writes), G1a (aborted reads), G1b (intermediate reads),
//! G1c (circular information flow), OTV (observed transaction vanishes); and at REPEATABLE READ also PMP (predicate
//! many preceders), P4 (lost update) and G-single (read skew). What it allows at REPEATABLE READ, and these scripts
//! show as allowed: G2-item (write skew) and G2 (anti-dependency cycles).

use chronos::sql::{Session, Value};
use chronos::{Db, MAIN};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::Duration;

const SETUP: &str =
    "create table test (id int primary key, value int); insert into test (id, value) values (1, 10), (2, 20)";

/// How long a step that must wait is given to answer anyway, and how long one that waited has once let through.
const WAITS: Duration = Duration::from_millis(400);
const ANSWERS: Duration = Duration::from_secs(20);

type Answer = Result<String, String>; // rows as `1|10;2|20`, or the SQLSTATE

/// A connection on a thread of its own, so a statement that waits leaves the script free to go on.
struct Conn {
    send: Sender<String>,
    answers: Receiver<Answer>,
}

impl Conn {
    fn chronos(db: Arc<Db>) -> Conn {
        Conn::spawn(move |sql: &str| {
            thread_local! { static S: std::cell::RefCell<Session> = std::cell::RefCell::new(Session::new(MAIN)) }
            S.with(|s| match s.borrow_mut().run(&db, sql, &[]) {
                Ok(mut out) => Ok(out.pop().map_or_else(String::new, |o| {
                    o.rows
                        .iter()
                        .map(|r| {
                            let cell = |v: &Value| if *v == Value::Null { "null".into() } else { v.to_text() };
                            r.iter().map(cell).collect::<Vec<_>>().join("|")
                        })
                        .collect::<Vec<_>>()
                        .join(";")
                })),
                Err(e) => Err(e.code.to_string()),
            })
        })
    }

    fn postgres(url: &str, schema: &str) -> Conn {
        let mut c = postgres::Client::connect(url, postgres::NoTls).unwrap();
        c.batch_execute(&format!("set search_path = {schema}")).unwrap();
        Conn::spawn(move |sql: &str| match c.simple_query(sql) {
            Ok(msgs) => Ok(msgs
                .iter()
                .filter_map(|m| match m {
                    postgres::SimpleQueryMessage::Row(r) => {
                        Some((0..r.len()).map(|i| r.get(i).unwrap_or("null").to_string()).collect::<Vec<_>>().join("|"))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(";")),
            Err(e) => Err(e.code().map_or_else(|| format!("{e}"), |c| c.code().to_string())),
        })
    }

    fn spawn(mut run: impl FnMut(&str) -> Answer + Send + 'static) -> Conn {
        let (send, todo) = channel::<String>();
        let (answer, answers) = channel();
        std::thread::spawn(move || {
            for sql in todo {
                if answer.send(run(&sql)).is_err() {
                    break;
                }
            }
        });
        Conn { send, answers }
    }
}

/// One step: which connection, its SQL ("" to wait for the answer of its statement that was waiting), and what it
/// must give: "ok", "waits", "err:SQLSTATE", or "=" and its rows exactly (`=` alone: none).
type Step = (usize, &'static str, &'static str);

/// Plays `steps`, stopping at the first that doesn't give what it must: Err says which and what it gave.
fn play(on: &str, conns: &[Conn], steps: &[Step]) -> Result<(), String> {
    for (i, (c, sql, want)) in steps.iter().enumerate() {
        let conn = &conns[*c];
        if !sql.is_empty() {
            conn.send.send(sql.to_string()).unwrap();
        }
        let wait = if *want == "waits" { WAITS } else { ANSWERS };
        let got = match conn.answers.recv_timeout(wait) {
            Ok(a) => Some(a),
            Err(RecvTimeoutError::Timeout) => None,
            Err(e) => panic!("{on}: connection {c} is gone: {e}"),
        };
        let fine = match (*want, &got) {
            ("waits", None) => true,
            ("ok", Some(Ok(_))) => true,
            (w, Some(Err(code))) => w.strip_prefix("err:") == Some(code.as_str()),
            (w, Some(Ok(rows))) => w.strip_prefix('=') == Some(rows.as_str()),
            _ => false,
        };
        if !fine {
            return Err(format!(
                "{on}, step {i} (T{} {:?}): wanted {want}, got {}",
                c + 1,
                if sql.is_empty() { "(its waiting statement)" } else { *sql },
                match &got {
                    None => "no answer: it waited".to_string(),
                    Some(Ok(r)) => format!("rows {r:?}"),
                    Some(Err(e)) => format!("error {e}"),
                }
            ));
        }
    }
    Ok(())
}

static SCHEMA: AtomicUsize = AtomicUsize::new(0);

/// Ends what each connection left open (none should be waiting by now), so nothing holds a lock on the table.
fn close(conns: Vec<Conn>) {
    // (a script that stopped early may leave a statement waiting: its answer comes first, then the rollback's)
    for c in &conns {
        c.send.send("rollback".into()).unwrap();
        if c.answers.recv_timeout(ANSWERS).is_ok() {
            while c.answers.recv_timeout(Duration::from_millis(100)).is_ok() {}
        }
    }
}

/// Runs `steps` at `level` on Chronos, and on the Postgres at `CHRONOS_TEST_PG` when there is one: every connection
/// begins a transaction at that level first, and a fourth one reads the table at the end, which must be `end`. Both
/// run even when the first differs, and a failure says what each gave, so a difference from Postgres is on record.
fn hermitage(name: &str, level: &str, steps: &[Step], end: &'static str) {
    differs(name, level, (steps, end), (steps, end));
}

/// `hermitage` for a case where Chronos is known to answer differently from Postgres: each must give its own.
fn differs(name: &str, level: &str, chronos: (&[Step], &'static str), pg: (&[Step], &'static str)) {
    let mut start: Vec<Step> = vec![];
    for c in 0..3 {
        start.push((c, "begin", "ok"));
        start.push((c, if level == "rc" { RC } else { RR }, "ok"));
    }
    let script = |steps: &[Step]| -> Vec<Step> { start.iter().chain(steps).copied().collect() };
    let (script, pg_script) = (script(chronos.0), script(pg.0));
    let check: Step = (3, "select * from test order by id", chronos.1);
    let db = Arc::new(Db::new());
    Session::new(MAIN).run(&db, SETUP, &[]).unwrap();
    let conns: Vec<Conn> = (0..4).map(|_| Conn::chronos(db.clone())).collect();
    let on = format!("Chronos, {name} at {level}");
    let mut failed: Vec<String> =
        play(&on, &conns, &script).and_then(|_| play(&on, &conns, &[check])).err().into_iter().collect();
    close(conns);
    let Ok(url) = std::env::var("CHRONOS_TEST_PG") else {
        eprintln!("{name}: not checked on Postgres (no CHRONOS_TEST_PG)");
        assert!(failed.is_empty(), "{}", failed.join("\n"));
        return;
    };
    let schema = format!("hermitage_{}_{}", std::process::id(), SCHEMA.fetch_add(1, Ordering::Relaxed));
    let mut admin = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    admin.batch_execute(&format!("create schema {schema}; set search_path = {schema}; {SETUP}")).unwrap();
    let conns: Vec<Conn> = (0..4).map(|_| Conn::postgres(&url, &schema)).collect();
    let on = format!("Postgres, {name} at {level}");
    let check: Step = (3, "select * from test order by id", pg.1);
    failed.extend(play(&on, &conns, &pg_script).and_then(|_| play(&on, &conns, &[check])).err());
    close(conns);
    admin.batch_execute(&format!("drop schema {schema} cascade")).unwrap();
    assert!(failed.is_empty(), "{}", failed.join("\n"));
}

fn cat(a: &[Step], b: &[Step]) -> Vec<Step> {
    [a, b].concat()
}

const RC: &str = "set transaction isolation level read committed";
const RR: &str = "set transaction isolation level repeatable read";

// ---- prevented at both levels

#[test]
fn g0_dirty_writes() {
    let head: [Step; 4] = [
        (0, "update test set value = 11 where id = 1", "ok"),
        (1, "update test set value = 12 where id = 1", "waits"),
        (0, "update test set value = 21 where id = 2", "ok"),
        (0, "commit", "ok"),
    ];
    let rc: &[Step] = &[(1, "", "ok"), (1, "update test set value = 22 where id = 2", "ok"), (1, "commit", "ok")];
    hermitage("G0", "rc", &cat(&head, rc), "=1|12;2|22");
    hermitage("G0", "rr", &cat(&head, &[(1, "", "err:40001"), (1, "rollback", "ok")]), "=1|11;2|21");
}

#[test]
fn g1a_aborted_reads() {
    for level in ["rc", "rr"] {
        let steps = [
            (0, "update test set value = 101 where id = 1", "ok"),
            (1, "select * from test order by id", "=1|10;2|20"),
            (0, "rollback", "ok"),
            (1, "select * from test order by id", "=1|10;2|20"),
            (1, "commit", "ok"),
        ];
        hermitage("G1a", level, &steps, "=1|10;2|20");
    }
}

#[test]
fn g1b_intermediate_reads() {
    for (level, last) in [("rc", "=1|11;2|20"), ("rr", "=1|10;2|20")] {
        let steps = [
            (0, "update test set value = 101 where id = 1", "ok"),
            (1, "select * from test order by id", "=1|10;2|20"),
            (0, "update test set value = 11 where id = 1", "ok"),
            (0, "commit", "ok"),
            (1, "select * from test order by id", last),
            (1, "commit", "ok"),
        ];
        hermitage("G1b", level, &steps, "=1|11;2|20");
    }
}

#[test]
fn g1c_circular_information_flow() {
    for level in ["rc", "rr"] {
        let steps = [
            (0, "update test set value = 11 where id = 1", "ok"),
            (1, "update test set value = 22 where id = 2", "ok"),
            (0, "select * from test where id = 2", "=2|20"),
            (1, "select * from test where id = 1", "=1|10"),
            (0, "commit", "ok"),
            (1, "commit", "ok"),
        ];
        hermitage("G1c", level, &steps, "=1|11;2|22");
    }
}

#[test]
fn otv_observed_transaction_vanishes() {
    let head: [Step; 4] = [
        (0, "update test set value = 11 where id = 1", "ok"),
        (0, "update test set value = 19 where id = 2", "ok"),
        (1, "update test set value = 12 where id = 1", "waits"),
        (0, "commit", "ok"),
    ];
    let rc: &[Step] = &[
        (1, "", "ok"),
        (2, "select * from test where id = 1", "=1|11"),
        (1, "update test set value = 18 where id = 2", "ok"),
        (2, "select * from test where id = 2", "=2|19"),
        (1, "commit", "ok"),
        (2, "select * from test where id = 2", "=2|18"),
        (2, "select * from test where id = 1", "=1|12"),
        (2, "commit", "ok"),
    ];
    hermitage("OTV", "rc", &cat(&head, rc), "=1|12;2|18");
    let rr: &[Step] = &[
        (1, "", "err:40001"),
        (1, "rollback", "ok"),
        (2, "select * from test where id = 1", "=1|11"),
        (2, "select * from test where id = 2", "=2|19"),
        (2, "commit", "ok"),
    ];
    hermitage("OTV", "rr", &cat(&head, rr), "=1|11;2|19");
}

// ---- prevented at REPEATABLE READ, allowed at READ COMMITTED

#[test]
fn pmp_predicate_many_preceders() {
    for (level, seen) in [("rc", "=3|30"), ("rr", "=")] {
        let steps = [
            (0, "select * from test where value = 30", "="),
            (1, "insert into test (id, value) values (3, 30)", "ok"),
            (1, "commit", "ok"),
            (0, "select * from test where value % 3 = 0", seen),
            (0, "commit", "ok"),
        ];
        hermitage("PMP", level, &steps, "=1|10;2|20;3|30");
    }
}

#[test]
fn pmp_for_write_predicates() {
    let head: [Step; 3] = [
        (0, "update test set value = value + 10", "ok"),
        (1, "delete from test where value = 20 returning id", "waits"),
        (0, "commit", "ok"),
    ];
    // READ COMMITTED: Postgres's waiting delete looks again only at the row it waited for (now 30) and deletes
    // nothing, though row 1 is 20 now. Chronos runs the waiting statement again from a fresh snapshot, so it deletes
    // row 1: a known difference (see docs/postgres-compatibility.md)
    let pg: &[Step] = &[(1, "", "="), (1, "select * from test where value = 20", "=1|20"), (1, "commit", "ok")];
    let chronos: &[Step] = &[(1, "", "=1"), (1, "select * from test where value = 20", "="), (1, "commit", "ok")];
    differs("PMP (write predicates)", "rc", (&cat(&head, chronos), "=2|30"), (&cat(&head, pg), "=1|20;2|30"));
    let rr: &[Step] = &[(1, "", "err:40001"), (1, "rollback", "ok")];
    hermitage("PMP (write predicates)", "rr", &cat(&head, rr), "=1|20;2|30");
}

#[test]
fn p4_lost_update() {
    let head: [Step; 5] = [
        (0, "select * from test where id = 1", "=1|10"),
        (1, "select * from test where id = 1", "=1|10"),
        (0, "update test set value = 11 where id = 1", "ok"),
        (1, "update test set value = 11 where id = 1", "waits"),
        (0, "commit", "ok"),
    ];
    hermitage("P4", "rc", &cat(&head, &[(1, "", "ok"), (1, "commit", "ok")]), "=1|11;2|20");
    hermitage("P4", "rr", &cat(&head, &[(1, "", "err:40001"), (1, "rollback", "ok")]), "=1|11;2|20");
}

#[test]
fn g_single_read_skew() {
    for (level, seen) in [("rc", "=2|18"), ("rr", "=2|20")] {
        let steps = [
            (0, "select * from test where id = 1", "=1|10"),
            (1, "select * from test where id = 1", "=1|10"),
            (1, "select * from test where id = 2", "=2|20"),
            (1, "update test set value = 12 where id = 1", "ok"),
            (1, "update test set value = 18 where id = 2", "ok"),
            (1, "commit", "ok"),
            (0, "select * from test where id = 2", seen),
            (0, "commit", "ok"),
        ];
        hermitage("G-single", level, &steps, "=1|12;2|18");
    }
}

#[test]
fn g_single_with_a_write_predicate() {
    let head: [Step; 4] = [
        (0, "select * from test where id = 1", "=1|10"),
        (1, "update test set value = 12 where id = 1", "ok"),
        (1, "update test set value = 18 where id = 2", "ok"),
        (1, "commit", "ok"),
    ];
    let rc: &[Step] = &[(0, "delete from test where value = 20 returning id", "="), (0, "commit", "ok")];
    hermitage("G-single (write predicate)", "rc", &cat(&head, rc), "=1|12;2|18");
    let rr: &[Step] = &[(0, "delete from test where value = 20 returning id", "err:40001"), (0, "rollback", "ok")];
    hermitage("G-single (write predicate)", "rr", &cat(&head, rr), "=1|12;2|18");
}

// ---- allowed at both levels, as in Postgres (SERIALIZABLE would prevent them)

#[test]
fn g2_item_write_skew() {
    for level in ["rc", "rr"] {
        let steps = [
            (0, "select * from test where id in (1, 2) order by id", "=1|10;2|20"),
            (1, "select * from test where id in (1, 2) order by id", "=1|10;2|20"),
            (0, "update test set value = 11 where id = 1", "ok"),
            (1, "update test set value = 21 where id = 2", "ok"),
            (0, "commit", "ok"),
            (1, "commit", "ok"),
        ];
        hermitage("G2-item", level, &steps, "=1|11;2|21");
    }
}

#[test]
fn g2_anti_dependency_cycles() {
    for level in ["rc", "rr"] {
        let steps = [
            (0, "select * from test where value % 3 = 0", "="),
            (1, "select * from test where value % 3 = 0", "="),
            (0, "insert into test (id, value) values (3, 30)", "ok"),
            (1, "insert into test (id, value) values (4, 42)", "ok"),
            (0, "commit", "ok"),
            (1, "commit", "ok"),
        ];
        hermitage("G2", level, &steps, "=1|10;2|20;3|30;4|42");
    }
}
