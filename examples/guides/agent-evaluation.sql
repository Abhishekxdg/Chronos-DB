-- Grade an agent by diffing its world against the expected state (docs/guides/agent-evaluation.md).
-- Against a fresh database, with no --token (so psql needs no password on loopback):
--   CHRONOS_PROTECT_MAIN=off chronos serve evaldb   (this setup writes main directly)
--   psql -v ON_ERROR_STOP=1 -f examples/guides/agent-evaluation.sql postgres://127.0.0.1:5433/main
\set ON_ERROR_STOP 1
-- how this script connected, to come back after acting as an agent (step 6)
\set admin_uri 'postgres://' :USER '@' :HOST ':' :PORT '/main'

-- The task's starting state, in main
create table tickets (
  id       int primary key,
  title    text not null,
  status   text not null,
  priority text not null,
  assignee text
);
insert into tickets values
  (1, 'Checkout button does nothing', 'open', 'high', null),
  (2, 'Checkout button broken on Safari', 'open', 'high', null),
  (3, 'Typo on pricing page', 'open', 'low', null),
  (4, 'Export to CSV times out', 'open', 'high', 'linus'),
  (5, 'Dark mode colors', 'open', 'low', null);

-- 1. The answer key: the state a correct run leaves behind
create world expected with (task = 'triage-42', role = 'answer key');
switch world expected;
update tickets set status = 'closed' where id = 2;
update tickets set assignee = 'ada' where status = 'open' and priority = 'high';
insert into tickets values (6, 'Outage postmortem', 'open', 'high', 'ada');
switch world main;

-- 2. A world per run
create world run_1 with (task = 'triage-42', model = 'model-a');
create world run_2 with (task = 'triage-42', model = 'model-b');
checkpoint world run_2 as 'start';

-- 3. The runs (here, SQL standing in for two agents)
switch world run_1;
update tickets set assignee = 'ada' where status = 'open' and priority = 'high';
update tickets set status = 'closed' where id = 2;
insert into tickets values (6, 'Outage postmortem', 'open', 'high', 'ada');

switch world run_2;
update tickets set status = 'closed' where id = 1;
update tickets set assignee = 'ada' where status = 'open' and priority = 'high' and assignee is null;
switch world main;

-- 4. Grade: every row returned is a difference; 0 rows is a pass
diff world expected to run_1;
diff world expected to run_2;
diff world run_2 to expected as sql;

-- 5. Reset a run and go again
restore world run_2 to checkpoint 'start';
diff world run_2;

-- 6. A real agent, logged in as itself, then undone
create agent evalbot \gset agent_
\set agent_uri 'postgres://evalbot:' :agent_token '@' :HOST ':' :PORT '/main'
\connect :agent_uri
create world run_3 with (task = 'triage-42', model = 'model-c');
switch world run_3;
update tickets set status = 'closed' where id in (2, 3);
update tickets set assignee = 'ada' where status = 'open' and priority = 'high';
\connect :admin_uri
diff world expected to run_3;
show audit for agent evalbot;
switch world run_3;
undo agent evalbot since '-10 minutes';
switch world main;
diff world run_3;

-- 7. Clean up
drop world run_1;
drop world run_2;
drop world run_3;
drop world expected;
show worlds;
