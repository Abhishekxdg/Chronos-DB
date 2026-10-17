//! View::each_rev is exactly View::each backwards, over pages and recent changes alike.

use chronos::{Db, Error, MAIN};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

#[test]
fn backwards_is_forwards_reversed() {
    let dir = std::env::temp_dir().join(format!("chronos-rev-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Db::open(&dir).unwrap();
    db.fork(MAIN, "w").unwrap(); // a branch: its writes aren't fsynced one by one
    let mut r = Rng(7);
    let key = |r: &mut Rng| format!("k/{:04}", r.next() % 3000);
    for round in 0..6u32 {
        for _ in 0..800 {
            let k = key(&mut r);
            if r.next().is_multiple_of(4) {
                db.del("w", &k).unwrap();
            } else {
                db.put("w", &k, &format!("v{round}")).unwrap();
            }
        }
        db.put("w", "a-before", "x").unwrap(); // rows outside the prefix, on both sides
        db.put("w", "z-after", "x").unwrap();
        if round.is_multiple_of(2) {
            db.checkpoint().unwrap(); // some rows in pages, some only in memory
        }
        let v = db.view("w").unwrap();
        for _ in 0..50 {
            let (a, b) = (key(&mut r), key(&mut r));
            let after = (r.next().is_multiple_of(3)).then_some(a.as_str());
            let upto = (r.next().is_multiple_of(3)).then_some(b.as_str());
            let mut fwd = vec![];
            v.each("k/", after, upto, |k, _| {
                fwd.push(k.to_string());
                Ok::<_, Error>(true)
            })
            .unwrap();
            let mut back = vec![];
            v.each_rev("k/", after, upto, |k, _| {
                back.push(k.to_string());
                Ok::<_, Error>(true)
            })
            .unwrap();
            back.reverse();
            assert_eq!(fwd, back, "after {after:?} upto {upto:?}");
            // stopping early gives the top rows
            let mut top = vec![];
            v.each_rev("k/", after, upto, |k, _| {
                top.push(k.to_string());
                Ok::<_, Error>(top.len() < 5)
            })
            .unwrap();
            assert_eq!(top, fwd.iter().rev().take(5).cloned().collect::<Vec<_>>());
        }
    }
    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
}
