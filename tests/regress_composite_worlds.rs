//! A composite type is an object of its world: an ALTER TYPE in a fork changes the type there only, and main (and
//! every other world) keeps reading its own shape of it, in whatever order sessions of the two run their statements,
//! on one thread or two at once, across a reopen, until the fork merges. Each test reads the shapes back through
//! scripts and through statements parsed first and run later (the extended protocol's Parse, then Execute), with a
//! statement of the other world in between. A policy's condition is read with its world's shape too; and a world
//! whose rows change other than by its own DDL (a transaction's commit, a merge, a rebase, a restore) has the new
//! shape at once, in the same script and in other sessions' statements parsed before. A merge makes the index entries
//! of the rows it brings, and checks them, with the shapes the merged state has, whoever runs it: a session of another
//! world, or none (the API), beside another database with a type of the same name.

use chronos::sql::{Session, SqlError, Statement, Type, Value};
use chronos::{Db, MAIN};

fn run(db: &Db, s: &mut Session, q: &str) -> Vec<Vec<String>> {
    let out = s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {} {}", e.code, e.message));
    out.last().unwrap().rows.iter().map(|r| r.iter().map(Value::to_text).collect()).collect()
}

/// `run`, with parameters.
fn run_with(db: &Db, s: &mut Session, q: &str, params: &[Value]) -> Vec<Vec<String>> {
    let out = s.run(db, q, params).unwrap_or_else(|e| panic!("{q}: {} {}", e.code, e.message));
    out.last().unwrap().rows.iter().map(|r| r.iter().map(Value::to_text).collect()).collect()
}

fn fails(db: &Db, s: &mut Session, q: &str) -> SqlError {
    match s.run(db, q, &[]) {
        Ok(o) => panic!("{q}: should fail, gave {:?}", o.last().map(|o| o.rows.clone())),
        Err(e) => e,
    }
}

/// `q` parsed in `s`'s world now, to run later (see `exec`).
fn prepare(db: &Db, s: &Session, q: &str) -> (Statement, Vec<Type>) {
    let st = s.parse(db, q).unwrap_or_else(|e| panic!("{q}: {} {}", e.code, e.message)).pop().unwrap();
    let types = s.params(db, &st, &[]).unwrap_or_else(|e| panic!("{q}: {} {}", e.code, e.message));
    (st, types)
}

fn exec(db: &Db, s: &mut Session, st: &(Statement, Vec<Type>)) -> Result<Vec<Vec<String>>, SqlError> {
    let out = s.execute(db, &st.0, &[], &st.1)?;
    Ok(out.rows.iter().map(|r| r.iter().map(Value::to_text).collect()).collect())
}

/// One column of rows.
fn col(rows: Vec<Vec<String>>) -> Vec<String> {
    rows.into_iter().map(|mut r| r.remove(0)).collect()
}

/// Main, with type `ty` of attributes `attrs` and table `<ty>_t` of a column `v` of it holding row 1 (`first`), and
/// world `<ty>_w` forked from it then: a session in each.
fn forked(ty: &str, attrs: &str, first: &str) -> (Db, Session, Session) {
    let (db, mut m) = (Db::new(), Session::new(MAIN));
    run(&db, &mut m, &format!("create type {ty} as ({attrs})"));
    run(&db, &mut m, &format!("create table {ty}_t (id int primary key, v {ty})"));
    run(&db, &mut m, &format!("insert into {ty}_t values (1, {first})"));
    run(&db, &mut m, &format!("create world {ty}_w"));
    let w = Session::new(&format!("{ty}_w"));
    (db, m, w)
}

#[test]
fn an_attribute_added_in_a_fork_is_the_forks_only() {
    let (db, mut m, mut w) = forked("cw_add", "x int, y text", "row(1, 'a')");
    run(&db, &mut w, "alter type cw_add add attribute z int");
    // statement by statement, each world its own shape
    assert_eq!(col(run(&db, &mut m, "select v::text from cw_add_t")), ["(1,a)"]);
    assert_eq!(col(run(&db, &mut w, "select v::text from cw_add_t")), ["(1,a,)"]);
    run(&db, &mut m, "insert into cw_add_t values (2, row(2, 'b'))");
    run(&db, &mut w, "insert into cw_add_t values (3, row(3, 'c', 3))");
    assert_eq!(col(run(&db, &mut m, "select row(5, 'e')::cw_add::text")), ["(5,e)"]);
    assert_eq!(col(run(&db, &mut w, "select row(5, 'e', 5)::cw_add::text")), ["(5,e,5)"]);
    assert_eq!(fails(&db, &mut m, "select (v).z from cw_add_t").code, "42703");
    assert_eq!(col(run(&db, &mut w, "select (v).z from cw_add_t order by id")), ["", "3"]);
    assert_eq!(col(run(&db, &mut m, "select v::text from cw_add_t order by id")), ["(1,a)", "(2,b)"]);
    assert_eq!(col(run(&db, &mut w, "select v::text from cw_add_t order by id")), ["(1,a,)", "(3,c,3)"]);
    // parsed right after the other world's statement
    assert_eq!(col(run(&db, &mut m, "select 1")), ["1"]);
    let cast = prepare(&db, &w, "select row(6, 'f', 6)::cw_add::text");
    assert_eq!(exec(&db, &mut w, &cast).map(col), Ok(vec!["(6,f,6)".to_string()]));
    let field = prepare(&db, &w, "select (v).z from cw_add_t where id = 3");
    assert_eq!(col(run(&db, &mut w, "select 1")), ["1"]);
    let cast = prepare(&db, &m, "select row(6, 'f')::cw_add::text");
    assert_eq!(exec(&db, &mut m, &cast).map(col), Ok(vec!["(6,f)".to_string()]));
    // and run after the other world's statement: its tables read with its own shape still
    assert_eq!(col(run(&db, &mut m, "select v::text from cw_add_t where id = 1")), ["(1,a)"]);
    assert_eq!(exec(&db, &mut w, &field).map(col), Ok(vec!["3".to_string()]));
    let whole = prepare(&db, &m, "select v::text from cw_add_t order by id");
    assert_eq!(col(run(&db, &mut w, "select v::text from cw_add_t where id = 3")), ["(3,c,3)"]);
    assert_eq!(exec(&db, &mut m, &whole).map(col), Ok(vec!["(1,a)".to_string(), "(2,b)".to_string()]));
    let insert = prepare(&db, &m, "insert into cw_add_t values (4, row(4, 'd'))");
    assert_eq!(col(run(&db, &mut w, "select (v).z from cw_add_t where id = 3")), ["3"]);
    exec(&db, &mut m, &insert).unwrap();
    assert_eq!(col(run(&db, &mut m, "select v::text from cw_add_t where id = 4")), ["(4,d)"]);
    // the catalog of each
    let attrs = "select string_agg(attname, ',' order by attnum) from pg_attribute \
                 where attrelid = (select typrelid from pg_type where typname = 'cw_add') and attnum > 0";
    assert_eq!(col(run(&db, &mut m, attrs)), ["x,y"]);
    assert_eq!(col(run(&db, &mut w, attrs)), ["x,y,z"]);
}

