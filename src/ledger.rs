//! The merge ledger: one row in main for every merge into it, `_sonos_ledger/<seq>`, written in the merge's own log
//! record (`disk::Op::Applied`), in the same frame and fsync as the rows it merged: a crash leaves the merge with
//! its ledger row, or neither. Each row holds the hash of the one before it (`prev_hash`) and its own (`hash`):
//! blake3 over the previous hash and the row without its hash, so a row changed, dropped or put out of order breaks
//! the chain at that row (`VERIFY LEDGER`, see `verify`).
//!
//! A merge given an operation id (`MergeOptions::operation`) also writes `_sonos_ops/<id>` with what it was asked
//! (a digest of the request) and what it did: asked again with the same id and the same request it returns that
//! result, merging nothing; with the same id and another request it is refused.
//!
//! Both are the database's own rows of main: no world's merge, undo or restore changes them (see `reserved`).

use crate::{Error, Result};
use serde_json::{Map, Value as Json, json};

pub(crate) const PREFIX: &str = "_sonos_ledger/";
pub(crate) const OPS: &str = "_sonos_ops/";
/// The hash before the first row.
pub(crate) const GENESIS: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// A ledger row's key: its sequence number, zero-padded so rows sort in order.
pub(crate) fn key(seq: u64) -> String {
    format!("{PREFIX}{seq:020}")
}

/// An operation row's key.
pub(crate) fn op_key(id: &str) -> String {
    format!("{OPS}{id}")
}

/// A row of the ledger or the operations: never merged, undone or restored, only appended.
pub(crate) fn reserved(key: &str) -> bool {
    key.starts_with(PREFIX) || key.starts_with(OPS)
}

/// The hash of `entry` (a row without its `hash`) following `prev`.
pub(crate) fn hash(prev: &str, entry: &Map<String, Json>) -> String {
    let mut h = blake3::Hasher::new();
    h.update(prev.as_bytes());
    h.update(b"\n");
    h.update(Json::Object(entry.clone()).to_string().as_bytes());
    h.finalize().to_hex().to_string()
}

/// What a merge's ledger row says beyond its rows: who merged, under what, and why it went through.
pub(crate) struct Context {
    pub(crate) actor: String,
    /// the merge policy that judged it (the acting agent's, or the one that held it for review), and the blake3 of
    /// its row as main had it then
    pub(crate) policy: Option<(String, Option<String>)>,
    /// `merged`, or `approved` for a merge a policy held for review that was then let through
    pub(crate) verdict: &'static str,
    /// the rules it was held for, when approved
    pub(crate) reasons: Vec<String>,
    /// the caller's operation id, with the digest of the request (see `request`)
    pub(crate) operation: Option<(String, String)>,
}

/// The digest of a merge request: what an operation id is bound to.
pub(crate) fn request(world: &str, o: &crate::MergeOptions) -> String {
    // each pick with how it settles its row: the same keys settled another way are another request
    let mut picks: Vec<(&String, Json)> = o
        .picks
        .iter()
        .map(|(k, p)| {
            let how = match p {
                crate::Pick::Ours => json!("ours"),
                crate::Pick::Theirs => json!("theirs"),
                crate::Pick::Row(row) => json!({ "row": row }),
            };
            (k, how)
        })
        .collect();
    picks.sort_by(|a, b| a.0.cmp(b.0));
    let r = json!({
        "world": world,
        "how": format!("{:?}", o.how),
        "expected": o.expected,
        "columns": o.columns,
        "only_tables": o.only_tables,
        "only_keys": o.only_keys,
        "into": o.into,
        "picks": picks,
        "confirm_deletes": o.confirm_deletes,
    });
    blake3::hash(r.to_string().as_bytes()).to_hex().to_string()
}

/// For a merge with an operation id: the result stored for it, when the same request was done before (Some), or
/// None when the id is new. Refused when the id was used for another request.
pub(crate) fn repeated(stored: Option<&str>, id: &str, request: &str) -> Result<Option<usize>> {
    let Some(stored) = stored else { return Ok(None) };
    let row: Json = serde_json::from_str(stored).unwrap_or_default();
    if row["request"].as_str() != Some(request) {
        return Err(Error::Constraint {
            code: "23505",
            message: format!(
                "operation {id} was already used for another merge (merge {}); give this one its own id; nothing was merged",
                row["merge_id"].as_str().unwrap_or("?")
            ),
        });
    }
    Ok(Some(row["rows"].as_u64().unwrap_or(0) as usize))
}

