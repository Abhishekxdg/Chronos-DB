-- A small CRM for the five-minute trial (see README.md): three reps, their deals, leads and tasks,
-- and the rules an agent's work is held to.

create table reps (id text primary key, name text not null);
create table deals (
  id int primary key,
  owner text not null references reps,
  account text not null,
  stage text not null,                -- lead, proposal, won, lost
  amount int not null
);
create table leads (id int primary key, owner text references reps, company text, last_contact date);
create table tasks (id serial primary key, deal int references deals, owner text, note text);

insert into reps values ('ana', 'Ana Ruiz'), ('ben', 'Ben Okafor'), ('cho', 'Cho Min');
insert into deals values
  (1, 'ana', 'Acme',      'proposal', 12000),
  (2, 'ana', 'Globex',    'proposal',  8000),
  (3, 'ana', 'Initech',   'proposal', 15000),
  (4, 'ben', 'Umbrella',  'proposal',  9000),
  (5, 'ben', 'Hooli',     'proposal',  1000),
  (6, 'cho', 'Stark',     'won',      40000);
insert into leads values
  (1, 'ana', 'Wayne',     date '2024-02-01'),
  (2, 'ben', 'Tyrell',    date '2024-05-12'),
  (3, 'cho', 'Cyberdyne', date '2024-11-30'),
  (4, 'ana', 'Soylent',   date '2026-09-01'),
  (5, 'ben', 'Massive',   date '2026-10-01');

-- Every merge into main must leave no deal with a negative amount.
create merge check no_negative_amounts on tables (deals) as
  select id, account, amount from deals where amount < 0;

-- What the agent's merges are held to: no deletes without a person, and a merge whose reads
-- changed since its world was forked waits for a person too.
create merge policy careful with (max_deletes = 0, check_reads = true);

-- `main` changes only by merging a world: a mistaken DELETE there is refused, not an incident.
alter system set protect_main = on;
