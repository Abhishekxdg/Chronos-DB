create table if not exists p_t (id int primary key, grp text, v int, u uuid, s smallint, r real, ts timestamptz default now())
insert into p_t values (1,'a',10,'11111111-1111-1111-1111-111111111111',1,1.5,now()),(2,'a',20,null,2,2.5,now()),(3,'b',5,null,3,3.5,now())
select version()
select current_user
select session_user
select current_catalog
select current_database()
select current_schema()
select pg_backend_pid()
select inet_server_addr()
select pg_is_in_recovery()
select pg_postmaster_start_time()
select txid_current()
select pg_get_userbyid(10)
select has_table_privilege('p_t','select')
select has_schema_privilege('public','usage')
select current_setting('server_version_num')
select pg_encoding_to_char(6)
select pg_table_size('p_t')
select pg_relation_size('p_t')
select pg_total_relation_size('p_t')
select pg_size_pretty(1048576)
select pg_database_size('main')
select * from pg_stat_activity
select * from pg_roles
select * from pg_settings
select * from pg_proc limit 1
select * from pg_enum
select * from pg_extension
select * from pg_user
select * from pg_am
select * from pg_stat_user_tables
select * from information_schema.schemata
select * from information_schema.routines
select distinct on (grp) grp, v from p_t order by grp, v desc
select id from p_t order by id fetch first 2 rows only
select id from p_t order by id offset 1 rows fetch next 1 row only
select grp, sum(v) from p_t group by rollup(grp)
select grp, sum(v) from p_t group by grouping sets ((grp), ())
select grp, sum(v) from p_t group by cube(grp)
vacuum
vacuum analyze p_t
analyze
analyze p_t
discard all
comment on table p_t is 'hello'
comment on column p_t.v is 'hello'
lock table p_t in access exclusive mode
begin; lock table p_t; commit
do $$ begin perform 1; end $$
create role app login password 'x'
grant select on p_t to public
revoke all on p_t from public
alter table p_t owner to current_user
reset all
set client_encoding to 'UTF8'
set search_path to public
set application_name = 'x'
set extra_float_digits = 3
set statement_timeout = '5s'
set lock_timeout = '5s'
set idle_in_transaction_session_timeout = 0
show all
show server_version
select 'a'::bytea
select decode('616263','hex')
select encode('abc'::bytea,'hex')
select '10.0.0.1'::inet
select '1 day'::interval
select 1::smallint
select 1.5::real
select 'x'::uuid
select gen_random_uuid()
select uuid_generate_v4()
select md5('a')
select sha256('a'::bytea)
select pg_typeof('a'::uuid)
select pg_typeof(1::smallint)
select pg_typeof(1.5::real)
select regexp_replace('a-b','-','_','g')
select string_to_array('a,b',',')
select array_to_string(array[1,2],',')
select unnest(array[1,2,3])
select to_char(1234.5,'FM999,999.00')
select generate_series(1,3)
select * from generate_series(1,3) g
select jsonb_build_object('a',1)
select jsonb_path_query('{"a":1}','$.a')
select 'a' collate "C"
select 'a' ilike 'A'
select 'abc' similar to 'a%'
select extract(epoch from now())
select date_trunc('day', now())
select age(now(), now())
select make_interval(days=>1)
select overlaps
select (1,2) < (1,3)
select row(1,2)
select array[1,2] @> array[1]
select int4range(1,5)
select '{1,2}'::int[] && '{2}'::int[]
select coalesce(null,1), nullif(1,1), greatest(1,2), least(1,2)
select 1 where exists (select 1)
select cast('1' as int), '1'::int, int4('1')
merge into p_t t using (select 1 id) s on t.id=s.id when matched then update set v=1
insert into p_t values (9,'z',1,null,1,1,now()) on conflict (id) do update set v=excluded.v returning *
update p_t set v=v+1 where id=1 returning id, v
delete from p_t where id=99 returning *
create table p_c (id int generated always as identity primary key, x int, y int generated always as (x*2) stored)
create table p_d (id int, x int, unique (id) deferrable initially deferred)
create table p_e (a int, b int, primary key (a,b), exclude using gist (a with =))
create table p_f (id int primary key, n int references p_t(v))
create table p_g (id int primary key, t int references p_t(id) on update cascade)
create table p_h (id int) partition by range (id)
create table p_i (like p_t including all)
create table p_j (id int) inherits (p_t)
create domain posint as int check (value > 0)
create type pt as (a int, b int)
create index concurrently ix_p on p_t (v)
create index ix_h on p_t using hash (v)
create index ix_g on p_t using gin (grp gin_trgm_ops)
create unique index ix_pp on p_t (v) include (grp)
reindex table p_t
cluster p_t using p_t_pkey
truncate p_t, p_c restart identity cascade
create sequence sq; select nextval('sq'), currval('sq'), lastval()
create schema if not exists sx
create function f1(a int) returns int language sql as 'select a+1'
create procedure pr1() language plpgsql as $$ begin null; end $$
call pr1()
create trigger tr1 after insert on p_t for each row execute function f1()
create publication pub for all tables
create extension if not exists pgcrypto
create extension if not exists "uuid-ossp"
create extension if not exists pg_trgm
copy p_t to stdout with (format binary)
select * from p_t for update skip locked
select * from p_t tablesample bernoulli (50)
select * from p_t where v = any(array[1,2]) 
declare c1 cursor for select * from p_t
prepare q1 (int) as select * from p_t where id=$1
explain (analyze, buffers, format json) select 1
listen chan
select pg_notify('chan','x')
select pg_advisory_lock(1)
select set_config('a.b','1',false)
select * from pg_stat_statements
savepoint a
select txid_current_snapshot()
select pg_current_wal_lsn()
select pg_terminate_backend(1)
select pg_cancel_backend(1)
select pg_sleep(0.01)
select pg_get_viewdef('p_t'::regclass)
select pg_get_indexdef(1)
select format_type(23, null)
select current_timestamp, localtimestamp, current_time, statement_timestamp(), transaction_timestamp(), clock_timestamp()
select num_nonnulls(1,null), num_nulls(1,null)
select v, lag(v) over w, first_value(v) over (order by id rows between unbounded preceding and current row) from p_t window w as (order by id)
select v, sum(v) over (order by id groups between 1 preceding and current row) from p_t
select v, sum(v) over (order by id range between 1 preceding and current row exclude current row) from p_t
select count(distinct v) over () from p_t
select percentile_cont(0.5) within group (order by v) from p_t
select mode() within group (order by v) from p_t
select string_agg(grp, ',' order by grp) from p_t
select count(*) filter (where v>5) from p_t
select bool_and(v>1), every(v>1) from p_t
select * from p_t t1 join lateral (select * from p_t t2 where t2.id=t1.id limit 1) x on true
select * from p_t natural join p_t p2
select json_object_agg(id, v) from p_t
select xmlelement(name a)
select (select array_agg(id) from p_t)
select array(select id from p_t)
select ARRAY[1,2,3][2]
select (array[1,2,3])[1:2]
select 1 = 1 is true
select 'a' || 1
select 5 / 2, 5 % 2, 5 ^ 2, |/ 4, @ -1, 5 & 3, 5 | 3, 5 # 3, ~5, 1 << 2