/// Rows to write (key, value; none: delete) and the ledger's new head (sequence, hash).
pub(crate) type LedgerRows = (Vec<(String, Option<String>)>, (u64, String));

/// The rows a merge into main adds to the ledger, after `head` (the last row's sequence and hash; none: the first):
/// its ledger row, and its operation row if it has an id.
#[allow(clippy::too_many_arguments)]
pub(crate) fn rows(
    head: Option<(u64, String)>,
    ctx: &Context,
    (source, source_id): (&str, &str),
    target_id: &str,
    (rows, deletes, applied): (usize, usize, usize),
    digest: &[u8; 32],
    root_before: Option<String>,
    at: u64,
) -> LedgerRows {
    let (prev_seq, prev) = head.unwrap_or((0, GENESIS.into()));
    let seq = prev_seq + 1;
    let merge_id = format!("{target_id}/{seq}");
    let mut e = Map::new();
    e.insert("seq".into(), json!(seq));
    e.insert("merge_id".into(), json!(merge_id));
    e.insert("source".into(), json!(source));
    e.insert("source_id".into(), json!(source_id));
    e.insert("target".into(), json!(crate::MAIN));
    e.insert("target_id".into(), json!(target_id));
    e.insert("actor".into(), json!(ctx.actor));
    e.insert("policy".into(), json!(ctx.policy.as_ref().map(|p| &p.0)));
    e.insert("policy_hash".into(), json!(ctx.policy.as_ref().and_then(|p| p.1.as_ref())));
    e.insert("verdict".into(), json!(ctx.verdict));
    e.insert("reasons".into(), json!(ctx.reasons));
    e.insert("rows".into(), json!(rows));
    e.insert("deletes".into(), json!(deletes));
    e.insert("digest".into(), json!(blake3::Hash::from(*digest).to_hex().to_string()));
    e.insert("root_before".into(), json!(root_before));
    e.insert("root_after".into(), Json::Null); // a tree's root is known only once a checkpoint folds it
    e.insert("at".into(), json!(at));
    e.insert("operation".into(), json!(ctx.operation.as_ref().map(|o| &o.0)));
    e.insert("prev_hash".into(), json!(prev));
    let h = hash(&prev, &e);
    e.insert("hash".into(), json!(h));
    let mut out = vec![(key(seq), Some(Json::Object(e).to_string()))];
    if let Some((id, request)) = &ctx.operation {
        let row = json!({ "operation": id, "request": request, "merge_id": merge_id, "seq": seq, "rows": applied });
        out.push((op_key(id), Some(row.to_string())));
    }
    (out, (seq, h))
}

/// The last row of a ledger, from its rows in key order: (sequence, hash).
pub(crate) fn head_of(last: Option<(String, crate::Val)>) -> Option<(u64, String)> {
    let (k, v) = last?;
    let seq = k.strip_prefix(PREFIX)?.parse().ok()?;
    let row: Json = serde_json::from_str(&v).ok()?;
    Some((seq, row["hash"].as_str()?.to_string()))
}

/// The ledger checked row by row, in order: (sequence, whether it holds, what's wrong). A row holds when its hash is
/// its own, it follows the row before by sequence and by `prev_hash`.
pub(crate) fn verify(rows: impl Iterator<Item = (String, crate::Val)>) -> Vec<(i64, bool, String)> {
    let (mut out, mut prev, mut want) = (vec![], GENESIS.to_string(), 1u64);
    for (k, v) in rows {
        let seq = k.strip_prefix(PREFIX).and_then(|s| s.parse::<u64>().ok());
        let row: Option<Map<String, Json>> = serde_json::from_str(&v).ok();
        let (Some(seq), Some(mut row)) = (seq, row) else {
            out.push((-1, false, format!("{k} is not a ledger row")));
            continue;
        };
        let mut wrong = vec![];
        if seq != want {
            wrong.push(format!("follows row {} (a row is missing or out of order)", want - 1));
        }
        if row.get("seq").and_then(Json::as_u64) != Some(seq) {
            wrong.push("its seq isn't its key's".into());
        }
        let stored = row.remove("hash").and_then(|h| h.as_str().map(String::from)).unwrap_or_default();
        let linked = row.get("prev_hash").and_then(Json::as_str).unwrap_or_default().to_string();
        if linked != prev {
            wrong.push("its prev_hash isn't the hash of the row before".into());
        }
        let own = hash(&linked, &row);
        if own != stored {
            wrong.push("its hash doesn't match its contents (changed since it was written)".into());
        }
        out.push((seq as i64, wrong.is_empty(), wrong.join("; ")));
        prev = stored;
        want = seq + 1;
    }
    out
}
