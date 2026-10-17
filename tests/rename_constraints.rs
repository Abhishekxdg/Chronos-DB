//! ALTER TABLE ... RENAME CONSTRAINT of primary key, unique and foreign key constraints: the
//! SQLSTATEs Postgres gives for a taken name (42P07 when the name is an index's or another
//! relation's, 42710 when it is another constraint's), and the new names after a restart replays
//! the log.

use chronos::sql::{Output, Value, run};
use chronos::{Db, MAIN};

fn sql(db: &Db, q: &str) -> Output {
    run(db, MAIN, q, &[]).unwrap_or_else(|e| panic!("{q}: {e}")).pop().unwrap()
}

fn fails(db: &Db, q: &str) -> (&'static str, String) {
    let e = run(db, MAIN, q, &[]).expect_err(q);
    (e.code, e.message)
}

fn texts(db: &Db, q: &str) -> Vec<String> {
    sql(db, q)
        .rows
        .iter()
        .map(|r| {
            r.iter().map(|v| if *v == Value::Null { "null".into() } else { v.to_text() }).collect::<Vec<_>>().join("|")
        })
        .collect()
}

fn tables(db: &Db) {
    sql(db, "create table rkp (id int primary key, u int unique, v int, constraint rkv unique (v))");
    sql(db, "create table rkc (id int primary key, pid int references rkp (id), w int, constraint rkw check (w > 0))");
    sql(db, "alter table rkp rename constraint rkp_pkey to rkp_key2");
    sql(db, "alter table rkp rename constraint rkp_u_key to rku");
    sql(db, "alter table rkc rename constraint rkc_pid_fkey to rkf");
}

#[test]
fn taken_and_missing_names() {
    let db = Db::new();
    tables(&db);
    let code = |q: &str| fails(&db, q).0;
    // a key's or unique's index is renamed first: any relation of the name is 42P07
    assert_eq!(code("alter table rkp rename constraint rku to rkv"), "42P07");
    assert_eq!(code("alter table rkp rename constraint rku to rku"), "42P07");
    assert_eq!(code("alter table rkp rename constraint rkp_key2 to rku"), "42P07");
    assert_eq!(code("alter table rkp rename constraint rku to rkc"), "42P07");
    assert_eq!(code("alter table rkp rename constraint rku to rkc_pkey"), "42P07");
    sql(&db, "create sequence rks");
    assert_eq!(code("alter table rkp rename constraint rku to rks"), "42P07");
    // another constraint of the table is 42710
    assert_eq!(code("alter table rkc rename constraint rkf to rkw"), "42710");
    assert_eq!(code("alter table rkc rename constraint rkf to rkf"), "42710");
    assert_eq!(code("alter table rkc rename constraint rkf to rkc_pkey"), "42710");
    assert_eq!(code("alter table rkc rename constraint rkw to rkf"), "42710");
    // the old names are gone, and a unique index is no constraint
    assert_eq!(code("alter table rkp rename constraint rkp_pkey to x"), "42704");
    assert_eq!(code("alter table rkp rename constraint rkp_u_key to x"), "42704");
    sql(&db, "create unique index rkx on rkp (v, u)");
    assert_eq!(code("alter table rkp rename constraint rkx to x"), "42704");
    // a constraint of another table may have the name
    sql(&db, "alter table rkp rename constraint rkp_key2 to rkw");
    assert_eq!(
        fails(&db, "insert into rkp values (1, 1, 1), (1, 2, 2)").1,
        "duplicate key value violates unique constraint \"rkw\""
    );
}