#[test]
fn an_attribute_dropped_retyped_or_renamed_in_a_fork_is_so_there_only() {
    // dropped
    let (db, mut m, mut w) = forked("cw_drop", "x int, y text, z int", "row(1, 'a', 9)");
    run(&db, &mut w, "alter type cw_drop drop attribute z");
    let main_z = prepare(&db, &m, "select (v).z from cw_drop_t");
    assert_eq!(col(run(&db, &mut w, "select v::text from cw_drop_t")), ["(1,a)"]);
    assert_eq!(exec(&db, &mut m, &main_z).map(col), Ok(vec!["9".to_string()]));
    assert_eq!(fails(&db, &mut w, "select (v).z from cw_drop_t").code, "42703");
    let fork_z = prepare(&db, &w, "select (v).z from cw_drop_t");
    assert_eq!(col(run(&db, &mut m, "select v::text from cw_drop_t")), ["(1,a,9)"]);
    assert_eq!(exec(&db, &mut w, &fork_z).map(col).map_err(|e| e.code), Err("42703"));

    // retyped (no column holds it: ALTER ATTRIBUTE ... TYPE refuses one that does)
    let (db, mut m) = (Db::new(), Session::new(MAIN));
    run(&db, &mut m, "create type cw_retype as (x int, y text)");
    run(&db, &mut m, "create world cw_retype_w");
    let mut w = Session::new("cw_retype_w");
    run(&db, &mut w, "alter type cw_retype alter attribute x type text");
    let main_x = prepare(&db, &m, "select pg_typeof((row(1, 'a')::cw_retype).x)::text");
    assert_eq!(col(run(&db, &mut w, "select pg_typeof((row(1, 'a')::cw_retype).x)::text")), ["text"]);
    assert_eq!(exec(&db, &mut m, &main_x).map(col), Ok(vec!["integer".to_string()]));
    let fork_x = prepare(&db, &w, "select pg_typeof((row(1, 'a')::cw_retype).x)::text");
    assert_eq!(col(run(&db, &mut m, "select pg_typeof((row(1, 'a')::cw_retype).x)::text")), ["integer"]);
    assert_eq!(exec(&db, &mut w, &fork_x).map(col), Ok(vec!["text".to_string()]));

    // renamed
    let (db, mut m, mut w) = forked("cw_ren", "x int, y text", "row(1, 'a')");
    run(&db, &mut w, "alter type cw_ren rename attribute y to yy");
    let main_y = prepare(&db, &m, "select (v).y from cw_ren_t");
    assert_eq!(col(run(&db, &mut w, "select (v).yy from cw_ren_t")), ["a"]);
    assert_eq!(exec(&db, &mut m, &main_y).map(col), Ok(vec!["a".to_string()]));
    assert_eq!(fails(&db, &mut m, "select (v).yy from cw_ren_t").code, "42703");
    let fork_yy = prepare(&db, &w, "select (v).yy from cw_ren_t");
    assert_eq!(col(run(&db, &mut m, "select (v).y from cw_ren_t")), ["a"]);
    assert_eq!(exec(&db, &mut w, &fork_yy).map(col), Ok(vec!["a".to_string()]));
    assert_eq!(fails(&db, &mut w, "select (v).y from cw_ren_t").code, "42703");
}

