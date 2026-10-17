//! TLS on both ports of `chronos serve`, with a throwaway self-signed certificate: HTTPS with curl,
//! and the Postgres protocol with psql (verify-full). Plain connections are refused.

mod common;

use std::path::Path;
use std::process::{Child, Command, Stdio};

struct Server(Child, std::path::PathBuf);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
        let _ = std::fs::remove_dir_all(&self.1);
    }
}

fn cert(dir: &Path) {
    let ok = Command::new("openssl")
        .args(["req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:prime256v1", "-nodes", "-days", "1"])
        .args(["-subj", "/CN=localhost", "-addext", "subjectAltName=DNS:localhost,IP:127.0.0.1"])
        .arg("-keyout")
        .arg(dir.join("key.pem"))
        .arg("-out")
        .arg(dir.join("cert.pem"))
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success();
    assert!(ok, "openssl could not make a certificate");
}

#[test]
fn https_and_postgres_over_tls() {
    let dir = std::env::temp_dir().join(format!("chronos-tls-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    cert(&dir);
    let s = common::serve(
        Command::new(env!("CARGO_BIN_EXE_chronos"))
            .arg("serve")
            .arg(dir.join("db"))
            .arg("--tls-cert")
            .arg(dir.join("cert.pem"))
            .arg("--tls-key")
            .arg(dir.join("key.pem")),
        true,
    );
    let (http, pg) = (s.http, s.pg);
    let server = Server(s.child, dir.clone());
    let ca = dir.join("cert.pem");
    let curl = |url: &str, body: Option<&str>| {
        let mut c = Command::new("curl");
        c.args(["-s", "-w", "\n%{http_code}", "--cacert"]).arg(&ca).arg(url);
        if let Some(b) = body {
            c.args(["-X", "POST", "-H", "Content-Type: application/json", "-d", b]);
        }
        let out = String::from_utf8(c.output().unwrap().stdout).unwrap();
        let (body, code) = out.rsplit_once('\n').unwrap();
        (code.to_string(), body.to_string())
    };
    let base = format!("https://localhost:{http}");
    assert_eq!(curl(&format!("{base}/v1/health"), None).0, "200");
    // big enough that curl asks `Expect: 100-continue` first
    let big = format!(r#"{{"table": "t", "id": "1", "record": {{"pad": "{}"}}}}"#, "x".repeat(5000));
    assert_eq!(curl(&format!("{base}/v1/put"), Some(&big)).0, "200");
    let (code, got) = curl(&format!("{base}/v1/get"), Some(r#"{"table": "t", "id": "1"}"#));
    assert!(code == "200" && got.contains(&"x".repeat(5000)), "{code}");
    assert_eq!(curl(&format!("http://localhost:{http}/v1/health"), None).0, "000", "no plain HTTP");

    let psql = |mode: &str| {
        Command::new("psql")
            .arg(format!("postgres://localhost:{pg}/main?sslmode={mode}&sslrootcert={}", ca.display()))
            .args(["-tAc", "select count(*) from t"])
            .output()
    };
    match psql("verify-full") {
        Err(_) => eprintln!("psql not installed: skipping the Postgres half"),
        Ok(o) => {
            assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
            assert_eq!(String::from_utf8_lossy(&o.stdout).trim(), "1");
            let o = psql("disable").unwrap();
            assert!(!o.status.success() && String::from_utf8_lossy(&o.stderr).contains("requires TLS"));
        }
    }
    drop(server);
}
