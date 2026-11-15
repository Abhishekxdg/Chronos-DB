//! What a role with little may not do to get more (the 2026-11-10 security review): make or drop a rule on a table
//! that isn't its own, whose actions would run as whoever writes the table next; rotate an agent's token, which
//! hands it the agent; or end up a superuser because its role was dropped while its login was under way.

use chronos::sql::Session;
use chronos::{Db, MAIN};

fn ok(db: &Db, s: &mut Session, sql: &str) {
    s.run(db, sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e}"));
}

/// The one value `sql` gives.
fn one(db: &Db, s: &mut Session, sql: &str) -> String {
    let out = s.run(db, sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e}")).pop().unwrap();
    out.rows[0][0].to_text()
}

/// The SQLSTATE `sql` is refused with.
fn refused(db: &Db, s: &mut Session, sql: &str) -> &'static str {
    match s.run(db, sql, &[]) {
        Ok(_) => panic!("{sql}: ran, and should have been refused"),
        Err(e) => e.code,
    }
}

/// A session logged in as role `name`.
fn role(db: &Db, name: &str) -> Session {
    let mut s = Session::new(MAIN);
    s.user = Some(name.into());
    s.log_in(db, name).unwrap();
    s
}

#[test]
fn a_rule_is_its_tables_owners_to_make_drop_and_switch() {
    let (db, mut admin) = (Db::new(), Session::new(MAIN));
    ok(
        &db,
        &mut admin,
        "create role low login;
         create table orders (id int primary key, card text);
         grant select, insert, update, delete on orders to low;
         grant create on schema public to low;
         create rule guard as on delete to orders do instead nothing;
         insert into orders values (1, '4111')",
    );
    let mut low = role(&db, "low");
    // not on a table it only has privileges on: its actions would run as the next role to write the table
    for sql in [
        "create rule z as on insert to orders do also alter role low superuser",
        "create rule z as on update to orders do instead nothing",
        "create or replace rule guard as on delete to orders do also delete from orders",
        "drop rule guard on orders",
        "alter table orders disable rule guard",
        "alter table orders enable rule guard",
    ] {
        assert_eq!(refused(&db, &mut low, sql), "42501", "{sql}");
    }
    // the owner's rule is as it was: it still takes the delete, and no other was added
    ok(&db, &mut admin, "delete from orders");
    assert_eq!(one(&db, &mut admin, "select count(*) from orders"), "1");
    ok(&db, &mut admin, "insert into orders values (2, '4242')");
    assert_eq!(one(&db, &mut admin, "select rolsuper from pg_roles where rolname = 'low'"), "false");
    // on a table of its own, a rule is its to make, switch and drop
    ok(&db, &mut low, "create table mine (a int)");
    ok(&db, &mut low, "create rule quiet as on insert to mine do instead nothing");
    ok(&db, &mut low, "alter table mine disable rule quiet");
    ok(&db, &mut low, "drop rule quiet on mine");
}

#[test]
fn an_agents_token_is_a_superusers_to_rotate() {
    let (db, mut admin) = (Db::new(), Session::new(MAIN));
    ok(&db, &mut admin, "create role low login; create agent bot with (can = 'read')");
    let mut low = role(&db, "low");
    // (the statement answers with the new token: whoever may run it is the agent from then on)
    assert_eq!(refused(&db, &mut low, "alter agent bot rotate token"), "42501");
    assert_eq!(refused(&db, &mut low, "alter agent nobody rotate token"), "42501", "nor told which agents there are");
    ok(&db, &mut admin, "alter agent bot rotate token");
}

#[test]
fn a_role_dropped_or_changed_while_it_logs_in_is_refused() {
    let (db, mut admin) = (Db::new(), Session::new(MAIN));
    ok(&db, &mut admin, "create role r login password 'first-password-123'");
    // what a login as r proves, read before the exchange (see src/pg.rs)
    let proved = Session::role_password(&db, "r").unwrap().unwrap().0.unwrap();
    let login = |db: &Db| {
        let mut s = Session::new(MAIN);
        s.user = Some("r".into());
        s.log_in_role(db, "r", &proved).map(|()| s)
    };

    // still that role with that password: logged in as itself, with its own rights and no more
    let mut s = login(&db).unwrap();
    assert_eq!(one(&db, &mut s, "select current_user"), "r");
    assert_eq!(refused(&db, &mut s, "create role other"), "42501");

    // given another password meanwhile: what it proved is no longer the role's
    ok(&db, &mut admin, "alter role r password 'second-password-456'");
    assert_eq!(login(&db).map(|_| ()).unwrap_err().code, "28P01");

    // dropped meanwhile: r is a name that is no role now, which a plain login makes a superuser
    ok(&db, &mut admin, "drop role r");
    assert_eq!(login(&db).map(|_| ()).unwrap_err().code, "28P01");

    // and a role of that name made again is another role, with another password
    ok(&db, &mut admin, "create role r login password 'first-password-123'");
    assert_eq!(login(&db).map(|_| ()).unwrap_err().code, "28P01", "a new verifier has a new salt");
}
