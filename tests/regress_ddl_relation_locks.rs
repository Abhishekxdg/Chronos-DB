//! The relation locks DDL statements hold until their transaction ends, as pg_locks shows them: ALTER TABLE's at the
//! levels Postgres's AlterTableGetLockLevel gives, and what its foreign keys, CLUSTER ON and VALIDATE CONSTRAINT reach
//! besides; CREATE TRIGGER's, ALTER INDEX's and ALTER VIEW's; held under a renamed relation's new name; and the
//! transaction's own number, which a change to the catalogs gives it. Also ALTER TABLE naming a view (done as ALTER
//! VIEW does it, or refused) or a system catalog, and CREATE TABLE in pg_catalog.

use chronos::sql::{Session, Value};
use chronos::{Db, MAIN};

fn run(db: &Db, s: &mut Session, q: &str) -> Vec<Vec<String>> {
    let out = s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {} {}", e.code, e.message));
    out.last().unwrap().rows.iter().map(|r| r.iter().map(Value::to_text).collect()).collect()
}

fn fails(db: &Db, s: &mut Session, q: &str) -> (&'static str, String, String) {
    match s.run(db, q, &[]) {
        Ok(_) => panic!("{q}: should fail"),
        Err(e) => (e.code, e.message, e.info.and_then(|i| i.detail).unwrap_or_default()),
    }
}

/// The regression suite's `my_locks`: the relations the running transaction holds, each with its strongest mode.
fn setup(db: &Db, s: &mut Session) {
    run(
        db,
        s,
        "create type lockmodes as enum ('SIReadLock', 'AccessShareLock', 'RowShareLock', 'RowExclusiveLock',
           'ShareUpdateExclusiveLock', 'ShareLock', 'ShareRowExclusiveLock', 'ExclusiveLock', 'AccessExclusiveLock')",
    );
    run(
        db,
        s,
        "create view my_locks as
         select case when c.relname like 'pg_toast%' then 'pg_toast' else c.relname end, max(mode::lockmodes) as max_lockmode
         from pg_locks l join pg_class c on l.relation = c.oid
         where virtualtransaction = (
                 select virtualtransaction
                 from pg_locks
                 where transactionid = pg_current_xact_id()::xid)
         and locktype = 'relation'
         and relnamespace != (select oid from pg_namespace where nspname = 'pg_catalog')
         and c.relname != 'my_locks'
         group by c.relname",
    );
    run(db, s, "create table alterlock (f1 int primary key, f2 text); insert into alterlock values (1, 'foo')");
    run(db, s, "create table alterlock2 (f3 int primary key, f1 int); insert into alterlock2 values (1, 1)");
}

fn locks(db: &Db, s: &mut Session) -> Vec<String> {
    run(db, s, "select * from my_locks order by 1").into_iter().map(|r| r.join(":")).collect()
}

/// `alter` in a transaction of its own: what it holds, then rolled back.
fn held(db: &Db, s: &mut Session, alter: &str) -> Vec<String> {
    run(db, s, "begin");
    run(db, s, alter);
    let held = locks(db, s);
    run(db, s, "rollback");
    held
}

#[test]
fn alter_table_holds_the_lock_its_changes_need() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    setup(&db, &mut s);
    let sue = "ShareUpdateExclusiveLock";
    let ael = "AccessExclusiveLock";
    for (alter, mode) in [
        ("alter table alterlock alter column f2 set statistics 150", sue),
        ("alter table alterlock set without cluster", sue),
        // (the TOAST table is locked too, but isn't in pg_class to be shown)
        ("alter table alterlock set (fillfactor = 100)", sue),
        ("alter table alterlock reset (fillfactor)", sue),
        ("alter table alterlock set (toast.autovacuum_enabled = off)", sue),
        ("alter table alterlock alter column f2 set (n_distinct = 1)", sue),
        ("alter table alterlock set (autovacuum_enabled = off, fillfactor = 80)", sue),
        ("alter table alterlock alter column f2 set storage extended", ael),
        ("alter table alterlock alter column f2 set default 'x'", ael),
        ("alter table alterlock add column f3 int", ael),
        ("alter table alterlock alter column f2 set statistics 150, alter column f2 drop default", ael),
    ] {
        assert_eq!(held(&db, &mut s, alter), [format!("alterlock:{mode}")], "{alter}");
    }
    // CLUSTER ON locks the index too
    assert_eq!(
        held(&db, &mut s, "alter table alterlock cluster on alterlock_pkey"),
        ["alterlock:ShareUpdateExclusiveLock", "alterlock_pkey:ShareUpdateExclusiveLock"]
    );
    // the locks go with the transaction, and a transaction that hasn't changed anything holds none
    run(&db, &mut s, "begin");
    assert!(locks(&db, &mut s).is_empty());
    run(&db, &mut s, "rollback");
    // nor does a statement outside one leave any behind
    run(&db, &mut s, "alter table alterlock alter column f2 set statistics 100");
    assert_eq!(run(&db, &mut s, "select count(*) from pg_locks where locktype = 'relation'"), [["0"]]);
}

