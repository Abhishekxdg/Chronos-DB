//! The `chronos` binary end to end: a piped script, one-shot commands, exit codes, reopen.

use std::io::Write;
use std::process::{Command, Stdio};

fn chronos(dir: &std::path::Path, args: &[&str], stdin: &str) -> (bool, String, String) {
    let mut c = Command::new(env!("CARGO_BIN_EXE_chronos"))
        .arg(dir)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    c.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
    let o = c.wait_with_output().unwrap();
    (o.status.success(), String::from_utf8(o.stdout).unwrap(), String::from_utf8(o.stderr).unwrap())
}

#[test]
fn shell_session() {
    let dir = std::env::temp_dir().join(format!("chronos-cli-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    let (ok, out, err) = chronos(
        &dir,
        &[],
        r#"
put users 1 {"name": "Ada", "role": "admin"}
put users 2 {"role": "user", "name": "Linus"}
put users 3 {"name": "Grace", "role": "user"}
find users where role = user
describe users
fork agent-7
put users 1 {"name": "Ada L.", "role": "admin"}
del users 2
diff
merge
fork agent-8
put users 3 {"name": "Grace H.", "role": "user"}
use main
put users 3 {"role": "user", "name": "Grace Hopper"}
merge agent-8
merge agent-8 theirs
"#,
    );
    assert!(!ok, "a failed command makes the script exit non-zero");
    assert!(out.contains("2 matches"), "{out}");
    assert!(out.contains("role  string  one of: \"admin\", \"user\""), "describe spots enums:\n{out}");
    assert!(out.contains("forked main -> agent-7; now on agent-7"), "{out}");
    assert!(out.contains(r#"~ users/1  name: "Ada" -> "Ada L.""#), "field-level diff:\n{out}");
    assert!(out.contains("- DELETE users/2"), "{out}");
    assert!(out.contains("merged 2 changes from agent-7 into main; now on main"), "{out}");
    assert!(err.contains("1 conflict: both agent-8 and main changed these rows"), "{err}");
    assert!(err.contains("resolve with: merge agent-8 ours"), "conflicts say how to fix:\n{err}");
    assert!(out.contains("merged 0 changes from agent-8 into main"), "theirs keeps main's value:\n{out}");

    // SQL in the shell: a statement can span lines until its ;
    let (ok, out, err) = chronos(
        &dir,
        &[],
        "create table pets (id int primary key,\n  name text not null);\ninsert into pets values (1, 'Rex'), (2, 'Tom');\nselect * from pets where id > 1;\nselect name from users order by name limit 1;\n",
    );
    assert!(ok, "{err}");
    assert!(out.contains("CREATE TABLE\nINSERT 0 2\n id | name\n----+------\n 2  | Tom\n(1 row)"), "{out}");
    assert!(out.contains(" name\n--------\n Ada L.\n(1 row)"), "SQL reads JSON tables too:\n{out}");

    // a transaction spans lines; shell commands inside it see its writes
    let (ok, out, err) = chronos(
        &dir,
        &[],
        "begin;\ninsert into pets values (3, 'Kit');\nget pets 3\nfork nope\nrollback;\nselect name from pets where id = 3;\n",
    );
    assert!(!ok && err.contains("a transaction is open"), "{err}");
    assert!(
        out.contains("BEGIN\nINSERT 0 1\n{\n  \"id\": 3,\n  \"name\": \"Kit\"\n}\nROLLBACK\n name\n------\n(0 rows)"),
        "{out}"
    );

    // one-shot, after reopening the folder
    let (ok, out, _) = chronos(&dir, &["get", "users", "3"], "");
    assert!(ok && out.contains("Grace Hopper"), "{out}");
    let (ok, out, _) = chronos(&dir, &["scan", "users"], "");
    assert!(ok && out.starts_with("2 rows"), "{out}");
    let (ok, _, err) = chronos(&dir, &["get", "users", "2"], "");
    assert!(!ok && err.contains("no row users/2"), "{err}");
    let (ok, _, err) = chronos(&dir, &["put", "users", "9", "{not json"], "");
    assert!(!ok && err.contains("invalid JSON"), "{err}");
    // JSON is stored canonically (sorted keys), so field order in the input doesn't matter
    let (_, out, _) = chronos(&dir, &["scan", "users"], "");
    assert!(out.contains(r#"users/3  {"name":"Grace Hopper","role":"user"}"#), "{out}");

    // search: text, vector, filters together
    let (ok, out, err) = chronos(
        &dir,
        &[],
        r#"
put docs 1 {"about": "billing error on invoice", "team": "core", "e": [1, 0]}
put docs 2 {"about": "login error", "team": "core", "e": [0, 1]}
put docs 3 {"about": "billing question", "team": "sales", "e": [1, 0]}
find docs match billing error
find docs near e [1, 0] limit 1
find docs where team = core match billing near e [1, 0]
find docs near e oops
"#,
    );
    assert!(!ok && err.contains("near needs a field and a JSON array"), "{err}");
    let results: Vec<&str> = out.split("put docs/3\n").nth(1).unwrap().lines().collect();
    assert!(results[0] == "3 matches" && results[1].starts_with("  docs/1  0.818"), "text: both words first:\n{out}");
    assert!(results[4] == "3 matches" && results[5].starts_with("  docs/1  1.000"), "vector, limit 1:\n{out}");
    assert_eq!(results.get(6), Some(&"2 matches"), "filter keeps only team core:\n{out}");
    assert!(results[7].starts_with("  docs/1  ") && results[8].starts_with("  docs/2  "), "{out}");

    std::fs::remove_dir_all(&dir).unwrap();
}

fn call(id: u32, name: &str, args: &str) -> String {
    format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"{name}","arguments":{args}}}}}"#)
}

/// Runs `chronos mcp <dir> <flags>` over a script of JSON-RPC lines; returns its replies.
fn mcp(dir: &std::path::Path, flags: &[&str], script: &[String]) -> Vec<serde_json::Value> {
    let mut c = Command::new(env!("CARGO_BIN_EXE_chronos"))
        .args(["mcp", dir.to_str().unwrap()])
        .args(flags)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    c.stdin.take().unwrap().write_all(script.join("\n").as_bytes()).unwrap();
    let out = String::from_utf8(c.wait_with_output().unwrap().stdout).unwrap();
    out.lines().map(|l| serde_json::from_str(l).unwrap()).collect()
}

#[test]
fn mcp_session() {
    let dir = std::env::temp_dir().join(format!("chronos-mcp-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let script = [
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#.to_string(),
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#.to_string(),
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#.to_string(),
        call(3, "put", r#"{"table":"users","id":"1","record":{"name":"Ada"}}"#),
        call(4, "fork", r#"{"name":"task-1"}"#),
        call(5, "put", r#"{"table":"users","id":"1","record":{"name":"Ada L."},"branch":"task-1"}"#),
        call(6, "merge", r#"{"branch":"task-1"}"#),
        call(7, "find", r#"{"table":"users","where":{"name":"Ada L."},"text":"ada"}"#),
        call(8, "get", r#"{"table":"users"}"#),
        r#"{"jsonrpc":"2.0","id":9,"method":"nope"}"#.to_string(),
        call(10, "sql", r#"{"sql":"select name from users where id = $1","params":["1"]}"#),
    ];
    // today's behavior, pinned: --allow-merge gives agents every tool, merge included
    let replies = mcp(&dir, &["--allow-merge"], &script);
    let out = format!("{replies:#?}");

    assert_eq!(replies.len(), 10, "one reply per request, none for the notification:\n{out}");
    assert_eq!(replies[0]["result"]["serverInfo"]["name"], "chronos");
    assert_eq!(replies[1]["result"]["tools"].as_array().unwrap().len(), 31);
    let text = |i: usize| replies[i]["result"]["content"][0]["text"].as_str().unwrap().to_string();
    let is_err = |i: usize| replies[i]["result"]["isError"].as_bool().unwrap();
    assert_eq!(text(5), "merged 1 change from task-1 into main");
    assert!(text(6).starts_with("1 match"), "{}", text(6));
    assert!(is_err(7) && text(7).contains("'id' is required"), "{}", text(7));
    assert_eq!(replies[8]["error"]["code"], -32601);
    assert_eq!(text(9), " name\n--------\n Ada L.\n(1 row)");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Safe mode, the default: the agent reads anything and changes only worlds it forked itself;
/// merging, restoring and every other world are left to a person.
#[test]
fn mcp_safe_mode() {
    let dir = std::env::temp_dir().join(format!("chronos-mcp-safe-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (ok, _, err) = chronos(&dir, &[], "put users 1 {\"name\": \"Ada\"}\nfork other\nuse main\n");
    assert!(ok, "{err}");

    let script = [
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#.to_string(),
        call(2, "put", r#"{"table":"users","id":"1","record":{"name":"X"}}"#),
        call(3, "put", r#"{"table":"users","id":"1","record":{"name":"X"},"branch":"main"}"#),
        call(4, "sql", r#"{"sql":"delete from users"}"#),
        call(5, "put", r#"{"table":"users","id":"1","record":{"name":"X"},"branch":"other"}"#),
        call(6, "discard", r#"{"branch":"other"}"#),
        call(7, "set_meta", r#"{"branch":"other","meta":{"a":1}}"#),
        call(8, "fork", r#"{"name":"task-1"}"#),
        call(9, "put", r#"{"table":"users","id":"1","record":{"name":"Ada L."},"branch":"task-1"}"#),
        call(
            10,
            "sql",
            r#"{"sql":"create table pets (id int primary key, name text); insert into pets values (1, 'Rex')","branch":"task-1"}"#,
        ),
        call(11, "sql", r#"{"sql":"insert into pets values (2, 'Tom'); merge world 'task-1'","branch":"task-1"}"#),
        call(12, "sql", r#"{"sql":"restore world main to '-1 minute'","branch":"task-1"}"#),
        call(13, "sql", r#"{"sql":"fork world main as sneaky","branch":"task-1"}"#),
        call(14, "merge", r#"{"branch":"task-1"}"#),
        call(15, "restore", r#"{"branch":"main","at":"-1 minute"}"#),
        call(16, "sql", r#"{"sql":"explain analyze delete from users"}"#),
        call(17, "sql", r#"{"sql":"select name from users"}"#),
        call(18, "get", r#"{"table":"users","id":"1","branch":"other"}"#),
        call(19, "diff", r#"{"branch":"task-1"}"#),
        call(20, "sql", r#"{"sql":"select count(*) from pets","branch":"task-1"}"#),
        call(21, "discard", r#"{"branch":"task-1"}"#),
        call(22, "put", r#"{"table":"users","id":"1","record":{"name":"X"},"branch":"task-1"}"#),
        // a simulation's kept worlds are this session's own; its script can't reach other worlds
        call(23, "simulate", r#"{"worlds":3,"prefix":"sim","script":"select 1","score":"select $1::int","keep":1}"#),
        call(24, "put", r#"{"table":"users","id":"1","record":{"name":"S"},"branch":"sim_2"}"#),
        call(25, "simulate", r#"{"worlds":2,"prefix":"bad","script":"merge world main","score":"select 1"}"#),
        call(26, "replay", r#"{"world":"sim_2"}"#),
    ];
    let r = mcp(&dir, &[], &script);
    assert_eq!(r.len(), 26, "{r:#?}");
    let names: Vec<&str> =
        r[0]["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names.len(), 28, "{names:?}"); // rebase too: on its own branches only; check_reads only reads
    assert!(!names.contains(&"merge") && !names.contains(&"restore") && !names.contains(&"undo_merge"), "{names:?}");
    let text = |id: usize| r[id - 1]["result"]["content"][0]["text"].as_str().unwrap().to_string();
    let refused = |id: usize, why: &str| {
        assert_eq!(r[id - 1]["result"]["isError"], true, "call {id} should be refused: {}", text(id));
        assert!(text(id).contains(why), "call {id}: {}", text(id));
    };
    let allowed = |id: usize| assert_eq!(r[id - 1]["result"]["isError"], false, "call {id}: {}", text(id));

    refused(2, "changes need branch=");
    refused(3, "'main' isn't one");
    refused(4, "changes need branch=");
    refused(5, "'other' isn't one");
    refused(6, "'other' isn't one");
    refused(7, "'other' isn't one");
    allowed(8);
    allowed(9);
    allowed(10);
    refused(11, "can't fork, merge, restore");
    refused(12, "can't fork, merge, restore");
    refused(13, "can't fork, merge, restore");
    refused(14, "agents can't merge");
    refused(15, "agents can't restore");
    refused(16, "changes need branch=");
    allowed(17);
    assert!(text(17).contains("Ada") && !text(17).contains("Ada L."), "main is unchanged: {}", text(17));
    allowed(18);
    allowed(19);
    assert!(text(19).contains("Ada L."), "{}", text(19));
    assert!(text(20).contains(" 1\n"), "the insert beside the refused merge never ran: {}", text(20));
    allowed(21);
    refused(22, "'task-1' isn't one");
    allowed(23);
    assert!(text(23).contains("kept: sim_2") && text(23).contains("sim_2 (index 2): score 2, kept"), "{}", text(23));
    allowed(24);
    refused(25, "can't fork, merge");
    allowed(26);
    assert!(text(26).starts_with("sim_2: not identical (1 rows differ"), "changed since: {}", text(26));

    // nothing reached main or the other world
    let (_, out, _) = chronos(&dir, &["get", "users", "1"], "");
    assert!(out.contains("\"Ada\"") && !out.contains("Ada L."), "{out}");
    let (_, out, _) = chronos(&dir, &["branches"], "");
    assert!(out.contains("other") && !out.contains("sneaky"), "{out}");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// `chronos import`: a copy of one Postgres table, all or nothing, with errors that name the
/// line and column. (The files are what `\copy ... csv header` and the schema query write.)
#[test]
fn import_table() {
    let dir = std::env::temp_dir().join(format!("chronos-import-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let file = |name: &str, text: &str| {
        let p = dir.join(name);
        std::fs::write(&p, text).unwrap();
        p.to_str().unwrap().to_string()
    };
    let db = dir.join("db");
    let import = |table: &str, data: &str, schema: &str| {
        let o = Command::new(env!("CARGO_BIN_EXE_chronos"))
            .args(["import", db.to_str().unwrap(), table, data, "--schema", schema])
            .output()
            .unwrap();
        (o.status.success(), String::from_utf8(o.stdout).unwrap(), String::from_utf8(o.stderr).unwrap())
    };
    let schema =
        file("schema.csv", "id,integer,NO,t\nname,text,NO,f\nnote,text,YES,f\nn,bigint,YES,f\ntags,ARRAY,YES,f\n");

    let data = file("ok.csv", "id,name,note,n,tags\n1,Ada,\"a, \"\"b\"\"\",7,\"{x,y}\"\n2,Bob,,,\n3,Cy,\"\",-1,{}\n");
    let (ok, out, err) = import("people", &data, &schema);
    assert!(ok, "{err}");
    assert_eq!(out, "warning: column tags has type ARRAY, imported as text\nimported 3 rows into people\n");
    let (_, out, _) =
        chronos(&db, &[], "select count(*) from people where note is null;\nselect note from people where id = 1;\n");
    assert!(out.contains(" 1\n") && out.contains("a, \"b\""), "empty is NULL, quotes kept:\n{out}");

    let (ok, _, err) = import("people", &data, &schema);
    assert!(!ok && err.contains("table \"people\" already exists"), "{err}");

    // a bad value fails the whole import, naming line and column; no table is left behind
    let bad = file("bad.csv", "id,name,note,n,tags\n1,Ada,,1,\n2,Bob,,lots,\n");
    let (ok, _, err) = import("bad", &bad, &schema);
    assert!(!ok && err.contains("line 3, column n:") && err.contains("lots"), "{err}");
    let (ok, _, err) = import("bad", &file("null.csv", "id,name,note,n,tags\n1,,,,\n"), &schema);
    assert!(!ok && err.contains("line 2, column name: empty, but the column is NOT NULL"), "{err}");
    let (ok, _, err) = import("bad", &file("short.csv", "id,name,note,n,tags\n1,Ada\n"), &schema);
    assert!(!ok && err.contains("line 2: 2 fields, but the header has 5"), "{err}");
    let (ok, _, err) = import("bad", &file("head.csv", "id,name,oops\n"), &schema);
    assert!(!ok && err.contains("column \"oops\" isn't in the schema"), "{err}");
    let (_, out, _) = chronos(&db, &["tables"], "");
    assert!(!out.contains("bad"), "failed imports leave no table:\n{out}");

    let (ok, out, err) = import("empty", &file("hdr.csv", "id,name,note,n,tags\n"), &schema);
    assert!(ok && out.contains("imported 0 rows into empty"), "{err}");
    let (ok, _, err) = import("t", &data, &file("nokey.csv", "id,integer,NO,f\n"));
    assert!(!ok && err.contains("no primary key"), "{err}");
    let two = file("twokey.csv", "a,integer,NO,t\nb,text,NO,t\nv,text,YES,f\n");
    let (ok, out, err) = import("two", &file("two.csv", "a,b,v\n1,x,one\n1,y,two\n"), &two);
    assert!(ok && out.contains("imported 2 rows into two"), "a key of several columns: {err}");
    let (ok, _, err) = import("two2", &file("dup.csv", "a,b,v\n1,x,one\n1,x,two\n"), &two);
    assert!(!ok && err.contains("already exists"), "{err}");
    let o = Command::new(env!("CARGO_BIN_EXE_chronos")).args(["import", "x"]).output().unwrap();
    assert!(
        String::from_utf8(o.stderr).unwrap().contains("information_schema.columns"),
        "usage shows the psql commands"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

/// `undo` in the shell: puts back what the latest merge changed, keeps later writes to other
/// rows, and refuses (changing nothing) once a merged row has changed again.
#[test]
fn shell_undo() {
    let dir = std::env::temp_dir().join(format!("chronos-undo-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (ok, _, err) = chronos(&dir, &[], "undo\n");
    assert!(!ok && err.contains("nothing was merged into main recently"), "{err}");

    let (ok, out, err) = chronos(
        &dir,
        &[],
        r#"
put users 1 {"name": "Ada"}
put users 2 {"name": "Bob"}
fork task-1
put users 1 {"name": "Ada L."}
put users 3 {"name": "Cy"}
use main
merge task-1
put users 2 {"name": "Bob B."}
undo
"#,
    );
    assert!(ok, "{err}");
    assert!(out.contains("undid the merge of task-1 into main: put back 2 rows"), "{out}");
    let (_, out, _) = chronos(&dir, &["scan", "users"], "");
    assert!(
        out.contains(r#"users/1  {"name":"Ada"}"#)
            && out.contains(r#"users/2  {"name":"Bob B."}"#)
            && !out.contains("users/3"),
        "merged rows put back, the later write to users/2 kept:\n{out}"
    );

    // undoing again would overwrite the undo itself: refused, rows named, nothing changes
    let (ok, _, err) = chronos(&dir, &["undo"], "");
    assert!(!ok && err.contains("users/1") && err.contains("nothing was undone"), "{err}");
    assert!(err.contains("if you already undid it, name an earlier one"), "{err}");
    let (_, again, _) = chronos(&dir, &["scan", "users"], "");
    assert_eq!(again, out);

    // the SQL form still reaches SQL
    let (ok, _, err) = chronos(&dir, &[], "undo merge of world 'task-1';\n");
    assert!(!ok && err.contains("changed again since"), "{err}");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// `chronos mcp --agent NAME`: the session acts as that agent. Its forks are its own (and stay its
/// own in later sessions), it changes only what it may, and the audit trail says it was it.
#[test]
fn mcp_as_an_agent() {
    let dir = std::env::temp_dir().join(format!("chronos-mcp-agent-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    {
        let db = chronos::Db::open(&dir).unwrap();
        db.put(chronos::MAIN, "users/1", r#"{"name": "Ada"}"#).unwrap();
        db.create_agent("helper", &chronos::AgentSpec::default()).unwrap();
    }
    let script = [
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#.to_string(),
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#.to_string(),
        call(3, "fork", r#"{"name":"helper-1"}"#),
        call(4, "put", r#"{"table":"users","id":"1","record":{"name":"Ada L."},"branch":"helper-1"}"#),
        call(5, "put", r#"{"table":"users","id":"2","record":{"name":"Bo"}}"#),
        call(6, "sql", r#"{"sql":"merge world 'helper-1'"}"#),
        call(7, "get", r#"{"table":"users","id":"1"}"#),
    ];
    let replies = mcp(&dir, &["--agent", "helper"], &script);
    let text = |i: usize| replies[i]["result"]["content"][0]["text"].as_str().unwrap_or_default().to_string();
    let is_err = |i: usize| replies[i]["result"]["isError"].as_bool().unwrap_or(false);
    let tools: Vec<&str> =
        replies[1]["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(!tools.contains(&"merge") && !tools.contains(&"restore"), "{tools:?}");
    assert!(
        !tools.contains(&"undo_merge") && !tools.contains(&"rollback") && tools.contains(&"checkpoint"),
        "{tools:?}"
    );
    assert!(!is_err(2) && !is_err(3), "{} / {}", text(2), text(3));
    assert!(is_err(4) && text(4).contains("may not change main"), "{}", text(4));
    assert!(is_err(5) && text(5).contains("may not"), "{}", text(5));
    assert!(!is_err(6), "reads anywhere");

    // a later session: the world is still its own
    let later = mcp(
        &dir,
        &["--agent", "helper"],
        &[call(1, "put", r#"{"table":"users","id":"3","record":{"name":"Cy"},"branch":"helper-1"}"#)],
    );
    assert_eq!(later[0]["result"]["isError"].as_bool(), Some(false), "{}", later[0]);
    let db = chronos::Db::open(&dir).unwrap();
    assert_eq!(db.world("helper-1").unwrap().owner.as_deref(), Some("helper"));
    let who: Vec<String> = db.audit(None, 10).unwrap().iter().map(|e| format!("{} {}", e.agent, e.action)).collect();
    assert_eq!(who, ["helper write", "helper write", "helper fork from main"]);
    // an unknown agent can't start a session
    drop(db);
    let out = Command::new(env!("CARGO_BIN_EXE_chronos"))
        .args(["mcp", dir.to_str().unwrap(), "--agent", "ghost"])
        .output()
        .unwrap();
    assert!(!out.status.success() && String::from_utf8_lossy(&out.stderr).contains("agent ghost"));
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The server's `instructions` follow what the session may do: one that may merge its own branches
/// is told to merge them (and what a held merge means), one that may not is told a person merges.
#[test]
fn mcp_instructions_follow_the_merge_right() {
    let dir = std::env::temp_dir().join(format!("chronos-mcp-instr-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    {
        let db = chronos::Db::open(&dir).unwrap();
        let can = |c: &[chronos::Can]| chronos::AgentSpec { can: Some(c.to_vec()), ..Default::default() };
        db.create_agent("reader", &chronos::AgentSpec::default()).unwrap();
        db.create_agent("own", &can(&[chronos::Can::Read, chronos::Can::Fork, chronos::Can::MergeOwn])).unwrap();
        db.create_agent("any", &can(&[chronos::Can::Read, chronos::Can::Fork, chronos::Can::Merge])).unwrap();
    }
    let init =
        [r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#.to_string()];
    let instructions = |flags: &[&str]| -> String {
        let r = mcp(&dir, flags, &init);
        r[0]["result"]["instructions"].as_str().unwrap().to_string()
    };
    // may not merge: a person does, with the terminal command, and the agent says which branch
    for flags in [&[][..], &["--agent", "reader"]] {
        let t = instructions(flags);
        assert!(
            t.contains("you can't merge") && t.contains("chronos <database folder> merge <branch>"),
            "{flags:?}: {t}"
        );
        assert!(!t.contains("call merge yourself"), "{flags:?}: {t}");
        assert!(t.contains("needs a person's review") && t.contains("stop, and tell the person"), "{flags:?}: {t}");
    }
    // may merge its own branches (--allow-merge, merge_own, merge): told to merge, and what a held merge means
    for flags in [&["--allow-merge"][..], &["--agent", "own"], &["--agent", "any"]] {
        let t = instructions(flags);
        assert!(t.contains("call merge yourself"), "{flags:?}: {t}");
        // conflicts: settled by columns first, a whole side last
        let (cols, whole) = (t.find("by_columns=true combines"), t.find("resolve without by_columns takes whole rows"));
        assert!(cols.is_some() && whole.is_some() && cols < whole, "{flags:?}: {t}");
        assert!(!t.contains("you can't merge") && !t.contains("chronos <database folder> merge"), "{flags:?}: {t}");
        assert!(t.contains("needs a person's review") && t.contains("stop, and tell the person"), "{flags:?}: {t}");
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

/// What people read: a text search counts only rows with the words, and a diff lists
/// numeric ids by value (users/2, users/10, users/100), not as text.
#[test]
fn search_counts_and_diff_order() {
    let dir = std::env::temp_dir().join(format!("chronos-display-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (ok, out, err) = chronos(
        &dir,
        &[],
        r#"
put docs 1 {"about": "billing error on invoice", "team": "core"}
put docs 2 {"about": "login trouble", "team": "core"}
put docs 3 {"about": "feature request", "team": "core"}
put docs 4 {"about": "billing question", "team": "sales"}
find docs where team = core match billing
find docs match nothing-like-this
find docs where team = core
fork f
put users 100 {"n": 1}
put users 2 {"n": 1}
put users 10 {"n": 1}
put users a {"n": 1}
diff
"#,
    );
    assert!(ok, "{err}");
    let lines: Vec<&str> = out.lines().collect();
    let at = |s: &str| lines.iter().position(|l| *l == s).unwrap_or_else(|| panic!("no line {s:?}:\n{out}"));
    assert!(lines[at("put docs/4") + 1] == "1 match", "only the core doc with billing:\n{out}");
    assert!(out.contains("0 matches"), "words no row has match nothing:\n{out}");
    assert!(out.contains("3 matches"), "filters alone count every row that passes:\n{out}");
    let diff: Vec<&str> = lines[at("4 changes on f (users: 0 deleted, 0 updated, 4 inserted)") + 1..]
        .iter()
        .take_while(|l| !l.starts_with("(at version"))
        .map(|l| l.split("  ").next().unwrap())
        .collect();
    assert_eq!(diff, ["+ users/a", "+ users/2", "+ users/10", "+ users/100"], "{out}");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn backup_restore_verify() {
    let base = std::env::temp_dir().join(format!("chronos-cli-backup-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let (dir, b1, b2, restored) = (base.join("db"), base.join("b1"), base.join("b2"), base.join("restored"));
    let (ok, out, err) = chronos(
        &dir,
        &[],
        &format!(
            "put users 1 {{\"name\": \"Ada\"}}\nbackup {}\nbackup database to '{}';\nverify\n",
            b1.display(),
            b2.display()
        ),
    );
    assert!(ok, "{out}{err}");
    assert!(
        out.contains("backed up to") && out.contains("files | bytes") && out.contains("log records read and checked"),
        "{out}"
    );
    let (ok, _, err) = chronos(&dir, &["backup", b1.to_str().unwrap()], "");
    assert!(!ok && err.contains("not empty"), "{err}");

    let o = Command::new(env!("CARGO_BIN_EXE_chronos"))
        .args(["restore", b2.to_str().unwrap(), restored.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let (ok, out, _) = chronos(&restored, &["get", "users", "1"], "");
    assert!(ok && out.contains("Ada"), "{out}");
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn shell_and_mcp_refuse_the_databases_own_tables() {
    let dir = std::env::temp_dir().join(format!("chronos-cli-reserved-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (ok, _, err) = chronos(&dir, &["get", "_sonos_agents", "bot"], "");
    assert!(!ok && err.contains("database's own"), "{err}");
    let out = mcp(&dir, &[], &[call(2, "get", r#"{"table": "_sonos_agents", "id": "bot"}"#)]);
    assert!(out.iter().any(|r| r.to_string().contains("database's own")), "{out:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// MCP's sql tool checks an agent's statements exactly as the Postgres port does: an agent that
/// may restore, merge and write main may undo a merge into main, without being admin.
#[test]
fn mcp_sql_checks_agents_like_postgres() {
    let dir = std::env::temp_dir().join(format!("chronos-mcp-undo-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    {
        let db = chronos::Db::open(&dir).unwrap();
        db.put(chronos::MAIN, "users/1", r#"{"name": "Ada"}"#).unwrap();
        db.fork(chronos::MAIN, "x").unwrap();
        db.put("x", "users/1", r#"{"name": "Ada L."}"#).unwrap();
        db.merge("x", chronos::Resolve::Fail).unwrap();
        use chronos::Can::*;
        db.create_agent(
            "fixer",
            &chronos::AgentSpec { can: Some(vec![Read, Fork, Merge, Restore, WriteMain]), ..Default::default() },
        )
        .unwrap();
    }
    let replies = mcp(&dir, &["--agent", "fixer"], &[call(1, "sql", r#"{"sql":"undo merge of world x"}"#)]);
    assert_eq!(replies[0]["result"]["isError"].as_bool(), Some(false), "{}", replies[0]);
    let db = chronos::Db::open(&dir).unwrap();
    let v = db.get(chronos::MAIN, "users/1").unwrap().unwrap();
    assert!(v.contains("\"Ada\""), "{v} after {}", replies[0]);
    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// While an agent's `chronos mcp` has the folder open, a person's shell commands and a second agent
/// run through that process (a local socket) instead of failing with "already open"; each
/// session keeps its own safe-mode state, and closing the first process keeps everyone's work.
#[test]
fn shared_folder() {
    let dir = std::env::temp_dir().join(format!("chronos-shared-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (ok, _, err) = chronos(&dir, &["put", "users", "1", r#"{"name": "Ada"}"#], "");
    assert!(ok, "{err}");

    // agent A: a long-running `chronos mcp`, the folder's owner
    let mut a = Command::new(env!("CARGO_BIN_EXE_chronos"))
        .args(["mcp", dir.to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut a_in = a.stdin.take().unwrap();
    let mut a_out = std::io::BufReader::new(a.stdout.take().unwrap());
    let mut ask = |line: String| {
        writeln!(a_in, "{line}").unwrap();
        let mut reply = String::new();
        std::io::BufRead::read_line(&mut a_out, &mut reply).unwrap();
        serde_json::from_str::<serde_json::Value>(&reply).unwrap()
    };
    ask(call(1, "fork", r#"{"name":"agent-a"}"#));
    let r = ask(call(2, "put", r#"{"table":"users","id":"1","record":{"name":"Ada L."},"branch":"agent-a"}"#));
    assert_eq!(r["result"]["isError"], false, "{r}");

    // the person, while A is connected
    let (ok, out, err) = chronos(&dir, &["diff", "agent-a"], "");
    assert!(ok && out.contains(r#"name: "Ada" -> "Ada L.""#), "{out}{err}");

    // a second agent joins; its own safe mode still refuses merging
    let b = mcp(&dir, &[], &[call(1, "fork", r#"{"name":"agent-b"}"#), call(2, "merge", r#"{"branch":"agent-b"}"#)]);
    assert_eq!(b[0]["result"]["isError"], false, "{b:?}");
    assert!(b[1]["result"]["content"][0]["text"].as_str().unwrap().contains("agents can't merge"), "{b:?}");

    let (ok, out, err) = chronos(&dir, &[], "merge agent-a\nget users 1\nundo\n");
    assert!(
        ok && out.contains("merged 1 change") && out.contains("Ada L.") && out.contains("put back 1 row"),
        "{out}{err}"
    );

    // A still works, then closes; everything is on disk for the next process
    let r = ask(call(3, "diff", r#"{"branch":"agent-b"}"#));
    assert_eq!(r["result"]["isError"], false, "{r}");
    drop(a_in);
    assert!(a.wait().unwrap().success());
    let (ok, out, _) = chronos(&dir, &["branches"], "");
    assert!(ok && out.contains("agent-b") && !out.contains("agent-a"), "{out}");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A world name with a dash works unquoted in SQL too (agents fork such names, and write them as they are).
#[test]
fn dashed_world_names_work_unquoted() {
    let dir = std::env::temp_dir().join(format!("chronos-dash-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (ok, out, err) = chronos(&dir, &[], "fork add-todos-2\nuse main\nmerge world add-todos-2;\n");
    assert!(ok, "unquoted: {out}{err}");
    let (ok, out, err) = chronos(&dir, &[], "fork task-1\nuse main\nmerge world 'task-1';\n");
    assert!(ok, "quoted: {out}{err}");
    // a dash with nothing name-like after it still says how to write it
    let (ok, _, err) = chronos(&dir, &[], "merge world task-;\n");
    assert!(!ok && err.contains("world names with a dash need quotes, like 'task-...'"), "{err}");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The shell's diff (and MCP's) puts a table made in a world in words, not as the catalog's JSON row.
#[test]
fn diff_shows_a_new_table_in_words() {
    let dir = std::env::temp_dir().join(format!("chronos-diff-schema-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (ok, out, err) = chronos(
        &dir,
        &[],
        "fork w\ncreate table todos (id int primary key, title text not null);\ninsert into todos values (1, 'a');\ndiff\n",
    );
    assert!(ok, "{out}{err}");
    assert!(out.contains("+ create table todos (id integer not null, title text not null"), "{out}");
    // the primary key is part of the definition: `id int primary key` doesn't read as `id int not null`
    assert!(out.contains(r#"primary key "id""#), "{out}");
    assert!(out.contains("+ todos/1"), "{out}");
    assert!(!out.contains("_sonos_tables"), "{out}");
    // an altered table (one main has): the new column whole, its default and not null included
    let (ok, out, err) = chronos(
        &dir,
        &[],
        "use main\ncreate table tags (id int primary key);\nfork w2\nalter table tags add column rank int not null default 1;\ndiff\n",
    );
    assert!(ok, "{out}{err}");
    assert!(out.contains("~ alter table tags: +rank integer not null") && out.contains("default"), "{out}");
    // dropping a table's only check shows what was dropped
    let (ok, out, err) = chronos(
        &dir,
        &[],
        "use main\ncreate table prices (id int primary key, amount int, constraint positive check (amount > 0));\nfork w3\nalter table prices drop constraint positive;\ndiff\n",
    );
    assert!(ok, "{out}{err}");
    assert!(out.contains("~ alter table prices: -check") && out.contains("amount > 0"), "{out}");
    // a type DIFF has no words for still shows, with its definition
    let (ok, out, err) = chronos(&dir, &[], "use w\ncreate type mood as enum ('happy', 'sad');\ndiff\n");
    assert!(ok, "{out}{err}");
    assert!(out.contains("+ type mood") && out.contains("happy"), "{out}");
    // a view has no columns to list: its definition shows, so two different queries never read the same
    let (ok, out, err) =
        chronos(&dir, &[], "use w\ncreate view open_todos as select id from todos where id > 7;\ndiff\n");
    assert!(ok, "{out}{err}");
    assert!(out.contains("+ create view open_todos") && out.contains("id > 7"), "{out}");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn migrate_applies_new_files_once_each_whole_or_not_at_all() {
    let base = std::env::temp_dir().join(format!("chronos-cli-migrate-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let (dir, m) = (base.join("db"), base.join("migrations"));
    std::fs::create_dir_all(&m).unwrap();
    let w = |f: &str, sql: &str| std::fs::write(m.join(f), sql).unwrap();
    let migrate = || chronos(&dir, &["migrate", m.to_str().unwrap()], "");
    w("0001_users.sql", "create table users (id int primary key, name text);\ninsert into users values (1, 'Ada');");
    w("0002_email.sql", "alter table users add column email text;");
    w("notes.txt", "not a migration");
    let (ok, out, err) = migrate();
    assert!(ok && out.contains("applied 0001_users.sql, 0002_email.sql"), "{out}{err}");
    let (ok, out, _) = migrate();
    assert!(ok && out.contains("up to date: 2 applied before"), "{out}");

    // a failing file rolls back whole: its first statement doesn't stay, nor its record
    w("0003_bad.sql", "insert into users values (2, 'Bo', 'bo@x');\ninsert into nope values (1);");
    let (ok, _, err) = migrate();
    assert!(!ok && err.contains("0003_bad.sql") && err.contains("nope"), "{err}");
    let (_, out, _) = chronos(&dir, &[], "select count(*) from users;\n");
    assert!(out.contains(" 1"), "{out}");
    w("0003_bad.sql", "insert into users values (2, 'Bo', 'bo@x');");
    let (ok, out, err) = migrate();
    assert!(ok && out.contains("applied 0003_bad.sql"), "{out}{err}");

    w("0004_tx.sql", "begin; insert into users values (3, 'Cy', null); commit;");
    let (ok, _, err) = migrate();
    assert!(!ok && err.contains("leave out BEGIN/COMMIT"), "{err}");
    std::fs::remove_file(m.join("0004_tx.sql")).unwrap();
    w("0001_users.sql", "create table users (id int primary key);");
    let (ok, _, err) = migrate();
    assert!(!ok && err.contains("0001_users.sql changed after it was applied"), "{err}");
    let (_, out, _) = chronos(&dir, &[], "select id, name, email from users order by id;\n");
    assert!(out.contains("Bo") && out.contains("bo@x") && !out.contains("Cy"), "{out}");
    let _ = std::fs::remove_dir_all(&base);
}

/// Merge preview, per-row decisions, undo, SQL diffs and checkpoints over MCP; safe mode lets an
/// agent preview and checkpoint its own worlds but never approve anything.
#[test]
fn mcp_merge_tools() {
    let dir = std::env::temp_dir().join(format!("chronos-mcp-merge-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (ok, _, err) = chronos(
        &dir,
        &[],
        "create table t (id int primary key, a text, b text);\ninsert into t values (1, 'a0', 'b0'), (2, 'x', 'y');\n",
    );
    assert!(ok, "{err}");
    let script = [
        call(1, "fork", r#"{"name":"w"}"#),
        call(
            2,
            "sql",
            r#"{"sql":"update t set a = 'a1' where id = 1; update t set a = 'mine' where id = 2","branch":"w"}"#,
        ),
        call(3, "sql", r#"{"sql":"update t set b = 'b2' where id = 1; update t set a = 'theirs' where id = 2"}"#),
        call(4, "merge_preview", r#"{"branch":"w"}"#),
        call(5, "merge_preview", r#"{"branch":"w","by_columns":true}"#),
        call(6, "diff", r#"{"branch":"w","as_sql":true}"#),
        call(7, "merge", r#"{"branch":"w","by_columns":true,"rows":{"t/2":"ours"}}"#),
        call(8, "sql", r#"{"sql":"select id, a, b from t order by id"}"#),
        call(9, "undo_merge", r#"{"branch":"w"}"#),
        call(10, "sql", r#"{"sql":"select id, a, b from t order by id"}"#),
        call(11, "fork", r#"{"name":"v"}"#),
        call(12, "checkpoint", r#"{"branch":"v","name":"before"}"#),
        call(13, "sql", r#"{"sql":"delete from t","branch":"v"}"#),
        call(14, "rollback", r#"{"branch":"v","checkpoint":"before"}"#),
        call(15, "sql", r#"{"sql":"select count(*) from t","branch":"v"}"#),
        call(16, "merge", r#"{"branch":"v","rows":{"t/1":"sideways"}}"#),
        call(17, "fork", r#"{"name":"u"}"#),
        call(18, "sql", r#"{"sql":"update t set a = 'u' where id = 1","branch":"u"}"#),
        call(19, "sql", r#"{"sql":"update t set a = 'main' where id = 1"}"#),
        call(20, "merge", r#"{"branch":"u"}"#),
        call(21, "merge", r#"{"branch":"u","by_columns":true}"#),
    ];
    let r = mcp(&dir, &["--allow-merge"], &script);
    let text = |id: usize| r[id - 1]["result"]["content"][0]["text"].as_str().unwrap().to_string();
    let ok = |id: usize| assert_eq!(r[id - 1]["result"]["isError"], false, "call {id}: {}", text(id));
    for id in 1..=15 {
        ok(id);
    }
    assert!(text(4).contains("conflict"), "row 1 and 2 conflict as whole rows:\n{}", text(4));
    assert!(text(5).contains("t") && text(5).matches("conflict").count() >= 1, "{}", text(5));
    assert!(text(6).contains("update") && text(6).contains("'mine'"), "{}", text(6));
    assert!(
        text(8).contains(" 1  | a1   | b2") && text(8).contains("mine"),
        "columns merged, row 2 ours:\n{}",
        text(8)
    );
    assert!(text(10).contains(" a0 ") && text(10).contains("theirs"), "undone to the parent's rows:\n{}", text(10));
    assert!(text(15).contains(" 2\n"), "rolled back: {}", text(15));
    assert_eq!(r[15]["result"]["isError"], true);
    assert!(text(16).contains("\"ours\", \"theirs\", \"delete\""), "{}", text(16));
    assert!(
        text(20).contains("1 conflict") && text(20).contains("by_columns=true") && text(20).contains("merge_preview"),
        "{}",
        text(20)
    );
    assert!(text(20).contains("safest first") && text(20).contains("the same call again fails"), "{}", text(20));
    // by columns, the same column still conflicts: told how to pick a side for it
    assert_eq!(r[20]["result"]["isError"], true);
    assert!(
        text(21).contains("both changed a") && text(21).contains("resolve=\"ours\"|\"theirs\" too"),
        "{}",
        text(21)
    );

    // safe mode: previews and its own checkpoints, but no approvals and nobody else's worlds
    let script = [
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#.to_string(),
        call(2, "fork", r#"{"name":"s"}"#),
        call(3, "checkpoint", r#"{"branch":"s","name":"c"}"#),
        call(4, "rollback", r#"{"branch":"s","checkpoint":"c"}"#),
        call(5, "checkpoint", r#"{"branch":"main","name":"c"}"#),
        call(6, "rollback", r#"{"branch":"v","checkpoint":"before"}"#),
        call(7, "merge_preview", r#"{"branch":"v"}"#),
        call(8, "undo_merge", r#"{"branch":"w"}"#),
        call(9, "merge", r#"{"branch":"s","rows":{"t/1":"ours"}}"#),
        // a later page names the first page's version: one the branch moved past is refused
        call(10, "merge_preview", r#"{"branch":"v","offset":20,"version":0}"#),
    ];
    let r = mcp(&dir, &[], &script);
    let text = |id: usize| r[id - 1]["result"]["content"][0]["text"].as_str().unwrap().to_string();
    let err = |id: usize| r[id - 1]["result"]["isError"] == true;
    assert!(!err(2) && !err(3) && !err(4) && !err(7), "{r:#?}");
    assert!(text(7).contains("pass version="), "{}", text(7));
    assert!(err(10) && text(10).contains("changed since version 0"), "{}", text(10));
    assert!(err(5) && text(5).contains("'main' isn't one"), "{}", text(5));
    assert!(err(6) && text(6).contains("'v' isn't one"), "{}", text(6));
    assert!(err(8) && text(8).contains("can't undo_merge"), "{}", text(8));
    assert!(err(9) && text(9).contains("can't merge"), "{}", text(9));
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Merging part of a branch, or into another branch, over MCP; safe mode previews it but never
/// runs it, and an agent needs the right to change the branch merged into.
#[test]
fn mcp_partial_and_into_merges() {
    let dir = std::env::temp_dir().join(format!("chronos-mcp-part-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (ok, _, err) =
        chronos(&dir, &[], "create table t (id int primary key, a text);\ncreate table u (id int primary key);\n");
    assert!(ok, "{err}");
    let script = [
        call(1, "fork", r#"{"name":"w"}"#),
        call(2, "fork", r#"{"name":"x"}"#),
        call(
            3,
            "sql",
            r#"{"sql":"insert into t values (1, 'one'), (2, 'two'); insert into u values (1)","branch":"w"}"#,
        ),
        call(4, "merge_preview", r#"{"branch":"w","only_keys":["t/1"]}"#),
        call(5, "merge", r#"{"branch":"w","into":"x","only_tables":["u"]}"#),
        call(6, "merge", r#"{"branch":"w","only_keys":["t/1"]}"#),
        call(7, "sql", r#"{"sql":"select (select count(*) from t) || '/' || (select count(*) from u) as n"}"#),
        call(8, "sql", r#"{"sql":"select count(*) as n from u","branch":"x"}"#),
        call(9, "merge", r#"{"branch":"w","only_keys":"t/2"}"#),
        call(10, "merge", r#"{"branch":"w"}"#),
    ];
    let r = mcp(&dir, &["--allow-merge"], &script);
    let text = |id: usize| r[id - 1]["result"]["content"][0]["text"].as_str().unwrap().to_string();
    let err = |id: usize| r[id - 1]["result"]["isError"] == true;
    for id in [1, 2, 3, 4, 5, 6, 7, 8, 10] {
        assert!(!err(id), "call {id}: {}", text(id));
    }
    assert!(text(4).contains("t/1") || text(4).contains("| 1 "), "{}", text(4));
    assert!(!text(4).contains("two"), "only t/1: {}", text(4));
    assert!(text(5).contains("MERGE 1") && text(6).contains("MERGE 1"), "{}{}", text(5), text(6));
    assert!(text(7).contains("1/0") && text(8).contains(" 1"), "{}{}", text(7), text(8));
    assert!(err(9) && text(9).contains("non-empty array"), "{}", text(9));
    assert!(text(10).contains("merged 2 changes"), "the rest, u/1 too (it went to x, not main): {}", text(10));

    // safe mode: a preview into another branch is a read; the merge isn't allowed
    let script = [
        call(1, "fork", r#"{"name":"s"}"#),
        call(2, "sql", r#"{"sql":"insert into t values (3, 'three')","branch":"s"}"#),
        call(3, "merge_preview", r#"{"branch":"s","into":"x"}"#),
        call(4, "merge", r#"{"branch":"s","into":"x"}"#),
    ];
    let r = mcp(&dir, &[], &script);
    let text = |id: usize| r[id - 1]["result"]["content"][0]["text"].as_str().unwrap().to_string();
    let err = |id: usize| r[id - 1]["result"]["isError"] == true;
    assert!(!err(3) && text(3).contains("apply"), "{}", text(3));
    assert!(err(4) && text(4).contains("can't merge"), "{}", text(4));

    // an agent that may publish its own worlds to main, but not change x
    let (ok, out, err) =
        chronos(&dir, &[], "create agent bot with (can = 'read,fork,write_own,write_main,merge_own');\n");
    assert!(ok, "{out}{err}");
    let script = [
        call(1, "fork", r#"{"name":"b"}"#),
        call(2, "sql", r#"{"sql":"insert into t values (4, 'four')","branch":"b"}"#),
        call(3, "merge", r#"{"branch":"b","into":"x"}"#),
        call(4, "merge", r#"{"branch":"b","only_keys":["t/4"]}"#),
    ];
    let r = mcp(&dir, &["--agent", "bot"], &script);
    let text = |id: usize| r[id - 1]["result"]["content"][0]["text"].as_str().unwrap().to_string();
    let err = |id: usize| r[id - 1]["result"]["isError"] == true;
    assert!(err(3) && text(3).contains("may not change x"), "{}", text(3));
    assert!(!err(4) && text(4).contains("MERGE 1"), "{}", text(4));
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Merge policies over MCP: an admin crafts rules, tries them on real work, and gives them to an
/// agent; the agent's small merge goes through, its big one waits in `reviews`; safe mode drafts
/// the SQL for a person instead.
#[test]
fn mcp_merge_policies() {
    let dir = std::env::temp_dir().join(format!("chronos-mcp-policies-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    {
        let db = chronos::Db::open(&dir).unwrap();
        for i in 1..=5 {
            db.put(chronos::MAIN, &format!("orders/{i}"), r#"{"status": "new"}"#).unwrap();
        }
        let can = "read,fork,write_own,write_main,merge_own";
        chronos::sql::run(
            &db,
            chronos::MAIN,
            &format!("create agent bot with (can = '{can}'); create agent boss with (can = 'admin')"),
            &[],
        )
        .unwrap();
        db.fork(chronos::MAIN, "draft").unwrap();
        for i in 1..=3 {
            db.put("draft", &format!("orders/{i}"), r#"{"status": "paid"}"#).unwrap();
        }
    }
    let text = |r: &serde_json::Value| r["result"]["content"][0]["text"].as_str().unwrap_or_default().to_string();
    let is_err = |r: &serde_json::Value| r["result"]["isError"].as_bool().unwrap_or(false);

    // the admin: try rules on real work, set them, give them to bot
    let admin = mcp(
        &dir,
        &["--agent", "boss"],
        &[
            call(1, "check_merge_policy", r#"{"branch":"draft","rules":{"max_rows":2}}"#),
            call(2, "check_merge_policy", r#"{"branch":"draft","rules":{"max_rows":10,"review_tables":["payments"]}}"#),
            call(
                3,
                "set_merge_policy",
                r#"{"name":"small","rules":{"max_rows":2,"max_deletes":0,"review_tables":["payments"]},"agents":["bot"]}"#,
            ),
            call(4, "set_merge_policy", r#"{"name":"small","rules":{"max_rows":"lots"}}"#),
            call(5, "merge_policies", "{}"),
            call(6, "set_merge_policy", r#"{"name":"small","rules":{"bad key; drop":1}}"#),
        ],
    );
    assert!(
        text(&admin[0]).contains("would wait for a person") && text(&admin[0]).contains("changes 3 rows (at most 2)"),
        "{}",
        text(&admin[0])
    );
    assert!(text(&admin[1]).contains("would go through on its own"), "{}", text(&admin[1]));
    assert!(!is_err(&admin[2]), "{}", text(&admin[2]));
    assert!(is_err(&admin[3]) && text(&admin[3]).contains("max_rows must be a number"), "{}", text(&admin[3]));
    assert!(
        text(&admin[4]).contains("small") && text(&admin[4]).contains("agent bot keeps to small"),
        "{}",
        text(&admin[4])
    );
    assert!(is_err(&admin[5]) && text(&admin[5]).contains("isn't a rule"), "{}", text(&admin[5]));

    // bot: may not set rules; a small merge goes through, a big one waits
    let bot = mcp(
        &dir,
        &["--agent", "bot"],
        &[
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#.to_string(),
            call(2, "fork", r#"{"name":"bot-1"}"#),
            call(3, "put", r#"{"table":"orders","id":"1","record":{"status":"sent"},"branch":"bot-1"}"#),
            call(4, "merge", r#"{"branch":"bot-1"}"#),
            call(5, "fork", r#"{"name":"bot-2"}"#),
            call(6, "sql", r#"{"sql":"update orders set status = 'lost'","branch":"bot-2"}"#),
            call(7, "merge", r#"{"branch":"bot-2"}"#),
            call(8, "reviews", "{}"),
            call(9, "sql", r#"{"sql":"alter merge policy small set (max_rows = 1000)"}"#),
        ],
    );
    let tools: Vec<&str> =
        bot[0]["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(
        tools.contains(&"merge") && tools.contains(&"reviews") && tools.contains(&"check_merge_policy"),
        "{tools:?}"
    );
    assert!(!tools.contains(&"set_merge_policy") && !tools.contains(&"drop_merge_policy"), "{tools:?}");
    assert!(!is_err(&bot[3]), "small: {}", text(&bot[3]));
    assert!(is_err(&bot[6]) && text(&bot[6]).contains("needs a person's review"), "{}", text(&bot[6]));
    assert!(text(&bot[7]).contains("bot-2") && text(&bot[7]).contains("changes 5 rows"), "{}", text(&bot[7]));
    assert!(is_err(&bot[8]), "only admins change rules: {}", text(&bot[8]));

    // safe mode: drafts the SQL for a person; nothing changes
    let safe = mcp(
        &dir,
        &[],
        &[
            call(
                1,
                "set_merge_policy",
                r#"{"name":"strict","rules":{"max_rows":10,"tables":["orders","items"],"schema":false}}"#,
            ),
            call(2, "drop_merge_policy", r#"{"name":"small","release_agents":true}"#),
        ],
    );
    let t = text(&safe[0]);
    assert!(
        is_err(&safe[0])
            && t.contains(
                "create merge policy 'strict' with (max_rows = 10, schema = false, tables = 'orders,items');"
            ),
        "{t}"
    );
    assert!(text(&safe[1]).contains("drop merge policy 'small' cascade;"), "{}", text(&safe[1]));

    // the admin again: drop, releasing bot
    let admin =
        mcp(&dir, &["--agent", "boss"], &[call(1, "drop_merge_policy", r#"{"name":"small","release_agents":true}"#)]);
    assert!(!is_err(&admin[0]), "{}", text(&admin[0]));
    let db = chronos::Db::open(&dir).unwrap();
    assert!(db.policies().unwrap().is_empty() && db.agent("bot").unwrap().policy.is_none());
    assert_eq!(db.reviews().len(), 1, "bot-2 still waits");
    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Merge checks over MCP: an admin sets one, an agent's merge that breaks it is refused with the
/// rows it found, and safe mode drafts the SQL for a person instead.
#[test]
fn mcp_merge_checks() {
    let dir = std::env::temp_dir().join(format!("chronos-mcp-checks-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    {
        let db = chronos::Db::open(&dir).unwrap();
        let can = "read,fork,write_own,write_main,merge_own";
        chronos::sql::run(
            &db,
            chronos::MAIN,
            &format!(
                "create table lists (id int primary key); create table leads (id int primary key, list int); \
                 insert into lists values (1); insert into leads values (1, 1); \
                 create agent bot with (can = '{can}'); create agent boss with (can = 'admin')"
            ),
            &[],
        )
        .unwrap();
    }
    let text = |r: &serde_json::Value| r["result"]["content"][0]["text"].as_str().unwrap_or_default().to_string();
    let is_err = |r: &serde_json::Value| r["result"]["isError"].as_bool().unwrap_or(false);

    let keep = r#"{"name":"keep","query":"select id from lists where id not in (select list from leads)","tables":["leads"],"timeout_ms":500}"#;
    let admin = mcp(
        &dir,
        &["--agent", "boss"],
        &[
            call(1, "set_merge_check", keep),
            call(2, "merge_checks", "{}"),
            call(3, "set_merge_check", r#"{"name":"two","query":"select 1; drop table leads"}"#),
        ],
    );
    assert!(!is_err(&admin[0]), "{}", text(&admin[0]));
    assert!(text(&admin[1]).contains("keep") && text(&admin[1]).contains("500"), "{}", text(&admin[1]));
    assert!(is_err(&admin[2]) && text(&admin[2]).contains("one SELECT"), "{}", text(&admin[2]));

    let bot = mcp(
        &dir,
        &["--agent", "bot"],
        &[
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#.to_string(),
            call(2, "fork", r#"{"name":"bot-1"}"#),
            call(3, "sql", r#"{"sql":"delete from leads","branch":"bot-1"}"#),
            call(4, "merge", r#"{"branch":"bot-1"}"#),
            call(5, "merge_checks", "{}"),
            call(6, "diff", r#"{"branch":"bot-1","readers":true}"#),
            call(7, "merge_preview", r#"{"branch":"bot-1"}"#),
        ],
    );
    let tools: Vec<&str> =
        bot[0]["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(tools.contains(&"merge_checks") && !tools.contains(&"set_merge_check"), "{tools:?}");
    assert!(
        is_err(&bot[3]) && text(&bot[3]).contains("breaks merge check keep, which finds (id = 1)"),
        "{}",
        text(&bot[3])
    );
    assert!(text(&bot[4]).contains("keep"), "{}", text(&bot[4]));
    // what reads the columns the branch changed, and what that couldn't see
    let readers = text(&bot[5]);
    assert!(
        readers.contains("merge check") && readers.contains("keep") && readers.contains("recorded since"),
        "{readers}"
    );
    assert!(
        text(&bot[6]).contains("What reads the changed columns") && text(&bot[6]).contains("leads_pkey"),
        "{}",
        text(&bot[6])
    );

    let safe = mcp(
        &dir,
        &[],
        &[
            call(
                1,
                "set_merge_check",
                r#"{"name":"nonneg","query":"select id from leads where list < 0","tables":["leads","Lists","app.orders","select"]}"#,
            ),
            call(2, "drop_merge_check", r#"{"name":"keep"}"#),
        ],
    );
    assert!(
        is_err(&safe[0])
            && text(&safe[0]).contains(
                r#"create or replace merge check 'nonneg' on tables ("leads", "Lists", "app"."orders", "select") as select id from leads where list < 0;"#
            ),
        "{}",
        text(&safe[0])
    );
    assert!(text(&safe[1]).contains("drop merge check 'keep';"), "{}", text(&safe[1]));
    let db = chronos::Db::open(&dir).unwrap();
    assert_eq!(db.checks().unwrap().len(), 1, "safe mode changed nothing");
    assert!(db.world("bot-1").is_ok(), "nothing was merged");
    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A plain `chronos mcp <folder>` serves a model the owner runs: it reads masked columns masked (SQL) and its row
/// tools are refused on tables with some; `--unmask` gives it the values (issue #45).
#[test]
fn a_plain_mcp_reads_masked_columns_masked() {
    let dir = std::env::temp_dir().join(format!("chronos-mcp-masked-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let setup = "create table customers (id int primary key, email text);\n\
                 insert into customers values (1, 'ann@example.com');\n\
                 alter table customers alter column email set masked;\n";
    let (ok, _, err) = chronos(&dir, &[], setup);
    assert!(ok, "{err}");
    let text = |r: &serde_json::Value| r["result"]["content"][0]["text"].as_str().unwrap_or_default().to_string();
    let script = [
        call(1, "sql", r#"{"sql":"select email from customers"}"#),
        call(2, "get", r#"{"table":"customers","id":"1"}"#),
    ];
    let masked = mcp(&dir, &[], &script);
    assert!(text(&masked[0]).contains("m_") && !text(&masked[0]).contains("ann@"), "{masked:?}");
    assert!(text(&masked[1]).contains("has masked columns"), "{masked:?}");
    let open = mcp(&dir, &["--unmask"], &script);
    assert!(text(&open[0]).contains("ann@example.com"), "{open:?}");
    assert!(text(&open[1]).contains("ann@example.com"), "{open:?}");
    std::fs::remove_dir_all(&dir).unwrap();
}
