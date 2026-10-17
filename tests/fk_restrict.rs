//! A foreign key keeps the action it was declared with: RESTRICT and NO ACTION behave alike here
//! but the catalogs (and schema diff tools reading them) tell them apart.

use chronos::sql::run;
use chronos::{Db, MAIN};

fn rule(db: &Db, name: &str) -> String {
    let sql =
        format!("select delete_rule from information_schema.referential_constraints where constraint_name = '{name}'");
    let out = run(db, MAIN, &sql, &[]).unwrap().pop().unwrap();
    out.rows[0][0].to_text()
}

#[test]
fn restrict_is_reported_as_restrict() {
    let db = Db::new();
    for sql in [
        "create table p (id int primary key)",
        "create table a (id int primary key, p int, constraint a_fk foreign key (p) references p (id) on delete restrict)",
        "create table b (id int primary key, p int, constraint b_fk foreign key (p) references p (id) on delete no action)",
        "create table c (id int primary key, p int, constraint c_fk foreign key (p) references p (id))",
        "create table d (id int primary key, p int, constraint d_fk foreign key (p) references p (id) on delete cascade)",
        "create table e (id int primary key, p int, constraint e_fk foreign key (p) references p (id) on delete set null)",
    ] {
        run(&db, MAIN, sql, &[]).unwrap();
    }
    assert_eq!(rule(&db, "a_fk"), "RESTRICT");
    assert_eq!(rule(&db, "b_fk"), "NO ACTION");
    assert_eq!(rule(&db, "c_fk"), "NO ACTION");
    assert_eq!(rule(&db, "d_fk"), "CASCADE");
    assert_eq!(rule(&db, "e_fk"), "SET NULL");

    let def = |n: &str| {
        let sql = format!("select pg_get_constraintdef(oid) from pg_constraint where conname = '{n}'");
        run(&db, MAIN, &sql, &[]).unwrap().pop().unwrap().rows[0][0].to_text()
    };
    assert_eq!(def("a_fk"), "FOREIGN KEY (p) REFERENCES p(id) ON DELETE RESTRICT");
    assert_eq!(def("b_fk"), "FOREIGN KEY (p) REFERENCES p(id)");
}

#[test]
fn restrict_and_no_action_both_refuse_the_delete() {
    let db = Db::new();
    for sql in [
        "create table p (id int primary key)",
        "create table a (id int primary key, p int references p (id) on delete restrict)",
        "create table b (id int primary key, p int references p (id) on delete no action)",
        "insert into p values (1), (2)",
        "insert into a values (1, 1)",
        "insert into b values (1, 2)",
    ] {
        run(&db, MAIN, sql, &[]).unwrap();
    }
    for id in [1, 2] {
        let e = run(&db, MAIN, &format!("delete from p where id = {id}"), &[]).expect_err("still referenced");
        assert_eq!(e.code, "23503");
    }
}
