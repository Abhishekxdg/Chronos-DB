//! pgbench, Postgres's own benchmark, against a Chronos server: it makes its tables with
//! `WITH (fillfactor)`, loads them with COPY, checks its catalogs (`pg_inherits`,
//! `pg_partitioned_table`) and runs the TPC-B transaction. Skipped where pgbench isn't installed.

mod common;

use std::process::{Command, Stdio};

fn pgbench(port: u16, args: &[&str]) -> Option<(bool, String)> {
    let out = Command::new("pgbench")
        .args(["-h", "127.0.0.1", "-p", &port.to_string(), "-U", "postgres", "-n"])
        .args(args)
        .arg("main")
        .stdin(Stdio::null())
        .output()
        .ok()?;
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    Some((out.status.success(), text))
}

#[test]
fn pgbench_initializes_and_runs() {
    let dir = std::env::temp_dir().join(format!("chronos-pgbench-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chronos"));
    cmd.args(["serve", dir.to_str().unwrap()]);
    let mut s = common::serve(&mut cmd, true);
    let port = s.pg;
    // (initialization without -n: it makes the tables)
    let init = Command::new("pgbench")
        .args(["-h", "127.0.0.1", "-p", &port.to_string(), "-U", "postgres", "-i", "-s", "1", "main"])
        .stdin(Stdio::null())
        .output();
    let Ok(init) = init else {
        eprintln!("skipped: no pgbench");
        let _ = s.child.kill();
        let _ = std::fs::remove_dir_all(&dir);
        return;
    };
    let text = format!("{}{}", String::from_utf8_lossy(&init.stdout), String::from_utf8_lossy(&init.stderr));
    assert!(init.status.success(), "pgbench -i failed:\n{text}");
    // one client, so no transaction conflicts with another; each of the three query modes
    for mode in ["simple", "extended", "prepared"] {
        let (ok, text) = pgbench(port, &["-c", "1", "-t", "20", "-M", mode]).unwrap();
        assert!(ok && text.contains("number of failed transactions: 0"), "-M {mode}:\n{text}");
    }
    // eight clients on one branch row: as Postgres's READ COMMITTED does, each update waits for the one before it,
    // and none fails
    for mode in ["simple", "prepared"] {
        let (ok, text) = pgbench(port, &["-c", "8", "-j", "4", "-t", "25", "-M", mode]).unwrap();
        assert!(ok && text.contains("number of failed transactions: 0"), "-c 8 -M {mode}:\n{text}");
        assert!(text.contains("transactions actually processed: 200"), "-c 8 -M {mode}:\n{text}");
    }
    let (ok, text) = pgbench(port, &["-c", "1", "-t", "50", "-S"]).unwrap();
    assert!(ok && text.contains("transactions actually processed: 50"), "{text}");
    let _ = s.child.kill();
    let _ = s.child.wait();
    let _ = std::fs::remove_dir_all(&dir);
}
