//! SHOW AGENT name PRIVILEGES [IN WORLD w]: what an agent may actually do with each table of a world, once its
//! capabilities, the privileges of a role of its name (both must allow), row-level security, its tenant, masked
//! columns and its merge policy are put together. A row per table, and one per column that differs from its table (a
//! masked column, a column's own grant, a column its policy sends to review).
//!
//! It reads what decides, as the checks themselves read it (`Agent::may`, `acl`, `rls::limits`, `rls::reach`,
//! `mask::of`, `MergePolicy`), so what it says is what the agent's statements meet. It doesn't run anything as the
//! agent.

use super::roles::Who;
use super::*;

/// The columns of SHOW AGENT ... PRIVILEGES.
pub(super) const COLUMNS: [(&str, Type); 6] = [
    ("table", Type::Text),
    ("column", Type::Text),
    ("read", Type::Text),
    ("write", Type::Text),
    ("merge", Type::Text),
    ("why", Type::Text),
];

/// What agent `name` may do with each table of world `world`.
pub(super) fn show(db: &Db, name: &str, world: &str) -> Result<Vec<Vec<Value>>> {
    let a = db.agent(name)?;
    let v = db.view_tx(world)?;
    let policy = match &a.policy {
        Some(p) => Some(db.policy(p)?),
        None => None,
    };
    // a role of its name: its privileges count too, both must allow (see `roles::who_now`)
    let reg = roles::registry(&db.core)?;
    let who: Option<Who> = match (db.core.has_privileges(), reg.named(name)) {
        (true, Some(r)) => Some(Who::for_role(&reg, r.oid)).filter(|w| !w.superuser),
        _ => None,
    };
    let unmask = a.may(crate::Can::Unmask);
    // held to a tenant: its column, and the condition in words (`"org_id" = 42`)
    let tenant = a
        .tenant
        .as_ref()
        .and_then(|t| t["column"].as_str())
        .map(|c| (c.to_string(), a.tenant_sql().unwrap_or_default()));
    let can = |c: crate::Can| a.may(c);
    // where it may change rows: an agent keeping to a merge policy changes main only by merging
    let mut places: Vec<&str> = vec![];
    if can(crate::Can::Write) {
        places.push("any world but main");
    } else if can(crate::Can::WriteOwn) {
        places.push("its own worlds");
    }
    if can(crate::Can::WriteMain) && policy.is_none() {
        places.push("main");
    }
    let mut out = vec![];
    for t in schemas(&v)? {
        if crate::reserved_table(&t.name) || t.matview.is_some() {
            continue;
        }
        let mut why: Vec<String> = vec![];
        if a.disabled {
            why.push("the agent is disabled".into());
        }
        // its role's privileges on the table (None: no role, or no privileges anywhere: its capabilities decide)
        let obj = match &who {
            Some(_) => acl::rel(&v, &t.name)?,
            None => None,
        };
        let has = |p: u32| match (&who, &obj) {
            (Some(w), Some(o)) => o.has(w, p),
            _ => true,
        };
        let role = who.as_ref().map(|w| w.name.clone()).unwrap_or_default();
        // how the table stands to its tenant (see `rls::reach`): held to it, or shared by every tenant
        let reach = match &tenant {
            Some((col, _)) => rls::reach(&v, &t.name, col)?,
            None => None,
        };
        let shared = reach == Some(rls::Reach::Shared);
        // read
        let limited = match &who {
            Some(w) => rls::limits(&v, &t.name, Some(w)).unwrap_or(true),
            None => false,
        };
        let read = if !can(crate::Can::Read) {
            why.push("no 'read' capability".into());
            "no".to_string()
        } else if !has(acl::SELECT) {
            why.push(format!("role {role} has no SELECT on it (columns it was granted are listed)"));
            "no".into()
        } else if let (Some(r), Some((col, cond))) = (reach.as_ref().filter(|_| !shared), &tenant) {
            why.push(format!("tenant {cond}: {}", r.words(col)));
            if limited {
                why.push(format!("row-level security: only the rows role {role}'s policies allow"));
            }
            "its tenant's rows".into()
        } else if limited {
            why.push(format!("row-level security: only the rows role {role}'s policies allow"));
            "rows its policies allow".into()
        } else {
            "yes".into()
        };
        // write
        const VERBS: [(&str, u32); 3] = [("insert", acl::INSERT), ("update", acl::UPDATE), ("delete", acl::DELETE)];
        let verbs: Vec<&str> = VERBS.into_iter().filter(|(_, p)| has(*p)).map(|(n, _)| n).collect();
        let writes = !(a.disabled || places.is_empty() || shared);
        let write_of = |verbs: &[&str]| format!("{} in {}", verbs.join(", "), places.join(" and "));
        let write = if !writes {
            if places.is_empty() {
                why.push("no 'write_own', 'write' or 'write_main' capability".into());
            } else if let (true, Some((col, cond))) = (shared, &tenant) {
                why.push(format!("tenant {cond}: {}", rls::Reach::Shared.words(col)));
            }
            "no".to_string()
        } else if verbs.is_empty() {
            why.push(format!("role {role} has no INSERT, UPDATE or DELETE on it (columns it was granted are listed)"));
            "no".into()
        } else {
            if verbs.len() < 3 {
                why.push(format!("role {role} may {} only", verbs.join(", ")));
            }
            write_of(&verbs)
        };
        if can(crate::Can::WriteMain) && policy.is_some() {
            why.push("it keeps to a merge policy, so it changes main only by merging".into());
        }
        // merge
        let merge = merge_of(&a, policy.as_ref(), &t.name, None, &mut why);
        out.push(vec![
            Value::Text(t.name.clone()),
            Value::Null,
            Value::Text(read.clone()),
            Value::Text(write.clone()),
            Value::Text(merge.clone()),
            Value::Text(why.join("; ")),
        ]);
        // the columns that differ from the table
        let masked = mask::of(&v, &t.name)?;
        for c in t.cols.iter().filter(|c| !c.hidden) {
            let mut cwhy: Vec<String> = vec![];
            let mut cread = read.clone();
            let mut differs = false;
            if let (Some(w), Some(o)) = (&who, &obj)
                && read == "no"
                && can(crate::Can::Read)
                && acl::column_has(&v, &t.name, &c.name, w, o.owner, acl::SELECT)?
            {
                cread = "yes".into();
                cwhy.push(format!("role {role} was granted SELECT on the column"));
                differs = true;
            }
            // INSERT and UPDATE may be granted on a column alone (DELETE is the table's)
            let mut cwrite = write.clone();
            if let (Some(w), Some(o)) = (&who, &obj)
                && writes
            {
                let mut granted: Vec<&str> = vec![];
                for (n, p) in VERBS.into_iter().take(2).filter(|(n, _)| !verbs.contains(n)) {
                    if acl::column_has(&v, &t.name, &c.name, w, o.owner, p)? {
                        granted.push(n);
                    }
                }
                if !granted.is_empty() {
                    let all: Vec<&str> =
                        VERBS.iter().map(|(n, _)| *n).filter(|n| verbs.contains(n) || granted.contains(n)).collect();
                    cwrite = write_of(&all);
                    let names: Vec<String> = granted.iter().map(|n| n.to_uppercase()).collect();
                    cwhy.push(format!("role {role} was granted {} on the column", names.join(" and ")));
                    differs = true;
                }
            }
            if let Some((_, m)) = masked.iter().find(|(n, _)| *n == c.name)
                && cread != "no"
            {
                differs = true;
                if unmask {
                    cwhy.push(format!("masked ({}) for agents without 'unmask'; this one has it", mask::mode_name(*m)));
                } else {
                    cread = format!("masked ({})", mask::mode_name(*m));
                    cwhy.push("masked column: it reads a mask of each value, and may not filter on the value".into());
                    // a unique value's insert fails when the value is there: the failure says what the mask hides
                    let unique = t.key_cols().iter().any(|&k| t.cols.get(k).is_some_and(|k| k.id == c.id))
                        || t.uniques.iter().any(|u| u.cols.contains(&c.id));
                    if unique && cwrite.contains("insert") {
                        cwrite = format!("{cwrite} (an insert reveals whether a value is there)");
                        cwhy.push(
                            "unique and masked: an insert that fails on it says the value is there; give it no INSERT, \
                             or put the table in its merge policy's review_tables"
                                .into(),
                        );
                    }
                    // a trigger that reads it as it is may not fire for the agent's UPDATE or DELETE
                    let column = format!("column \"{}\"", c.name);
                    let changes = cwrite.contains("update") || cwrite.contains("delete");
                    for x in t.triggers.iter().filter(|x| {
                        changes
                            && x.enabled
                            && x.events.iter().any(|e| matches!(e, functions::Event::Update | functions::Event::Delete))
                    }) {
                        let Some(what) = functions::trigger_reads_masked(&v, x, &masked)? else { continue };
                        if what == column || !what.starts_with("column ") {
                            cwrite = format!("{cwrite} (an update or delete that fires trigger {} is refused)", x.name);
                            cwhy.push(format!(
                                "trigger {} reads {what} as it is: this agent's UPDATE and DELETE that fire it are refused",
                                x.name
                            ));
                        }
                    }
                }
            }
            let cmerge = merge_of(&a, policy.as_ref(), &t.name, Some(&c.name), &mut cwhy);
            differs |= cmerge != merge;
            if differs {
                out.push(vec![
                    Value::Text(t.name.clone()),
                    Value::Text(c.name.clone()),
                    Value::Text(cread),
                    Value::Text(cwrite),
                    Value::Text(cmerge),
                    Value::Text(cwhy.join("; ")),
                ]);
            }
        }
    }
    Ok(out)
}