#[test]
fn typed_tables_arrays_and_nested_types_take_their_worlds_shape() {
    // a typed table: changed with its type in the fork, and one made of the type in each world has its columns
    let (db, mut m) = (Db::new(), Session::new(MAIN));
    run(&db, &mut m, "create type cw_typed as (a int, b text)");
    run(&db, &mut m, "create table cw_typed_t of cw_typed");
    run(&db, &mut m, "insert into cw_typed_t values (1, 'x')");
    run(&db, &mut m, "create world cw_typed_w");
    let mut w = Session::new("cw_typed_w");
    run(&db, &mut w, "alter type cw_typed add attribute c int cascade");
    let cols = |t: &str| {
        format!(
            "select string_agg(attname, ',' order by attnum) from pg_attribute \
             where attrelid = '{t}'::regclass and attnum > 0 and not attisdropped"
        )
    };
    assert_eq!(col(run(&db, &mut m, &cols("cw_typed_t"))), ["a,b"]);
    assert_eq!(col(run(&db, &mut w, &cols("cw_typed_t"))), ["a,b,c"]);
    let made = prepare(&db, &w, "create table cw_typed_f of cw_typed");
    run(&db, &mut m, "select 1");
    exec(&db, &mut w, &made).unwrap();
    let made = prepare(&db, &m, "create table cw_typed_m of cw_typed");
    run(&db, &mut w, "select 1");
    exec(&db, &mut m, &made).unwrap();
    assert_eq!(col(run(&db, &mut w, &cols("cw_typed_f"))), ["a,b,c"]);
    assert_eq!(col(run(&db, &mut m, &cols("cw_typed_m"))), ["a,b"]);
    run(&db, &mut w, "insert into cw_typed_t values (2, 'y', 2)");
    assert_eq!(run(&db, &mut w, "select * from cw_typed_t order by a"), [["1", "x", ""], ["2", "y", "2"]]);
    assert_eq!(run(&db, &mut m, "select * from cw_typed_t order by a"), [["1", "x"]]);

    // an array of the type (its elements kept as text, read as the type by a cast): an attribute renamed in the fork
    // is read by its name there, by the old one in main
    let (db, mut m) = (Db::new(), Session::new(MAIN));
    run(&db, &mut m, "create type cw_arr as (x int, y text)");
    run(&db, &mut m, "create table cw_arr_t (id int primary key, a cw_arr[])");
    run(&db, &mut m, "insert into cw_arr_t values (1, array[row(1, 'a')::cw_arr, row(2, 'b')::cw_arr])");
    run(&db, &mut m, "create world cw_arr_w");
    let mut w = Session::new("cw_arr_w");
    run(&db, &mut w, "alter type cw_arr rename attribute y to yy");
    let main_y = prepare(&db, &m, "select (a[2]::cw_arr).y from cw_arr_t");
    assert_eq!(col(run(&db, &mut w, "select (a[2]::cw_arr).yy from cw_arr_t")), ["b"]);
    assert_eq!(exec(&db, &mut m, &main_y).map(col), Ok(vec!["b".to_string()]));
    run(&db, &mut w, "insert into cw_arr_t values (2, array[row(5, 'e')::cw_arr])");
    let fork_yy = prepare(&db, &w, "select ((array[row(3, 'c')::cw_arr])[1]::cw_arr).yy");
    assert_eq!(col(run(&db, &mut m, "select ((array[row(3, 'c')::cw_arr])[1]::cw_arr).y")), ["c"]);
    assert_eq!(exec(&db, &mut w, &fork_yy).map(col), Ok(vec!["c".to_string()]));
    assert_eq!(fails(&db, &mut m, "select ((array[row(3, 'c')::cw_arr])[1]::cw_arr).yy").code, "42703");
    assert_eq!(col(run(&db, &mut w, "select (a[1]::cw_arr).yy from cw_arr_t order by id")), ["a", "e"]);
    assert_eq!(col(run(&db, &mut m, "select (a[1]::cw_arr).y from cw_arr_t order by id")), ["a"]);

    // a type with an attribute of the type: its attribute's shape is the world's too
    let (db, mut m) = (Db::new(), Session::new(MAIN));
    run(&db, &mut m, "create type cw_inner as (x int, y text)");
    run(&db, &mut m, "create type cw_outer as (i cw_inner, n int)");
    run(&db, &mut m, "create table cw_outer_t (id int primary key, o cw_outer)");
    run(&db, &mut m, "insert into cw_outer_t values (1, row(row(1, 'a'), 2))");
    run(&db, &mut m, "create world cw_outer_w");
    let mut w = Session::new("cw_outer_w");
    run(&db, &mut w, "alter type cw_inner rename attribute y to yy");
    let main_y = prepare(&db, &m, "select ((o).i).y from cw_outer_t");
    assert_eq!(col(run(&db, &mut w, "select ((o).i).yy from cw_outer_t")), ["a"]);
    assert_eq!(exec(&db, &mut m, &main_y).map(col), Ok(vec!["a".to_string()]));
    let fork_yy = prepare(&db, &w, "select ((row(row(3, 'c'), 4)::cw_outer).i).yy");
    assert_eq!(col(run(&db, &mut m, "select ((row(row(3, 'c'), 4)::cw_outer).i).y")), ["c"]);
    assert_eq!(exec(&db, &mut w, &fork_yy).map(col), Ok(vec!["c".to_string()]));
    assert_eq!(fails(&db, &mut m, "select ((o).i).yy from cw_outer_t").code, "42703");
    assert_eq!(fails(&db, &mut w, "select ((o).i).y from cw_outer_t").code, "42703");
}