#[test]
fn a_foreign_key_locks_both_tables_and_their_indexes() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    setup(&db, &mut s);
    run(&db, &mut s, "begin");
    assert!(locks(&db, &mut s).is_empty());
    run(&db, &mut s, "alter table alterlock2 add foreign key (f1) references alterlock (f1)");
    assert_eq!(
        locks(&db, &mut s),
        [
            "alterlock:ShareRowExclusiveLock",
            "alterlock2:ShareRowExclusiveLock",
            "alterlock2_pkey:AccessShareLock",
            "alterlock_pkey:AccessShareLock"
        ]
    );
    run(&db, &mut s, "rollback");
    // NOT VALID checks no rows: only the referenced key's index is held (as Postgres 16.15 holds it)
    run(&db, &mut s, "begin");
    run(
        &db,
        &mut s,
        "alter table alterlock2 add constraint alterlock2nv foreign key (f1) references alterlock (f1) not valid",
    );
    assert_eq!(
        locks(&db, &mut s),
        ["alterlock:ShareRowExclusiveLock", "alterlock2:ShareRowExclusiveLock", "alterlock_pkey:AccessShareLock"]
    );
    run(&db, &mut s, "commit");
    // a unique index that isn't a constraint's is a key a foreign key leans on too
    run(&db, &mut s, "create table alterlock3 (f4 int, f1 int); create unique index alterlock3_f4 on alterlock3 (f4)");
    assert_eq!(
        held(&db, &mut s, "alter table alterlock2 add foreign key (f1) references alterlock3 (f4) not valid"),
        ["alterlock2:ShareRowExclusiveLock", "alterlock3:ShareRowExclusiveLock", "alterlock3_f4:AccessShareLock"]
    );
    assert_eq!(
        held(&db, &mut s, "alter table alterlock2 validate constraint alterlock2nv"),
        [
            "alterlock:RowShareLock",
            "alterlock2:ShareUpdateExclusiveLock",
            "alterlock2_pkey:AccessShareLock",
            "alterlock_pkey:AccessShareLock"
        ]
    );
}

#[test]
fn create_trigger_keeps_writers_out() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    setup(&db, &mut s);
    run(&db, &mut s, "create function trig() returns trigger language plpgsql as $$ begin return new; end $$");
    assert_eq!(
        held(
            &db,
            &mut s,
            "create trigger t before delete or update on alterlock for each row execute procedure trig()"
        ),
        ["alterlock:ShareRowExclusiveLock"]
    );
}