/// What becomes of the agent's merges of changes to `table` (to its `column`, if given): with the reason, if any,
/// added to `why`.
fn merge_of(
    a: &crate::Agent,
    policy: Option<&crate::MergePolicy>,
    table: &str,
    column: Option<&str>,
    why: &mut Vec<String>,
) -> String {
    use crate::Can::*;
    if a.disabled {
        return "no".into();
    }
    if !a.may(Merge) && !a.may(MergeOwn) {
        if column.is_none() {
            why.push("no 'merge_own' or 'merge': a person merges its worlds".into());
        }
        return "a person merges".into();
    }
    if !a.may(Merge) && !a.may(WriteMain) {
        if column.is_none() {
            why.push("'merge_own' without 'write_main': its worlds merge into worlds it may change, not main".into());
        }
        return "not into main".into();
    }
    let Some(p) = policy else {
        if column.is_none() {
            why.push("no merge policy: its rights alone decide".into());
        }
        return "on its own".into();
    };
    let shown = rel_part(table);
    let named = |list: &[String]| list.iter().any(|t| t == table || t == shown);
    if !p.tables.is_empty() && !named(&p.tables) {
        if column.is_none() {
            why.push(format!("merge policy {} lets merges change only {}", p.name, crate::agents::listed(&p.tables)));
        }
        return "refused".into();
    }
    if named(&p.review_tables) {
        if column.is_none() {
            why.push(format!("merge policy {}: every change to it waits for a person (review_tables)", p.name));
        }
        return "waits for a person".into();
    }
    if let Some(c) = column
        && p.review_columns.iter().any(|(t, col)| (t == table || t == shown) && col == c)
    {
        why.push(format!("merge policy {}: every change to the column waits for a person (review_columns)", p.name));
        return "waits for a person".into();
    }
    if column.is_none() {
        let mut limits = vec![];
        if let Some(n) = p.max_rows {
            limits.push(format!("max_rows {n}"));
        }
        if let Some(n) = p.max_deletes {
            limits.push(format!("max_deletes {n}"));
        }
        if let Some(n) = p.confirm_deletes_over {
            limits.push(format!("confirm_deletes_over {n}"));
        }
        why.push(match limits.is_empty() {
            true => format!("merge policy {}", p.name),
            false => format!("merge policy {}: past {} it waits for a person", p.name, limits.join(", ")),
        });
    }
    format!("on its own within policy {}", p.name)
}