#[test]
fn renamed_keys_survive_a_reopen() {
    let dir = std::env::temp_dir().join(format!("chronos-rename-constraints-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    {
        let db = Db::open(&dir).unwrap();
        sql(&db, "create table rkp (id int primary key, u int unique, v int, constraint rkv unique (v))");
        sql(&db, "insert into rkp values (1, 1, 1)");
        sql(
            &db,
            "create table rkc (id int primary key, pid int references rkp (id), w int, constraint rkw check (w > 0))",
        );
        sql(&db, "insert into rkc values (1, 1, 1)");
        sql(&db, "alter table rkp rename constraint rkp_pkey to rkp_key2");
        sql(&db, "alter table rkp rename constraint rkp_u_key to rku");
        sql(&db, "alter table rkc rename constraint rkc_pid_fkey to rkf");
    }
    let db = Db::open(&dir).unwrap();
    assert_eq!(
        texts(
            &db,
            "select conname, contype from pg_constraint where conrelid in ('rkp'::regclass, 'rkc'::regclass) order by conname"
        ),
        ["rkc_pkey|p", "rkf|f", "rkp_key2|p", "rku|u", "rkv|u", "rkw|c"]
    );
    assert_eq!(
        texts(&db, "select indexname from pg_indexes where tablename = 'rkp' order by 1"),
        ["rkp_key2", "rku", "rkv"]
    );
    // the rows that keep the constraints are under the new names
    let (code, msg) = fails(&db, "insert into rkp values (1, 2, 2)");
    assert_eq!(code, "23505");
    assert!(msg.contains("\"rkp_key2\""), "{msg}");
    let (code, msg) = fails(&db, "insert into rkp values (2, 1, 2)");
    assert_eq!(code, "23505");
    assert!(msg.contains("\"rku\""), "{msg}");
    let (code, msg) = fails(&db, "insert into rkc values (2, 9, 1)");
    assert_eq!(code, "23503");
    assert!(msg.contains("rkf"), "{msg}");
    let (code, msg) = fails(&db, "delete from rkp");
    assert_eq!(code, "23503");
    assert!(msg.contains("rkf"), "{msg}");
    sql(&db, "insert into rkp values (1, 5, 5) on conflict on constraint rkp_key2 do update set v = 10");
    sql(&db, "insert into rkp values (3, 1, 3) on conflict on constraint rku do nothing");
    assert_eq!(texts(&db, "select id, u, v from rkp order by id"), ["1|1|10"]);
    // dropped by the new names
    assert_eq!(fails(&db, "alter table rkp drop constraint rkp_u_key").0, "42704");
    sql(&db, "alter table rkp drop constraint rku");
    sql(&db, "alter table rkc drop constraint rkf");
    sql(&db, "insert into rkp values (2, 1, 2)");
    sql(&db, "insert into rkc values (2, 9, 1)");
    drop(db);
    let db = Db::open(&dir).unwrap();
    assert_eq!(
        texts(
            &db,
            "select conname from pg_constraint where conrelid in ('rkp'::regclass, 'rkc'::regclass) order by conname"
        ),
        ["rkc_pkey", "rkp_key2", "rkv", "rkw"]
    );
    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn partition_links_and_new_key_names_survive_a_reopen() {
    let dir = std::env::temp_dir().join(format!("chronos-rename-links-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let fks = "select c.conrelid::regclass::text, c.conname, p.conname, c.confdeltype from pg_constraint c \
               left join pg_constraint p on p.oid = c.conparentid where c.contype = 'f' \
               and c.conrelid in ('pft'::regclass, 'pft1'::regclass) order by 1, 2";
    {
        let db = Db::open(&dir).unwrap();
        sql(&db, "create table pfr (id int primary key)");
        sql(
            &db,
            "create table pft (k int, rid int, constraint fa foreign key (rid) references pfr, \
             constraint fb foreign key (rid) references pfr on delete cascade) partition by range (k)",
        );
        sql(&db, "create table pft1 partition of pft for values from (0) to (10)");
        sql(&db, "alter table pft rename constraint fa to fz");
        sql(&db, "alter table pft rename constraint fb to fa");
        sql(&db, "alter table pft1 rename constraint fb to fb1");
        // a table renamed keeps its key's name; one made under its old name gets the next
        sql(&db, "create table nka (id int primary key)");
        sql(&db, "alter table nka rename to nkb");
        sql(&db, "create table nka (id int primary key)");
    }
    let db = Db::open(&dir).unwrap();
    assert_eq!(texts(&db, fks), ["pft|fa|null|c", "pft|fz|null|a", "pft1|fa|fz|a", "pft1|fb1|fa|c"]);
    assert_eq!(
        texts(&db, "select indexname, tablename from pg_indexes where tablename in ('nka', 'nkb') order by 1"),
        ["nka_pkey|nkb", "nka_pkey1|nka"]
    );
    let (code, msg) = fails(&db, "insert into nka values (1), (1)");
    assert_eq!(code, "23505");
    assert!(msg.contains("\"nka_pkey1\""), "{msg}");
    // the parent's fa (made as fb, ON DELETE CASCADE) goes with the partition's copy of it
    sql(&db, "alter table pft drop constraint fa");
    assert_eq!(texts(&db, fks), ["pft|fz|null|a", "pft1|fa|fz|a"]);
    sql(&db, "insert into pfr values (1)");
    sql(&db, "insert into pft values (1, 1)");
    assert_eq!(fails(&db, "delete from pfr").0, "23503");
    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn partition_copy_named_for_its_columns_survives_a_reopen() {
    let dir = std::env::temp_dir().join(format!("chronos-rename-copies-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let fks = "select c.conrelid::regclass::text, c.conname, p.conname from pg_constraint c \
               left join pg_constraint p on p.oid = c.conparentid where c.contype = 'f' \
               and c.conrelid in ('fnp'::regclass, 'fnp1'::regclass, 'fnp2'::regclass) order by 1, 2";
    {
        let db = Db::open(&dir).unwrap();
        sql(&db, "create table fnr (id int primary key, b int unique)");
        sql(
            &db,
            "create table fnp (id int, r int, s int, constraint fa foreign key (r) references fnr) partition by range (id)",
        );
        sql(&db, "create table fnp1 partition of fnp for values from (0) to (10)");
        sql(&db, "alter table fnp rename constraint fa to fz");
        // the partition has fa, its copy of fz: the copy of the new fa is named for its column
        sql(&db, "alter table fnp add constraint fa foreign key (s) references fnr (b)");
        // attached with a constraint of the name: the same
        sql(&db, "create table fnp2 (id int, r int, s int, constraint fa check (s > 0))");
        sql(&db, "alter table fnp attach partition fnp2 for values from (10) to (20)");
    }
    let db = Db::open(&dir).unwrap();
    assert_eq!(
        texts(&db, fks),
        ["fnp|fa|null", "fnp|fz|null", "fnp1|fa|fz", "fnp1|fnp1_s_fkey|fa", "fnp2|fnp2_s_fkey|fa", "fnp2|fz|fz"]
    );
    let (code, msg) = fails(&db, "insert into fnp values (1, null, 999)");
    assert_eq!(code, "23503");
    assert!(msg.contains("fnp1_s_fkey"), "{msg}");
    // dropping the parent's fa takes the copies of it, and leaves those of fz
    sql(&db, "alter table fnp drop constraint fa");
    assert_eq!(texts(&db, fks), ["fnp|fz|null", "fnp1|fa|fz", "fnp2|fz|fz"]);
    sql(&db, "insert into fnp values (1, null, 999), (11, null, 999)");
    let (code, msg) = fails(&db, "insert into fnp values (1, 999, null)");
    assert_eq!(code, "23503");
    assert!(msg.contains("fa"), "{msg}");
    assert_eq!(fails(&db, "insert into fnp values (11, 999, null)").0, "23503");
    drop(db);
    let db = Db::open(&dir).unwrap();
    assert_eq!(texts(&db, fks), ["fnp|fz|null", "fnp1|fa|fz", "fnp2|fz|fz"]);
    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn partition_foreign_key_taken_as_copy_survives_a_reopen() {
    let dir = std::env::temp_dir().join(format!("chronos-rename-adopted-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let fks = "select c.conrelid::regclass::text, c.conname, p.conname from pg_constraint c \
               left join pg_constraint p on p.oid = c.conparentid where c.contype = 'f' \
               and c.conrelid in ('pat'::regclass, 'pat1'::regclass, 'pat2'::regclass) order by 1, 2";
    {
        let db = Db::open(&dir).unwrap();
        sql(&db, "create table par (id int primary key)");
        sql(&db, "create table pat (id int, a int, rid int) partition by range (a)");
        sql(&db, "create table pat1 partition of pat for values from (0) to (10)");
        sql(&db, "create table pat2 partition of pat for values from (10) to (20)");
        sql(&db, "alter table pat1 add constraint g foreign key (rid) references par");
        sql(&db, "alter table pat2 add constraint h foreign key (rid) references par");
        // each partition's own is the copy of the new one
        sql(&db, "alter table pat add constraint g foreign key (rid) references par");
    }
    let db = Db::open(&dir).unwrap();
    assert_eq!(texts(&db, fks), ["pat|g|null", "pat1|g|g", "pat2|h|g"]);
    sql(&db, "alter table pat drop constraint g");
    assert!(texts(&db, fks).is_empty());
    sql(&db, "insert into pat values (1, 1, 7), (11, 11, 7)");
    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
}