#[test]
fn a_renamed_relation_is_held_under_its_new_name() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create table alter_idx_rename_test (a int)");
    run(&db, &mut s, "create index alter_idx_rename_test_idx on alter_idx_rename_test (a)");
    let held = |s: &mut Session| -> Vec<String> {
        run(
            &db,
            s,
            "select relation::regclass, mode from pg_locks
             where pid = pg_backend_pid() and locktype = 'relation' and relation::regclass::text like 'alter\\_idx%'
             order by relation::regclass::text collate \"C\", mode",
        )
        .into_iter()
        .map(|r| r.join(":"))
        .collect()
    };
    // ALTER INDEX naming a table locks it as ALTER TABLE does
    run(&db, &mut s, "begin");
    run(&db, &mut s, "alter index alter_idx_rename_test rename to alter_idx_rename_test_2");
    assert_eq!(held(&mut s), ["alter_idx_rename_test_2:AccessExclusiveLock"]);
    run(&db, &mut s, "commit");
    // an index, against other changes only
    run(&db, &mut s, "begin");
    run(&db, &mut s, "alter index alter_idx_rename_test_idx rename to alter_idx_rename_test_idx_2");
    assert_eq!(held(&mut s), ["alter_idx_rename_test_idx_2:ShareUpdateExclusiveLock"]);
    run(&db, &mut s, "commit");
    // ALTER TABLE naming an index locks it against everyone
    run(&db, &mut s, "begin");
    run(&db, &mut s, "alter table alter_idx_rename_test_idx_2 rename to alter_idx_rename_test_idx_3");
    assert_eq!(held(&mut s), ["alter_idx_rename_test_idx_3:AccessExclusiveLock"]);
    run(&db, &mut s, "commit");
    // a lock taken before the rename follows it
    run(&db, &mut s, "begin");
    run(&db, &mut s, "alter table alter_idx_rename_test_2 alter column a set statistics 10");
    run(&db, &mut s, "alter table alter_idx_rename_test_2 rename to alter_idx_rename_test_3");
    assert_eq!(
        held(&mut s),
        ["alter_idx_rename_test_3:AccessExclusiveLock", "alter_idx_rename_test_3:ShareUpdateExclusiveLock"]
    );
    run(&db, &mut s, "rollback");
    // a ROLLBACK TO undoing the rename leaves what was held before it under the old name
    run(&db, &mut s, "begin");
    run(&db, &mut s, "alter table alter_idx_rename_test_2 alter column a set statistics 10");
    run(&db, &mut s, "savepoint s");
    run(&db, &mut s, "alter table alter_idx_rename_test_2 rename to alter_idx_rename_test_3");
    assert_eq!(
        held(&mut s),
        ["alter_idx_rename_test_3:AccessExclusiveLock", "alter_idx_rename_test_3:ShareUpdateExclusiveLock"]
    );
    run(&db, &mut s, "rollback to savepoint s");
    assert_eq!(held(&mut s), ["alter_idx_rename_test_2:ShareUpdateExclusiveLock"]);
    // (and a RELEASE keeps the rename's)
    run(&db, &mut s, "savepoint s2");
    run(&db, &mut s, "alter table alter_idx_rename_test_2 rename to alter_idx_rename_test_3");
    run(&db, &mut s, "release savepoint s2");
    assert_eq!(
        held(&mut s),
        ["alter_idx_rename_test_3:AccessExclusiveLock", "alter_idx_rename_test_3:ShareUpdateExclusiveLock"]
    );
    run(&db, &mut s, "rollback");
    // a table moved to another schema takes its indexes, and what is held of them, along
    run(&db, &mut s, "create schema alter_idx_schema");
    run(&db, &mut s, "begin");
    run(&db, &mut s, "alter table alter_idx_rename_test_2 cluster on alter_idx_rename_test_idx_3");
    run(&db, &mut s, "alter table alter_idx_rename_test_2 set schema alter_idx_schema");
    assert_eq!(
        run(
            &db,
            &mut s,
            "select relation::regclass, mode from pg_locks
             where pid = pg_backend_pid() and locktype = 'relation' and relation::regclass::text like 'alter\\_idx%'
             order by relation::regclass::text collate \"C\", mode",
        )
        .into_iter()
        .map(|r| r.join(":"))
        .collect::<Vec<_>>(),
        [
            "alter_idx_schema.alter_idx_rename_test_2:AccessExclusiveLock",
            "alter_idx_schema.alter_idx_rename_test_2:ShareUpdateExclusiveLock",
            "alter_idx_schema.alter_idx_rename_test_idx_3:ShareUpdateExclusiveLock"
        ]
    );
    run(&db, &mut s, "rollback");
}

