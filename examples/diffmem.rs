//! Memory of a big diff: `diffmem setup DIR ROWS CHANGED` builds a table of ROWS rows, forks w and
//! updates CHANGED of them; then `diffmem open|count|page|full DIR` runs one diff in a fresh process
//! (measure each with /usr/bin/time -v).

use chronos::sql::Session;
use chronos::{Db, MAIN};
use std::time::Instant;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let db = Db::open(&a[2]).unwrap();
    let t = Instant::now();
    let run = |b: &str, q: &str| {
        Session::new(b).run(&db, q, &[]).unwrap();
    };
    match a[1].as_str() {
        "setup" => {
            let (rows, changed): (usize, usize) = (a[3].parse().unwrap(), a[4].parse().unwrap());
            run(MAIN, "create table leads (id int primary key, name text, status text, score int)");
            run(MAIN, &format!("insert into leads select g, 'lead ' || g, 'new', 0 from generate_series(1, {rows}) g"));
            db.checkpoint().unwrap();
            db.fork(MAIN, "w").unwrap();
            run("w", &format!("update leads set score = 1, status = 'contacted' where id <= {changed}"));
            db.checkpoint().unwrap();
        }
        "open" => {}
        "count" => {
            let n: usize = db.diff_count("w", None).unwrap().values().sum();
            println!("count {n}");
        }
        "page" => {
            let (p, next) = db.diff_page("w", None, None, 50).unwrap();
            println!("page {} next {:?}", p.len(), next);
        }
        "full" => println!("full {}", db.diff("w").unwrap().len()),
        _ => panic!("setup|open|count|page|full"),
    }
    println!("{} took {} ms", a[1], t.elapsed().as_millis());
}
