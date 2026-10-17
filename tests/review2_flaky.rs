//! `chronos serve --listen 127.0.0.1:0 --pg 127.0.0.1:0` prints the ports the kernel picked, so
//! tests start servers without guessing free ports (which raced with other test binaries).

mod common;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::Command;

/// Several servers started at once each get their own ports, and answer on them at once.
#[test]
fn servers_announce_the_ports_they_bound() {
    let dir = |i: usize| std::env::temp_dir().join(format!("chronos-review2-flaky-{}-{i}", std::process::id()));
    let servers: Vec<_> = (0..6)
        .map(|i| {
            let dir = dir(i);
            let _ = std::fs::remove_dir_all(&dir);
            std::thread::spawn(move || {
                common::serve(Command::new(env!("CARGO_BIN_EXE_chronos")).args(["serve", dir.to_str().unwrap()]), true)
            })
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|t| t.join().unwrap())
        .collect();
    let mut ports: Vec<u16> = servers.iter().flat_map(|s| [s.http, s.pg]).collect();
    assert!(ports.iter().all(|&p| p != 0));
    ports.sort();
    ports.dedup();
    assert_eq!(ports.len(), 12, "every listener has its own port");
    for s in &servers {
        let mut c = TcpStream::connect(("127.0.0.1", s.http)).unwrap();
        c.write_all(b"GET /v1/health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").unwrap();
        let mut out = String::new();
        c.read_to_string(&mut out).unwrap();
        assert!(out.starts_with("HTTP/1.1 200"), "{out}");
        TcpStream::connect(("127.0.0.1", s.pg)).unwrap();
    }
    for (i, mut s) in servers.into_iter().enumerate() {
        let _ = s.child.kill();
        let _ = s.child.wait();
        let _ = std::fs::remove_dir_all(dir(i));
    }
}