#[test]
fn the_transaction_holds_its_number_and_a_savepoint_rolled_back_lets_go() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    setup(&db, &mut s);
    let xact =
        "select count(*) from pg_locks where locktype = 'transactionid' and transactionid = pg_current_xact_id()::xid";
    run(&db, &mut s, "begin");
    assert_eq!(run(&db, &mut s, "select count(*) from pg_locks where locktype = 'transactionid'"), [["0"]]);
    run(&db, &mut s, "alter table alterlock set without cluster");
    assert_eq!(run(&db, &mut s, xact), [["1"]]);
    run(&db, &mut s, "savepoint a");
    run(&db, &mut s, "alter table alterlock2 add column x int");
    assert_eq!(locks(&db, &mut s), ["alterlock:ShareUpdateExclusiveLock", "alterlock2:AccessExclusiveLock"]);
    run(&db, &mut s, "rollback to savepoint a");
    assert_eq!(locks(&db, &mut s), ["alterlock:ShareUpdateExclusiveLock"]);
    run(&db, &mut s, "commit");
    assert_eq!(
        run(&db, &mut s, "select count(*) from pg_locks where locktype in ('relation', 'transactionid')"),
        [["0"]]
    );
}

#[test]
fn alter_table_naming_a_view_does_what_alter_view_does() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    setup(&db, &mut s);
    run(&db, &mut s, "create table def_test (c1 int, c2 text)");
    run(&db, &mut s, "create view def_view_test as select * from def_test");
    run(
        &db,
        &mut s,
        "create rule def_view_test_ins as on insert to def_view_test do instead insert into def_test select new.*",
    );
    run(&db, &mut s, "alter table def_view_test alter column c1 set default 45");
    run(&db, &mut s, "insert into def_view_test default values");
    run(&db, &mut s, "alter table def_view_test alter column c2 set default 'view_default'");
    run(&db, &mut s, "insert into def_view_test default values");
    assert_eq!(
        run(&db, &mut s, "select c1, coalesce(c2, '-') from def_view_test order by 2"),
        [["45", "-"], ["45", "view_default"]]
    );
    // what a view can't have is refused
    let not_for_views = |action: &str| {
        (
            "42809",
            format!("ALTER action {action} cannot be performed on relation \"def_view_test\""),
            "This operation is not supported for views.".to_string(),
        )
    };
    assert_eq!(
        fails(&db, &mut s, "alter table def_view_test alter column c1 drop not null"),
        not_for_views("ALTER COLUMN ... DROP NOT NULL")
    );
    assert_eq!(
        fails(&db, &mut s, "alter table def_view_test alter column c1 set not null"),
        not_for_views("ALTER COLUMN ... SET NOT NULL")
    );
    assert_eq!(fails(&db, &mut s, "alter table def_view_test drop c2"), not_for_views("DROP COLUMN"));
    // a table's option isn't a view's; a RESET of one says nothing
    let (code, message, _) = fails(&db, &mut s, "alter table my_locks set (autovacuum_enabled = false)");
    assert_eq!((code, message.as_str()), ("22023", "unrecognized parameter \"autovacuum_enabled\""));
    run(&db, &mut s, "alter table my_locks reset (autovacuum_enabled)");
    // a view's option, set through ALTER TABLE or ALTER VIEW, locks the view against everyone
    run(
        &db,
        &mut s,
        "create or replace view my_locks as
         select c.relname, max(mode::lockmodes) as max_lockmode
         from pg_locks l join pg_class c on l.relation = c.oid
         where virtualtransaction = (select virtualtransaction from pg_locks where transactionid = pg_current_xact_id()::xid)
         and locktype = 'relation' and c.relname = 'my_locks'
         group by c.relname",
    );
    for (alter, mode) in [
        ("alter table my_locks set (security_barrier = off)", "AccessExclusiveLock"),
        ("alter view my_locks set (security_barrier = off)", "AccessExclusiveLock"),
        // (a RESET of a table's option, which a view ignores, locks as that option needs)
        ("alter view my_locks reset (user_catalog_table)", "AccessExclusiveLock"),
        ("alter view my_locks reset (fillfactor)", "ShareUpdateExclusiveLock"),
        ("alter table my_locks reset (fillfactor)", "ShareUpdateExclusiveLock"),
    ] {
        assert_eq!(held(&db, &mut s, alter), [format!("my_locks:{mode}")], "{alter}");
    }
}

