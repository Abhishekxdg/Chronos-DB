//! The demo's sample data: 5,000 customers and their orders, as CSV files `chronos import` reads.
//!
//!   cargo run --example sample_data -- <out dir> [--today YYYY-MM-DD]
//!   chronos import mydb customers <out>/customers.csv --schema <out>/customers.schema.csv
//!   chronos import mydb orders <out>/orders.csv --schema <out>/orders.schema.csv
//!
//! Dates are relative to today (so "inactive for 90 days" means the same thing any day you run
//! it), and the seed is fixed, so every run has exactly CHURNED = 1,240 customers whose
//! last_active is more than 90 days ago: they're 93 to 700 days, and everyone else 0 to 87, so
//! `< current_date - 90` and `<= current_date - 90` both find the same 1,240.

use std::fmt::Write as _;
use std::path::Path;

const CUSTOMERS: usize = 5_000;
const CHURNED: usize = 1_240;

const FIRST: &[&str] = &[
    "Ada",
    "Grace",
    "Alan",
    "Linus",
    "Margaret",
    "Ken",
    "Barbara",
    "Dennis",
    "Frances",
    "Edsger",
    "Radia",
    "Bjarne",
    "Sophie",
    "Guido",
    "Hedy",
    "Tim",
    "Katherine",
    "John",
    "Annie",
    "Donald",
];
const LAST: &[&str] = &[
    "Lovelace",
    "Hopper",
    "Turing",
    "Torvalds",
    "Hamilton",
    "Thompson",
    "Liskov",
    "Ritchie",
    "Allen",
    "Dijkstra",
    "Perlman",
    "Stroustrup",
    "Wilson",
    "van Rossum",
    "Lamarr",
    "Berners-Lee",
    "Johnson",
    "Backus",
    "Easley",
    "Knuth",
];
const PLANS: &[&str] = &["free", "free", "free", "pro", "pro", "team"];
const COUNTRIES: &[&str] = &["US", "IN", "DE", "GB", "BR", "JP", "FR", "CA"];
const NOTES: &[&str] = &[
    "",
    "",
    "asked about annual billing",
    "reported a login error on mobile",
    "wants SSO for the team plan",
    "billing error on the last invoice",
    "prefers email over phone",
    "exporting data to a spreadsheet every week",
];

struct Rng(u64);
impl Rng {
    fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % n as u64) as usize
    }
    fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
        xs[self.below(xs.len())]
    }
}

/// Days since 1970-01-01 as YYYY-MM-DD (Howard Hinnant's civil_from_days).
fn date(days: i64) -> String {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    format!("{:04}-{m:02}-{d:02}", yoe + era * 400 + (m <= 2) as i64)
}

/// YYYY-MM-DD as days since 1970-01-01 (days_from_civil).
fn days(s: &str) -> Option<i64> {
    let mut it = s.split('-').map(|p| p.parse::<i64>().ok());
    let (y, m, d) = (it.next()??, it.next()??, it.next()??);
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    Some(era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468)
}

/// The files' contents: (name, text), for customers and orders plus their schema.csv files.
fn generate(today: i64) -> Vec<(&'static str, String)> {
    let mut r = Rng(2026);
    // which customers churn: a seeded shuffle, the first CHURNED of it
    let mut order: Vec<usize> = (0..CUSTOMERS).collect();
    for i in (1..CUSTOMERS).rev() {
        order.swap(i, r.below(i + 1));
    }
    let mut churned = vec![false; CUSTOMERS];
    for &i in &order[..CHURNED] {
        churned[i] = true;
    }

    let mut customers = String::from("id,name,email,plan,country,status,signed_up,last_active,notes\n");
    let mut orders = String::from("id,customer_id,amount,created_at\n");
    let mut order_id = 0;
    for (i, &gone) in churned.iter().enumerate() {
        let id = i + 1;
        let (first, last) = (r.pick(FIRST), r.pick(LAST));
        let idle = if gone { 93 + r.below(608) } else { r.below(88) } as i64;
        let last_active = today - idle;
        let signed_up = last_active - 30 - r.below(900) as i64;
        let email = format!("{}.{}{id}@example.com", first.to_lowercase(), last.to_lowercase().replace(' ', ""));
        let note = r.pick(NOTES);
        let note = if note.is_empty() { String::new() } else { format!("\"{note}\"") };
        let _ = writeln!(
            customers,
            "{id},{first} {last},{email},{},{},active,{},{},{note}",
            r.pick(PLANS),
            r.pick(COUNTRIES),
            date(signed_up),
            date(last_active)
        );
        for _ in 0..r.below(6) {
            order_id += 1;
            let when = signed_up + r.below((last_active - signed_up) as usize + 1) as i64;
            let cents = 500 + r.below(49_500);
            let _ = writeln!(orders, "{order_id},{id},{}.{:02},{}", cents / 100, cents % 100, date(when));
        }
    }
    let customers_schema = "id,bigint,NO,t\nname,text,NO,f\nemail,text,YES,f\nplan,text,NO,f\ncountry,text,YES,f\n\
        status,text,NO,f\nsigned_up,date,NO,f\nlast_active,date,NO,f\nnotes,text,YES,f\n";
    let orders_schema = "id,bigint,NO,t\ncustomer_id,bigint,NO,f\namount,numeric,NO,f\ncreated_at,date,NO,f\n";
    vec![
        ("customers.csv", customers),
        ("customers.schema.csv", customers_schema.into()),
        ("orders.csv", orders),
        ("orders.schema.csv", orders_schema.into()),
    ]
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let usage = "usage: cargo run --example sample_data -- <out dir> [--today YYYY-MM-DD]";
    let (out, today) = match &args[..] {
        [out] => {
            let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
            (out, secs as i64 / 86_400)
        }
        [out, flag, day] if flag == "--today" => (out, days(day).unwrap_or_else(|| panic!("{usage}"))),
        _ => panic!("{usage}"),
    };
    std::fs::create_dir_all(out).unwrap();
    for (name, text) in generate(today) {
        std::fs::write(Path::new(out).join(name), text).unwrap();
    }
    println!(
        "wrote {CUSTOMERS} customers ({CHURNED} inactive for over 90 days as of {}) and their orders to {out}",
        date(today)
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exactly_1240_churn_whichever_day() {
        assert_eq!(days("1970-01-01"), Some(0));
        assert_eq!(date(days("2024-02-29").unwrap()), "2024-02-29");
        for today in ["2026-09-24", "2027-03-01"] {
            let t = days(today).unwrap();
            let files = generate(t);
            let rows: Vec<&str> = files[0].1.lines().skip(1).collect();
            assert_eq!(rows.len(), CUSTOMERS);
            let idle: Vec<i64> = rows
                .iter()
                .map(|l| {
                    let f: Vec<&str> = l.split(',').collect();
                    t - days(f[7]).unwrap()
                })
                .collect();
            assert_eq!(idle.iter().filter(|&&d| d > 90).count(), CHURNED, "as of {today}");
            assert_eq!(idle.iter().filter(|&&d| d >= 90).count(), CHURNED, "no one sits on the boundary");
            assert_eq!(files, generate(t), "same seed, same files");
        }
    }
}