#[test]
fn functions_of_the_type_and_variables_of_it_take_their_worlds_shape() {
    let (db, mut m) = (Db::new(), Session::new(MAIN));
    run(&db, &mut m, "create type cw_fn as (x int, y text)");
    run(&db, &mut m, "create function cw_fn_y(r cw_fn) returns text language sql as $$ select (r).y $$");
    run(
        &db,
        &mut m,
        "create function cw_fn_json() returns text language plpgsql as $$ declare r cw_fn; begin r := row(1, 'p'); \
         return row_to_json(r)::text; end $$",
    );
    run(&db, &mut m, "create world cw_fn_w");
    let mut w = Session::new("cw_fn_w");
    run(&db, &mut w, "alter type cw_fn rename attribute y to yy");
    // the fork reads them first, then main, then the fork again: each with its own shape every time (a variable's
    // shape shows in the names row_to_json gives its fields)
    for _ in 0..2 {
        assert_eq!(col(run(&db, &mut w, "select cw_fn_json()")), [r#"{"x":1,"yy":"p"}"#]);
        assert_eq!(fails(&db, &mut w, "select cw_fn_y(row(1, 'q'))").code, "42703");
        assert_eq!(col(run(&db, &mut m, "select cw_fn_json()")), [r#"{"x":1,"y":"p"}"#]);
        assert_eq!(col(run(&db, &mut m, "select cw_fn_y(row(1, 'q'))")), ["q"]);
    }
    run(&db, &mut w, "create function cw_fn_yy(r cw_fn) returns text language sql as $$ select (r).yy $$");
    assert_eq!(col(run(&db, &mut w, "select cw_fn_yy(row(1, 'q'))")), ["q"]);
    let called = prepare(&db, &w, "select cw_fn_yy(row(2, 's'))");
    assert_eq!(col(run(&db, &mut m, "select cw_fn_y(row(1, 'q'))")), ["q"]);
    assert_eq!(exec(&db, &mut w, &called).map(col), Ok(vec!["s".to_string()]));
}

#[test]
fn two_sessions_in_two_worlds_at_once() {
    let (db, mut m, mut w) = forked("cw_race", "x int, y text", "row(-1, 'a')");
    run(&db, &mut w, "alter type cw_race add attribute z int");
    let rounds = 150;
    std::thread::scope(|scope| {
        let db = &db;
        let main = scope.spawn(move || {
            let mut wrong = vec![];
            for i in 0..rounds {
                let id = 1000 + i;
                let mut check = |q: String, want: String| match m.run(db, &q, &[]) {
                    Ok(o) => {
                        let got: Vec<String> = o.last().unwrap().rows.iter().map(|r| r[0].to_text()).collect();
                        if got != [want.clone()] {
                            wrong.push(format!("main: {q}: {got:?}, not {want}"));
                        }
                    }
                    Err(e) => wrong.push(format!("main: {q}: {} {}", e.code, e.message)),
                };
                check(
                    format!("insert into cw_race_t values ({id}, row({i}, 'm')) returning v::text"),
                    format!("({i},m)"),
                );
                check(format!("select v::text from cw_race_t where id = {id}"), format!("({i},m)"));
                check(format!("select row({i}, 'm')::cw_race::text"), format!("({i},m)"));
                check(format!("select count(*) from cw_race_t where (v).x = {i}"), "1".into());
            }
            wrong
        });
        let fork = scope.spawn(move || {
            let mut wrong = vec![];
            for i in 0..rounds {
                let id = 2000 + i;
                let mut check = |q: String, want: String| match w.run(db, &q, &[]) {
                    Ok(o) => {
                        let got: Vec<String> = o.last().unwrap().rows.iter().map(|r| r[0].to_text()).collect();
                        if got != [want.clone()] {
                            wrong.push(format!("fork: {q}: {got:?}, not {want}"));
                        }
                    }
                    Err(e) => wrong.push(format!("fork: {q}: {} {}", e.code, e.message)),
                };
                check(
                    format!("insert into cw_race_t values ({id}, row({i}, 'f', {i})) returning v::text"),
                    format!("({i},f,{i})"),
                );
                check(format!("select v::text from cw_race_t where id = {id}"), format!("({i},f,{i})"));
                check(format!("select (row({i}, 'f', {i})::cw_race).z"), format!("{i}"));
                check(format!("select (v).z from cw_race_t where id = {id}"), format!("{i}"));
            }
            wrong
        });
        let mut wrong = main.join().unwrap();
        wrong.extend(fork.join().unwrap());
        assert!(wrong.is_empty(), "{} wrong, the first: {:#?}", wrong.len(), &wrong[..wrong.len().min(8)]);
    });
    let (mut m, mut w) = (Session::new(MAIN), Session::new("cw_race_w"));
    assert_eq!(col(run(&db, &mut m, "select count(*) from cw_race_t")), [(rounds + 1).to_string()]);
    assert_eq!(col(run(&db, &mut w, "select count(*) from cw_race_t where (v).z is not null")), [rounds.to_string()]);
}

#[test]
fn a_reopened_database_reads_each_worlds_shape_from_its_rows() {
    let dir = std::env::temp_dir().join(format!("chronos-composite-worlds-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    {
        let db = Db::open(&dir).unwrap();
        let mut m = Session::new(MAIN);
        run(&db, &mut m, "create type cw_open as (x int, y text)");
        run(&db, &mut m, "create table cw_open_t (id int primary key, v cw_open)");
        run(&db, &mut m, "insert into cw_open_t values (1, row(1, 'a'))");
        run(&db, &mut m, "create world cw_open_w");
        let mut w = Session::new("cw_open_w");
        run(&db, &mut w, "alter type cw_open add attribute z int");
        run(&db, &mut w, "insert into cw_open_t values (2, row(2, 'b', 2))");
    }
    let db = Db::open(&dir).unwrap();
    let (mut m, mut w) = (Session::new(MAIN), Session::new("cw_open_w"));
    // (the fork's statement parsed first, before any statement of the reopened database ran in it)
    let fork_z = prepare(&db, &w, "select (v).z from cw_open_t order by id");
    assert_eq!(col(run(&db, &mut m, "select v::text from cw_open_t")), ["(1,a)"]);
    assert_eq!(exec(&db, &mut w, &fork_z).map(col), Ok(vec![String::new(), "2".to_string()]));
    let main_cast = prepare(&db, &m, "select row(3, 'c')::cw_open::text");
    assert_eq!(col(run(&db, &mut w, "select v::text from cw_open_t order by id")), ["(1,a,)", "(2,b,2)"]);
    assert_eq!(exec(&db, &mut m, &main_cast).map(col), Ok(vec!["(3,c)".to_string()]));
    assert_eq!(fails(&db, &mut m, "select (v).z from cw_open_t").code, "42703");
    drop((m, w));
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn merged_back_main_has_the_forks_shape() {
    let (db, mut m, mut w) = forked("cw_merge", "x int, y text", "row(1, 'a')");
    run(&db, &mut w, "alter type cw_merge add attribute z int");
    run(&db, &mut w, "insert into cw_merge_t values (2, row(2, 'b', 2))");
    let before = prepare(&db, &m, "select v::text from cw_merge_t order by id");
    assert_eq!(exec(&db, &mut m, &before).map(col), Ok(vec!["(1,a)".to_string()]));
    run(&db, &mut m, "merge world cw_merge_w");
    assert_eq!(col(run(&db, &mut m, "select v::text from cw_merge_t order by id")), ["(1,a,)", "(2,b,2)"]);
    assert_eq!(col(run(&db, &mut m, "select (v).z from cw_merge_t order by id")), ["", "2"]);
    assert_eq!(col(run(&db, &mut m, "select row(3, 'c', 3)::cw_merge::text")), ["(3,c,3)"]);
    run(&db, &mut m, "insert into cw_merge_t values (3, row(3, 'c', 3))");
    assert_eq!(col(run(&db, &mut m, "select (v).z from cw_merge_t where id = 3")), ["3"]);
    // a world forked after the merge has it too
    run(&db, &mut m, "create world cw_merge_w2");
    let mut w2 = Session::new("cw_merge_w2");
    assert_eq!(col(run(&db, &mut w2, "select (v).z from cw_merge_t where id = 3")), ["3"]);
}

#[test]
fn a_type_made_in_a_fork_is_not_in_main() {
    let (db, mut m) = (Db::new(), Session::new(MAIN));
    run(&db, &mut m, "create world cw_only_w");
    let mut w = Session::new("cw_only_w");
    run(&db, &mut w, "create type cw_only as (x int, y text)");
    assert_eq!(col(run(&db, &mut w, "select row(1, 'a')::cw_only::text")), ["(1,a)"]);
    let cast = prepare(&db, &m, "select 1");
    exec(&db, &mut m, &cast).unwrap();
    assert_eq!(fails(&db, &mut m, "select row(1, 'a')::cw_only").code, "42704");
    assert_eq!(fails(&db, &mut m, "create table cw_only_t (v cw_only)").code, "42704");
    assert_eq!(fails(&db, &mut m, "create table cw_only_t of cw_only").code, "42704");
    run(&db, &mut w, "create table cw_only_t (id int primary key, v cw_only)");
    run(&db, &mut w, "insert into cw_only_t values (1, row(1, 'a'))");
    assert_eq!(col(run(&db, &mut w, "select (v).y from cw_only_t")), ["a"]);
}

/// Main, with type `ty` of (x int, y text, z text), table `<ty>_t` of a text column `r` read as the type by a policy
/// that shows role `<ty>_r` only the rows whose `y` isn't 'secret', and world `<ty>_w` forked from it then, where `y`
/// is renamed away and `z` renamed to `y` (so `.y` is the third field there): a session of each, as that role. Row 1
/// is `(1,secret,public)`: hidden in main, shown in the fork.
fn policed(ty: &str) -> (Db, Session, Session) {
    let (db, mut m) = (Db::new(), Session::new(MAIN));
    for q in [
        format!("create type {ty} as (x int, y text, z text)"),
        format!("create role {ty}_r"),
        format!("create table {ty}_t (id int primary key, r text)"),
        format!("grant select on {ty}_t to {ty}_r"),
        format!("insert into {ty}_t values (1, '(1,secret,public)')"),
        format!("alter table {ty}_t enable row level security"),
        format!("create policy {ty}_p on {ty}_t using ((r::{ty}).y <> 'secret')"),
        format!("create world {ty}_w"),
    ] {
        run(&db, &mut m, &q);
    }
    let mut w = Session::new(&format!("{ty}_w"));
    run(&db, &mut w, &format!("alter type {ty} rename attribute y to old_y"));
    run(&db, &mut w, &format!("alter type {ty} rename attribute z to y"));
    run(&db, &mut m, &format!("set role {ty}_r"));
    run(&db, &mut w, &format!("set role {ty}_r"));
    (db, m, w)
}

#[test]
fn a_policy_reads_its_worlds_shape_whichever_world_reads_first() {
    let none: Vec<String> = vec![];
    for fork_first in [false, true] {
        let (db, mut m, mut w) = policed("cw_pol");
        let q = "select id from cw_pol_t";
        if fork_first {
            assert_eq!(col(run(&db, &mut w, q)), ["1"]);
        }
        for _ in 0..2 {
            assert_eq!(
                col(run(&db, &mut m, q)),
                none,
                "main shows the row its policy hides (fork first: {fork_first})"
            );
            assert_eq!(col(run(&db, &mut w, q)), ["1"], "fork first: {fork_first}");
        }
        // parsed in one world, run after the other read the table
        let main_q = prepare(&db, &m, q);
        assert_eq!(col(run(&db, &mut w, q)), ["1"]);
        assert_eq!(exec(&db, &mut m, &main_q).map(col), Ok(none.clone()));
        let fork_q = prepare(&db, &w, q);
        assert_eq!(col(run(&db, &mut m, q)), none);
        assert_eq!(exec(&db, &mut w, &fork_q).map(col), Ok(vec!["1".to_string()]));
    }
}

#[test]
fn a_policy_read_in_two_worlds_at_once() {
    let (db, mut m, mut w) = policed("cw_pol2");
    let rounds = 150;
    let q = "select id from cw_pol2_t";
    std::thread::scope(|scope| {
        let db = &db;
        let count = |s: &mut Session| s.run(db, q, &[]).map(|o| o.last().unwrap().rows.len()).ok();
        let main = scope.spawn(move || (0..rounds).filter(|_| count(&mut m) != Some(0)).count());
        let fork = scope.spawn(move || (0..rounds).filter(|_| count(&mut w) != Some(1)).count());
        let wrong = (main.join().unwrap(), fork.join().unwrap());
        assert_eq!(wrong, (0, 0), "wrong answers (main's, the fork's) of {rounds} each");
    });
}

#[test]
fn an_alter_type_committed_from_a_transaction_is_mains_at_once() {
    let (db, mut m) = (Db::new(), Session::new(MAIN));
    run(&db, &mut m, "create type cw_tx as (x int, y text)");
    run(&db, &mut m, "create table cw_tx_t (id int primary key, n int, v cw_tx)");
    run(&db, &mut m, "insert into cw_tx_t values (1, 0, row(1, 'a'))");
    // another session's statements, parsed before the commit and run after (a driver's prepared statements)
    let mut other = Session::new(MAIN);
    assert_eq!(col(run(&db, &mut other, "select v::text from cw_tx_t")), ["(1,a)"]);
    let copy = prepare(&db, &other, "insert into cw_tx_t select id + 100, n, v from cw_tx_t where id = 1");
    let bump = prepare(&db, &other, "update cw_tx_t set n = n + 1, v = v where id = 1");
    let read = prepare(&db, &other, "select v::text from cw_tx_t order by id");
    // the same script goes on after the COMMIT with the new shape
    let added = "begin; alter type cw_tx add attribute z int; commit; \
                 insert into cw_tx_t values (2, 0, row(2, 'b', 2)); select v::text from cw_tx_t order by id";
    assert_eq!(col(run(&db, &mut m, added)), ["(1,a,)", "(2,b,2)"]);
    exec(&db, &mut other, &copy).unwrap();
    exec(&db, &mut other, &bump).unwrap();
    let want = ["(1,a,)", "(2,b,2)", "(1,a,)"].map(String::from).to_vec();
    assert_eq!(exec(&db, &mut other, &read).map(col), Ok(want));
    assert_eq!(col(run(&db, &mut m, "select n from cw_tx_t where id = 1")), ["1"]);
    // dropped so, under a savepoint released
    let dropped = "begin; savepoint s; alter type cw_tx drop attribute z; release savepoint s; commit; \
                   insert into cw_tx_t values (3, 0, row(3, 'c')); select v::text from cw_tx_t order by id";
    assert_eq!(col(run(&db, &mut m, dropped)), ["(1,a)", "(2,b)", "(3,c)", "(1,a)"]);
    exec(&db, &mut other, &bump).unwrap();
    let want = ["(1,a)", "(2,b)", "(3,c)", "(1,a)"].map(String::from).to_vec();
    assert_eq!(exec(&db, &mut other, &read).map(col), Ok(want));
    assert_eq!(col(run(&db, &mut m, "select n from cw_tx_t where id = 1")), ["2"]);
}

#[test]
fn a_merge_or_a_rebase_gives_the_world_its_new_shape_at_once() {
    // merged: the script that merges reads it next, and so do statements of main's other sessions parsed before
    let (db, mut m, mut w) = forked("cw_into", "x int, y text", "row(1, 'a')");
    run(&db, &mut w, "alter type cw_into add attribute z int");
    run(&db, &mut w, "insert into cw_into_t values (2, row(2, 'b', 2))");
    assert_eq!(col(run(&db, &mut m, "select v::text from cw_into_t")), ["(1,a)"]);
    let mut other = Session::new(MAIN);
    let read = prepare(&db, &other, "select v::text from cw_into_t order by id");
    assert_eq!(col(run(&db, &mut m, "merge world cw_into_w; select (v).z from cw_into_t order by id")), ["", "2"]);
    let want = ["(1,a,)", "(2,b,2)"].map(String::from).to_vec();
    assert_eq!(exec(&db, &mut other, &read).map(col), Ok(want));

    // rebased: the fork's statements parsed before the rebase read main's new shape
    let (db, mut m, mut w) = forked("cw_onto", "x int, y text", "row(1, 'a')");
    assert_eq!(col(run(&db, &mut w, "select v::text from cw_onto_t")), ["(1,a)"]);
    let read = prepare(&db, &w, "select v::text from cw_onto_t order by id");
    run(&db, &mut m, "alter type cw_onto add attribute z int");
    run(&db, &mut m, "insert into cw_onto_t values (2, row(2, 'b', 3))");
    run(&db, &mut m, "rebase world cw_onto_w");
    let want = ["(1,a,)", "(2,b,3)"].map(String::from).to_vec();
    assert_eq!(exec(&db, &mut w, &read).map(col), Ok(want));
}

#[test]
fn a_read_committed_transaction_takes_a_committed_alter_type_with_its_next_statement() {
    let (db, mut m) = (Db::new(), Session::new(MAIN));
    run(&db, &mut m, "create type cw_rc as (x int, y text)");
    run(&db, &mut m, "create table cw_rc_t (id int primary key, v cw_rc)");
    run(&db, &mut m, "insert into cw_rc_t values (1, row(1, 'a'))");
    let mut a = Session::new(MAIN);
    run(&db, &mut a, "begin");
    assert_eq!(col(run(&db, &mut a, "select v::text from cw_rc_t")), ["(1,a)"]);
    run(&db, &mut m, "alter type cw_rc add attribute z int");
    run(&db, &mut m, "insert into cw_rc_t values (2, row(2, 'b', 3))");
    // (parsed before the statement brings the transaction up to what main committed)
    assert_eq!(col(run(&db, &mut a, "select (v).z from cw_rc_t order by id")), ["", "3"]);
    assert_eq!(col(run(&db, &mut a, "select row(1, 'a', 3)::cw_rc::text")), ["(1,a,3)"]);
    run(&db, &mut a, "commit");
}

#[test]
fn a_script_goes_on_in_the_world_a_use_world_switched_to() {
    let (db, mut m, mut w) = forked("cw_use", "x int, y text", "row(1, 'a')");
    run(&db, &mut w, "alter type cw_use add attribute z int");
    assert_eq!(col(run(&db, &mut m, "select row(1, 'a')::cw_use::text")), ["(1,a)"]);
    assert_eq!(col(run(&db, &mut m, "use world cw_use_w; select row(1, 'a', 3)::cw_use::text")), ["(1,a,3)"]);
    let mut m = Session::new(MAIN);
    assert_eq!(col(run(&db, &mut m, "use world cw_use_w; select (row(1, 'a', 3)::cw_use).z")), ["3"]);
    assert_eq!(col(run(&db, &mut m, "use world main; select row(1, 'a')::cw_use::text")), ["(1,a)"]);
}

/// A moment of the database's clock, between two of its statements.
fn moment(db: &Db, s: &mut Session) -> Value {
    std::thread::sleep(std::time::Duration::from_millis(5));
    let t = Value::Text(col(run(db, s, "select now()::text")).remove(0));
    std::thread::sleep(std::time::Duration::from_millis(5));
    t
}

#[test]
fn a_restored_world_and_one_read_as_it_was_have_their_shapes_then() {
    let dir = std::env::temp_dir().join(format!("chronos-composite-past-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Db::open(&dir).unwrap();
    let mut m = Session::new(MAIN);
    run(&db, &mut m, "create type cw_back as (x int, y text)");
    run(&db, &mut m, "create table cw_back_t (id int primary key, a int, b int, v cw_back)");
    run(&db, &mut m, "insert into cw_back_t select i, 0, 0, row(i, 'a') from generate_series(1, 20) i");
    run(&db, &mut m, "create world cw_back_w");

    // restored: the fork's statements parsed before read its shape as restored
    let mut w = Session::new("cw_back_w");
    assert_eq!(col(run(&db, &mut w, "select v::text from cw_back_t where id = 1")), ["(1,a)"]);
    run(&db, &mut m, "checkpoint world cw_back_w as 'before'");
    run(&db, &mut w, "alter type cw_back add attribute z int");
    assert_eq!(col(run(&db, &mut w, "select v::text from cw_back_t where id = 1")), ["(1,a,)"]);
    let read = prepare(&db, &w, "select v::text from cw_back_t where id = 1");
    let z = prepare(&db, &w, "select (v).z from cw_back_t where id = 1");
    run(&db, &mut m, "restore world cw_back_w to checkpoint 'before'");
    assert_eq!(exec(&db, &mut w, &read).map(col), Ok(vec!["(1,a)".to_string()]));
    assert_eq!(exec(&db, &mut w, &z).map(col).map_err(|e| e.code), Err("42703"));

    // merged by columns, row by row: each row both changed is the parent's with the fork's column
    run(&db, &mut w, "update cw_back_t set a = id");
    run(&db, &mut m, "update cw_back_t set b = id");
    run(&db, &mut m, "merge world cw_back_w by columns");
    let sums = "select sum(a), sum(b), count(*) filter (where (v).y = 'a') from cw_back_t";
    assert_eq!(run(&db, &mut m, sums), [["210", "210", "20"]]);

    // read as it was: each moment's shape, whichever was read last
    let then = moment(&db, &mut m);
    run(&db, &mut m, "alter type cw_back rename attribute y to yy");
    let later = moment(&db, &mut m);
    run(&db, &mut m, "alter type cw_back add attribute z int");
    for _ in 0..2 {
        let q = "select (v).y from cw_back_t as of $1 where id = 1";
        assert_eq!(col(run_with(&db, &mut m, q, std::slice::from_ref(&then))), ["a"]);
        let q = "select (v).yy from cw_back_t as of $1 where id = 1";
        assert_eq!(col(run_with(&db, &mut m, q, std::slice::from_ref(&later))), ["a"]);
        assert_eq!(col(run(&db, &mut m, "select v::text from cw_back_t where id = 1")), ["(1,a,)"]);
        let q = "select (v).yy from cw_back_t as of $1 where id = 1";
        assert_eq!(m.run(&db, q, std::slice::from_ref(&then)).map(|_| ()).map_err(|e| e.code), Err("42703"));
    }
    drop((m, w));
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Main, with type `ty` of (x int, y text, z text) and table `<ty>_t` whose CHECK reads its text column `r` as the type
/// and whose unique index is on its column `v`'s `y`, holding row 1; world `<ty>_w2` forked from it, which adds row 2,
/// and world `<ty>_w1` forked after, where `y` is renamed away and `z` renamed to `y` (so `.y` is the third field
/// there). Row 2 holds the CHECK only as main reads `y` ('ok', not 'bad'), and its index entry is 'a2' as main reads
/// it ('b2' as `<ty>_w1` does). A session of main and one of `<ty>_w1`.
fn merging(db: &Db, ty: &str) -> (Session, Session) {
    let mut m = Session::new(MAIN);
    for q in [
        format!("create type {ty} as (x int, y text, z text)"),
        format!("create table {ty}_t (id int primary key, r text check ((r::{ty}).y <> 'bad'), v {ty})"),
        format!("create unique index {ty}_y on {ty}_t (((v).y))"),
        format!("insert into {ty}_t values (1, '(1,ok,x)', row(1, 'a', 'b'))"),
        format!("create world {ty}_w2"),
    ] {
        run(db, &mut m, &q);
    }
    let mut w2 = Session::new(&format!("{ty}_w2"));
    run(db, &mut w2, &format!("insert into {ty}_t values (2, '(1,ok,bad)', row(2, 'a2', 'b2'))"));
    run(db, &mut m, &format!("create world {ty}_w1"));
    let mut w1 = Session::new(&format!("{ty}_w1"));
    run(db, &mut w1, &format!("alter type {ty} rename attribute y to old_y"));
    run(db, &mut w1, &format!("alter type {ty} rename attribute z to y"));
    (m, w1)
}

/// Main's rows of `ty`'s table found through its index on `y`, by `y` as main reads it, then as the fork does.
fn by_y(db: &Db, m: &mut Session, ty: &str) -> (Vec<String>, Vec<String>) {
    let q = |y: &str| format!("select id from {ty}_t where (v).y = '{y}' order by id");
    (col(run(db, m, &q("a2"))), col(run(db, m, &q("b2"))))
}

#[test]
fn a_merge_without_a_session_of_main_indexes_and_checks_its_rows_with_mains_shape() {
    let none: Vec<String> = vec![];
    // a statement of main after the fork's ALTER TYPE, then the merge through the API (no session at all)
    let db = Db::new();
    let (mut m, _w1) = merging(&db, "cw_mk1");
    assert_eq!(col(run(&db, &mut m, "select 1")), ["1"]);
    db.merge("cw_mk1_w2", chronos::Resolve::Fail).unwrap();
    assert_eq!(by_y(&db, &mut m, "cw_mk1"), (vec!["2".to_string()], none.clone()));
    // the merge right after the fork's ALTER TYPE
    let db = Db::new();
    let (mut m, _w1) = merging(&db, "cw_mk2");
    db.merge("cw_mk2_w2", chronos::Resolve::Fail).unwrap();
    assert_eq!(by_y(&db, &mut m, "cw_mk2"), (vec!["2".to_string()], none.clone()));
    // MERGE WORLD run by a session of the fork that renamed them
    let db = Db::new();
    let (mut m, mut w1) = merging(&db, "cw_mk3");
    run(&db, &mut w1, "merge world cw_mk3_w2");
    assert_eq!(by_y(&db, &mut m, "cw_mk3"), (vec!["2".to_string()], none.clone()));
    assert_eq!(col(run(&db, &mut w1, "select (v).y from cw_mk3_t order by id")), ["b"]);
    // with main's session in a transaction of its own meanwhile (the merge goes into main, not into it)
    let db = Db::new();
    let (mut m, _w1) = merging(&db, "cw_mk4");
    run(&db, &mut m, "begin");
    run(&db, &mut m, "select (v).y from cw_mk4_t");
    db.merge("cw_mk4_w2", chronos::Resolve::Fail).unwrap();
    run(&db, &mut m, "commit");
    assert_eq!(by_y(&db, &mut m, "cw_mk4"), (vec!["2".to_string()], none));
}

#[test]
fn a_merge_reads_its_own_databases_shape_not_another_databases() {
    let none: Vec<String> = vec![];
    let db = Db::new();
    let (mut m, _w1) = merging(&db, "cw_mk5");
    assert_eq!(col(run(&db, &mut m, "select 1")), ["1"]);
    // another database of the process makes a type of the same name, of other attributes, and reads it last
    let other = Db::new();
    let mut o = Session::new(MAIN);
    run(&other, &mut o, "create type cw_mk5 as (y text, x text, z text)");
    assert_eq!(col(run(&other, &mut o, "select (row('p', 'q', 'r')::cw_mk5).y")), ["p"]);
    db.merge("cw_mk5_w2", chronos::Resolve::Fail).unwrap();
    assert_eq!(by_y(&db, &mut m, "cw_mk5"), (vec!["2".to_string()], none));
    assert_eq!(col(run(&other, &mut o, "select (row('p', 'q', 'r')::cw_mk5).x")), ["q"]);
}

#[test]
fn merges_without_a_session_while_another_world_renames_the_types_attributes() {
    // one thread forks worlds of main, each of a row, and merges them through the API; the other renames an attribute
    // of the type back and forth in a world of its own meanwhile, which made the shape no session's merge went by
    let db = Db::new();
    let (mut m, mut w1) = merging(&db, "cw_mk7");
    let rounds = 40;
    let done = std::sync::atomic::AtomicBool::new(false);
    std::thread::scope(|scope| {
        let (db, done) = (&db, &done);
        let fork = scope.spawn(move || {
            while !done.load(std::sync::atomic::Ordering::Relaxed) {
                run(db, &mut w1, "alter type cw_mk7 rename attribute y to q");
                run(db, &mut w1, "alter type cw_mk7 rename attribute q to y");
            }
        });
        let mut wrong = vec![];
        for i in 0..rounds {
            let (id, w) = (100 + i, format!("cw_mk7_m{i}"));
            run(db, &mut m, &format!("create world {w}"));
            let mut s = Session::new(&w);
            run(db, &mut s, &format!("insert into cw_mk7_t values ({id}, '(1,ok,bad)', row({i}, 'a{i}', 'b{i}'))"));
            if let Err(e) = db.merge(&w, chronos::Resolve::Fail) {
                wrong.push(format!("merge {w}: {e}"));
                continue;
            }
            let q = |y: String| format!("select id from cw_mk7_t where (v).y = '{y}'");
            let found = (col(run(db, &mut m, &q(format!("a{i}")))), col(run(db, &mut m, &q(format!("b{i}")))));
            if found != (vec![id.to_string()], vec![]) {
                wrong.push(format!("{w}: {found:?}"));
            }
        }
        done.store(true, std::sync::atomic::Ordering::Relaxed);
        fork.join().unwrap();
        assert!(wrong.is_empty(), "{} wrong of {rounds}, the first: {:#?}", wrong.len(), &wrong[..wrong.len().min(8)]);
    });
}

#[test]
fn a_merge_that_changes_a_type_indexes_its_rows_with_the_new_shape() {
    // the fork renames `y` away and `z` to `y`, and its index on `y` follows the old attribute; merged by the API
    let (db, mut m) = (Db::new(), Session::new(MAIN));
    run(&db, &mut m, "create type cw_mk6 as (x int, y text, z text)");
    run(&db, &mut m, "create table cw_mk6_t (id int primary key, v cw_mk6)");
    run(&db, &mut m, "create unique index cw_mk6_y on cw_mk6_t (((v).y))");
    run(&db, &mut m, "insert into cw_mk6_t values (1, row(1, 'a', 'b')), (2, row(2, 'a2', 'b2'))");
    run(&db, &mut m, "create world cw_mk6_w");
    let mut w = Session::new("cw_mk6_w");
    run(&db, &mut w, "alter type cw_mk6 rename attribute y to old_y");
    run(&db, &mut w, "alter type cw_mk6 rename attribute z to y");
    run(&db, &mut w, "insert into cw_mk6_t values (3, row(3, 'a3', 'b3'))");
    assert_eq!(col(run(&db, &mut m, "select (v).y from cw_mk6_t where id = 1")), ["a"]);
    db.merge("cw_mk6_w", chronos::Resolve::Fail).unwrap();
    let ids = |m: &mut Session, q: &str| col(run(&db, m, &format!("select id from cw_mk6_t where {q} order by id")));
    assert_eq!(ids(&mut m, "(v).old_y = 'a'"), ["1"]);
    assert_eq!(ids(&mut m, "(v).old_y = 'a3'"), ["3"]);
    assert_eq!(ids(&mut m, "(v).y = 'b2'"), ["2"]);
    assert_eq!(ids(&mut m, "(v).y = 'b3'"), ["3"]);
    assert_eq!(fails(&db, &mut m, "insert into cw_mk6_t values (4, row(4, 'a3', 'c'))").code, "23505");
}
