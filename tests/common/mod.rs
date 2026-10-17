//! Starting `chronos serve` in a test. The server listens on ports the kernel picks (`--listen
//! 127.0.0.1:0`) and prints them once it listens, so tests never guess a free port: a port picked
//! and released before the server binds it can be taken by another test binary in between, and
//! polling a port nobody listens on yet can connect to itself (a TCP self-connect), both of which
//! pass a readiness check against the wrong server, or none.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};

#[allow(dead_code)]
pub struct Serve {
    pub child: Child,
    pub http: u16,
    /// 0 with `--pg off`
    pub pg: u16,
    /// what the server printed before it was listening
    pub log: String,
}

/// Runs `cmd` (`chronos serve <folder>` and any flags but `--listen`/`--pg`) on kernel-picked
/// ports, with the Postgres listener too if `pg`, and returns once both accept connections.
pub fn serve(cmd: &mut Command, pg: bool) -> Serve {
    cmd.args(["--listen", "127.0.0.1:0", "--pg", if pg { "127.0.0.1:0" } else { "off" }]).stderr(Stdio::piped());
    // tests try many wrong passwords from 127.0.0.1: no waits between them, unless a test asks for them
    if !cmd.get_envs().any(|(k, _)| k == "CHRONOS_LOGIN_FREE_TRIES") {
        cmd.env("CHRONOS_LOGIN_FREE_TRIES", "1000000");
    }
    let mut child = cmd.spawn().unwrap();
    let mut err = BufReader::new(child.stderr.take().unwrap());
    let (mut http, mut pg_port, mut log) = (0, 0, String::new());
    while http == 0 || (pg && pg_port == 0) {
        let mut line = String::new();
        if err.read_line(&mut line).unwrap_or(0) == 0 {
            let _ = child.kill();
            let _ = child.wait();
            panic!("server didn't start:\n{log}");
        }
        // "chronos: serving on http://127.0.0.1:N (...)", "chronos: postgres protocol on 127.0.0.1:N (...)"
        let port = |after: &str| {
            let addr = line.split_once(after)?.1.split(' ').next()?;
            addr.rsplit_once(':')?.1.parse::<u16>().ok()
        };
        if let Some(p) = port("serving on ") {
            http = p;
        } else if let Some(p) = port("postgres protocol on ") {
            pg_port = p;
        }
        log.push_str(&line);
    }
    // keep reading, so a chatty server never blocks on a full pipe
    std::thread::spawn(move || std::io::copy(&mut err, &mut std::io::sink()));
    Serve { child, http, pg: pg_port, log }
}
