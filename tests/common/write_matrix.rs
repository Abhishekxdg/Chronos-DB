//! The cases of Gate S's cross-tenant write matrix (CHRONOS-PLAN-2026-11-17-r3.md section 5): every way a tenant-42
//! agent can send a change, as (what, statements in order). Shared by `tenant_isolation::cross_tenant_write_matrix`
//! (in-process) and `attach::cross_tenant_write_matrix_postgres` (an attached Postgres), so a case added reaches both.

pub const CASES: &[(&str, &[&str])] = &[
    (
        "direct write to main",
        &["update leads set name = 'pwned' where org_id = 7", "delete from leads where org_id = 7"],
    ),
    (
        "world then merge",
        &["create world w; use world w; update leads set org_id = 42 where id = 3", "use world main; merge world w"],
    ),
    (
        "partial merge",
        &[
            "create world w; use world w; delete from leads where id = 3",
            "use world main; merge world w only tables (leads)",
        ],
    ),
    (
        "merge keys",
        &[
            "create world w; use world w; update leads set name = 'pwned'",
            "use world main; merge world w only keys ('leads/3')",
        ],
    ),
    (
        "merge into another world, then main",
        &[
            "create world a; create world b",
            "use world b; update leads set name = 'pwned' where id = 3",
            "use world main; merge world b into a",
            "merge world a",
        ],
    ),
    (
        "rebase then merge",
        &["create world w; use world w; delete from leads", "use world main; rebase world w", "merge world w"],
    ),
    (
        "simulate keep merge",
        &[
            "simulate 1 worlds as z run $$alter table leads alter column org_id type integer using 42$$ score $$select 1$$ keep 1",
            "merge world z_0",
        ],
    ),
    (
        "simulate script names main",
        &[
            "simulate 1 worlds as y run $$use world main; delete from leads$$ score $$select 1$$ keep 1",
            "merge world y_0",
        ],
    ),
    (
        "ddl then merge",
        &[
            "create world w; use world w; alter table leads alter column org_id type integer using 42",
            "use world main; merge world w",
        ],
    ),
    ("truncate then merge", &["create world w; use world w; truncate leads", "use world main; merge world w"]),
    ("restore", &["restore world main to '-1 second'"]),
    (
        "undo",
        &[
            "create world w; use world w; update leads set owner = 'x' where id = 1",
            "use world main; merge world w",
            "undo merge of world w",
        ],
    ),
    (
        "outbox",
        &[
            "create world w; use world w; notify on merge jobs, 'delete leads 3'",
            "use world main; merge world w",
            "claim outbox",
        ],
    ),
];
