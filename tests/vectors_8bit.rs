//! Vector search through SQL with every column's vectors at 8 bits a number, as big tables keep
//! them (`CHRONOS_VECTOR_FLOATS=0`; small ones keep floats): it still finds the true nearest rows,
//! over all of them and through a filter, since the index's last candidates are ranked from the
//! rows' own vectors.

use chronos::Db;
use chronos::sql::{Session, Value};

struct Rng(u64);
impl Rng {
    fn f(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 40) as f32 / (1u64 << 24) as f32 - 0.5
    }
    fn unit(&mut self, dim: usize) -> Vec<f32> {
        let v: Vec<f32> = (0..dim).map(|_| self.f()).collect();
        let l = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        v.into_iter().map(|x| x / l).collect()
    }
}

fn lit(v: &[f32]) -> String {
    format!("[{}]", v.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(","))
}

fn ids(db: &Db, s: &mut Session, q: &str) -> Vec<i64> {
    let out = s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {e}")).pop().unwrap();
    out.rows
        .iter()
        .map(|r| match &r[0] {
            Value::Int(i) => *i,
            v => panic!("id {v:?}"),
        })
        .collect()
}

#[test]
fn sql_vector_search_with_eight_bit_numbers_finds_the_nearest() {
    // SAFETY: before any search in this test binary reads it
    unsafe { std::env::set_var("CHRONOS_VECTOR_FLOATS", "0") };
    let (n, dim) = (4000, 96);
    let mut r = Rng(7);
    let db = Db::new();
    let s = &mut Session::new(chronos::MAIN);
    s.run(&db, &format!("create table items (id int primary key, team int, embedding vector({dim}))"), &[]).unwrap();
    let vecs: Vec<Vec<f32>> = (0..n).map(|_| r.unit(dim)).collect();
    for chunk in vecs.iter().enumerate().collect::<Vec<_>>().chunks(200) {
        let rows: Vec<String> = chunk.iter().map(|(i, v)| format!("({i}, {}, '{}')", i % 50, lit(v))).collect();
        s.run(&db, &format!("insert into items values {}", rows.join(",")), &[]).unwrap();
    }
    s.run(&db, "create index on items using hnsw (embedding vector_cosine_ops)", &[]).unwrap();
    let dot = |a: &[f32], b: &[f32]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>();
    let truth = |q: &[f32], team: Option<usize>| -> Vec<i64> {
        let mut all: Vec<(i64, f32)> =
            (0..n).filter(|i| team.is_none_or(|t| i % 50 == t)).map(|i| (i as i64, dot(q, &vecs[i]))).collect();
        all.sort_by(|a, b| b.1.total_cmp(&a.1));
        all.into_iter().take(10).map(|(i, _)| i).collect()
    };
    let (mut hit, mut hit_team) = (0, 0);
    for qi in 0..30 {
        let q = r.unit(dim);
        let got = ids(&db, s, &format!("select id from items order by embedding <=> '{}' limit 10", lit(&q)));
        hit += truth(&q, None).iter().filter(|i| got.contains(i)).count();
        let t = qi % 50;
        let got = ids(
            &db,
            s,
            &format!("select id from items where team = {t} order by embedding <=> '{}' limit 10", lit(&q)),
        );
        hit_team += truth(&q, Some(t)).iter().filter(|i| got.contains(i)).count();
    }
    // the 8-bit numbers only propose candidates; the last ones are ranked from the rows' vectors
    assert!(hit >= 295, "{hit} of the true top 300 over all rows");
    assert_eq!(hit_team, 300, "through a filter (80 rows each) the true top 10, every time");
}