#[test]
fn alter_table_naming_a_view_changes_all_or_nothing() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create table at_t (x int, t text)");
    run(&db, &mut s, "create view at_v as select * from at_t");
    run(&db, &mut s, "create rule at_v_ins as on insert to at_v do instead insert into at_t select new.*");
    let inserted = |s: &mut Session| -> Vec<Vec<String>> {
        run(&db, s, "delete from at_t");
        run(&db, s, "insert into at_v default values");
        run(&db, s, "select coalesce(x::text, '-'), coalesce(t, '-') from at_t")
    };
    let options = "select coalesce(array_to_string(reloptions, ','), '-') from pg_class where relname = 'at_v'";
    // a change failing leaves those before it unmade
    let (code, message, _) =
        fails(&db, &mut s, "alter table at_v alter column x set default 1, alter column nosuch set default 2");
    assert_eq!((code, message.as_str()), ("42703", "column \"nosuch\" of relation \"at_v\" does not exist"));
    let (code, message, _) =
        fails(&db, &mut s, "alter table at_v alter column t set default 'q', set (security_barrier = maybe)");
    assert_eq!((code, message.as_str()), ("22023", "invalid value for boolean option \"security_barrier\": maybe"));
    let (code, _, _) =
        fails(&db, &mut s, "alter table at_v set (security_barrier = true), alter column nosuch drop default");
    assert_eq!(code, "42703");
    assert_eq!(inserted(&mut s), [["-", "-"]]);
    assert_eq!(run(&db, &mut s, options), [["-"]]);
    // and all of them made together
    run(
        &db,
        &mut s,
        "alter table at_v alter column x set default 1, alter column t set default 'q', set (security_barrier)",
    );
    assert_eq!(inserted(&mut s), [["1", "q"]]);
    assert_eq!(run(&db, &mut s, options), [["security_barrier=true"]]);
    // a column renamed keeps its default
    run(&db, &mut s, "alter table at_v rename column t to t2");
    assert_eq!(inserted(&mut s), [["1", "q"]]);
    // a TOAST table's parameter a view, having none, takes and ignores; any other namespace it refuses, and RESET
    // takes no values
    run(&db, &mut s, "alter table at_v set (toast.autovacuum_enabled = off)");
    assert_eq!(run(&db, &mut s, options), [["security_barrier=true"]]);
    let (code, message, _) = fails(&db, &mut s, "alter table at_v set (foo.bar = 1)");
    assert_eq!((code, message.as_str()), ("22023", "unrecognized parameter namespace \"foo\""));
    let (code, message, _) = fails(&db, &mut s, "alter table at_v reset (security_barrier = true)");
    assert_eq!((code, message.as_str()), ("42601", "RESET must not include values for parameters"));
    run(&db, &mut s, "alter table at_v reset (toast.security_barrier)");
    assert_eq!(run(&db, &mut s, options), [["security_barrier=true"]]);
    run(&db, &mut s, "alter table at_v reset (security_barrier)");
    assert_eq!(run(&db, &mut s, options), [["-"]]);
}

#[test]
fn system_catalogs_are_not_altered_and_pg_catalog_takes_no_tables() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    for q in ["alter table pg_class alter column relname drop not null", "alter table pg_class drop column relname"] {
        let (code, message, _) = fails(&db, &mut s, q);
        assert_eq!((code, message.as_str()), ("42501", "permission denied: \"pg_class\" is a system catalog"), "{q}");
    }
    assert_eq!(
        fails(&db, &mut s, "create table pg_catalog.new_system_table()"),
        (
            "42501",
            "permission denied to create \"pg_catalog.new_system_table\"".to_string(),
            "System catalog modifications are currently disallowed.".to_string()
        )
    );
    // (a table of the name is made in public as before)
    run(&db, &mut s, "create table new_system_table (id serial primary key, othercol text)");
    run(&db, &mut s, "alter table new_system_table rename to old_system_table");
    assert_eq!(run(&db, &mut s, "select count(*) from old_system_table"), [["0"]]);
}
