//! The database a folder is attached to, as one interface: what kind it is, what it can do, and the four things a
//! merge and a refresh ask of it. Postgres's side is the functions of the parent module; MySQL's and MariaDB's are
//! `mysql`'s. A new source is a new implementation of `Source`, and one more arm where a saved record names it
//! (`from_saved`): nothing else in the parent module asks which source it has.

use super::*;

/// Which database a folder is attached to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceKind {
    Postgres,
    MySql,
    MariaDb,
    Mongo,
}

impl SourceKind {
    /// As `/v1/attached` says it (`kind`).
    pub fn as_str(self) -> &'static str {
        match self {
            SourceKind::Postgres => "postgres",
            SourceKind::MySql => "mysql",
            SourceKind::MariaDb => "mariadb",
            SourceKind::Mongo => "mongodb",
        }
    }
}

/// What a source can do besides the write-back and the refresh every source has. A caller (the gateway) reads these
/// rather than learning a source's limits from refusals.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capabilities {
    /// `serve --follow`: main takes the source's changes as they commit
    pub follow: bool,
    /// dry runs and rehearsals of a merge against the source
    pub dry_run: bool,
    /// a world's reads checked against the source (`check_reads`, `CHECK READS`)
    pub check_reads: bool,
    /// writes made as an end user's role (`pg_role`)
    pub act_as: bool,
    /// `attach --lazy-tenant`: a tenant's rows copied at its first fork
    pub lazy_tenant: bool,
    /// merge checks run in the source too, and the rules of `writeback.json`
    pub source_checks: bool,
}

impl Capabilities {
    const ALL: Capabilities = Capabilities {
        follow: true,
        dry_run: true,
        check_reads: true,
        act_as: true,
        lazy_tenant: true,
        source_checks: true,
    };
    const WRITE_BACK_ONLY: Capabilities = Capabilities {
        follow: false,
        dry_run: false,
        check_reads: false,
        act_as: false,
        lazy_tenant: false,
        source_checks: false,
    };

    pub fn to_json(self) -> Json {
        json!({
            "follow": self.follow, "dry_run": self.dry_run, "check_reads": self.check_reads, "act_as": self.act_as,
            "lazy_tenant": self.lazy_tenant, "source_checks": self.source_checks,
        })
    }
}

/// What the parent module asks of the source of an attached folder.
pub(super) trait Source: Send + Sync + std::fmt::Debug {
    fn kind(&self) -> SourceKind;

    fn capabilities(&self) -> Capabilities;

    /// The write-back of a merge's `rows` (see `write_back` in the parent module, which works the rows out).
    #[allow(clippy::too_many_arguments)]
    fn write_back(
        &self,
        core: &Core,
        att: &Attached,
        world: &str,
        world_id: &str,
        rows: &[Row],
        reads: Option<&Reads>,
        checks: &[crate::checks::MergeCheck],
        who: Option<&ActAs>,
        rules: &Rules,
    ) -> Result<Option<Held>>;

    /// Whether the write-back that recorded `merge_id` committed in the source (a merge in doubt after a crash).
    fn committed(&self, merge_id: &str) -> std::result::Result<bool, String>;

    /// A dry run's read of the source (see `dry_run` in the parent module): each of `rows` compared with the source's
    /// row, and `reads` checked, into `live`; nothing is written or locked.
    fn dry_run(
        &self,
        core: &Core,
        att: &Attached,
        rows: &[Row],
        reads: Option<&Reads>,
        live: &mut crate::LiveCheck,
    ) -> std::result::Result<(), String>;

    /// What a world read (`reads`), checked against the source as it is now, outside any merge (`CHECK READS`).
    fn check_reads(&self, core: &Core, att: &Attached, reads: &Reads) -> std::result::Result<Vec<String>, String>;

    /// Main takes the rows a merge in doubt touched (`pending`, the saved record of it) as the source has them now,
    /// adding to `said` what else it took.
    fn take_touched(
        &self,
        core: &Core,
        att: &Attached,
        pending: &Json,
        said: &mut String,
    ) -> std::result::Result<(), String>;

    /// Main takes the source's rows as they are now, of the tables `only` names if given.
    fn refresh(&self, core: &Core, att: &Attached, only: Option<&[usize]>) -> std::result::Result<Refreshed, String>;
}

/// The source a saved record names: `source` absent is Postgres (records made before there was another), `"mysql"`
/// MySQL, or MariaDB when `flavor` says so (records made before `flavor` say MySQL for both; they behave the same).
pub(super) fn from_saved(source: Option<&str>, flavor: Option<&str>) -> Box<dyn Source> {
    match source {
        Some("mysql") => Box::new(MySql { maria: flavor == Some("mariadb") }),
        Some("mongo") => Box::new(Mongo),
        _ => Box::new(Postgres),
    }
}

#[derive(Debug)]
pub(super) struct Postgres;

impl Source for Postgres {
    fn kind(&self) -> SourceKind {
        SourceKind::Postgres
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::ALL
    }

    fn write_back(
        &self,
        core: &Core,
        att: &Attached,
        world: &str,
        world_id: &str,
        rows: &[Row],
        reads: Option<&Reads>,
        checks: &[crate::checks::MergeCheck],
        who: Option<&ActAs>,
        rules: &Rules,
    ) -> Result<Option<Held>> {
        super::pg_write_back(core, att, world, world_id, rows, reads, checks, who, rules)
    }

    fn committed(&self, merge_id: &str) -> std::result::Result<bool, String> {
        super::committed(merge_id)
    }

    fn check_reads(&self, core: &Core, att: &Attached, reads: &Reads) -> std::result::Result<Vec<String>, String> {
        super::pg_check_reads(core, att, reads)
    }

    fn dry_run(
        &self,
        core: &Core,
        att: &Attached,
        rows: &[Row],
        reads: Option<&Reads>,
        live: &mut crate::LiveCheck,
    ) -> std::result::Result<(), String> {
        super::pg_dry_run(core, att, rows, reads, live)
    }

    fn take_touched(
        &self,
        core: &Core,
        att: &Attached,
        pending: &Json,
        said: &mut String,
    ) -> std::result::Result<(), String> {
        super::pg_take_touched(core, att, pending, said)
    }

    fn refresh(&self, core: &Core, att: &Attached, only: Option<&[usize]>) -> std::result::Result<Refreshed, String> {
        super::pg_refresh(core, att, only)
    }
}

/// MySQL 8.0 or later, or MariaDB 10.5 or later (`maria`): one path for both (see `mysql`).
#[derive(Debug)]
pub(super) struct MySql {
    pub(super) maria: bool,
}

impl Source for MySql {
    fn kind(&self) -> SourceKind {
        match self.maria {
            true => SourceKind::MariaDb,
            false => SourceKind::MySql,
        }
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities { check_reads: true, dry_run: true, ..Capabilities::WRITE_BACK_ONLY }
    }

    fn write_back(
        &self,
        core: &Core,
        att: &Attached,
        world: &str,
        world_id: &str,
        rows: &[Row],
        reads: Option<&Reads>,
        checks: &[crate::checks::MergeCheck],
        who: Option<&ActAs>,
        rules: &Rules,
    ) -> Result<Option<Held>> {
        mysql::write_back(core, att, world, world_id, rows, reads, checks, who, rules)
    }

    fn committed(&self, merge_id: &str) -> std::result::Result<bool, String> {
        mysql::committed(merge_id)
    }

    fn check_reads(&self, core: &Core, att: &Attached, reads: &Reads) -> std::result::Result<Vec<String>, String> {
        mysql::check_reads(core, att, reads)
    }

    fn dry_run(
        &self,
        core: &Core,
        att: &Attached,
        rows: &[Row],
        reads: Option<&Reads>,
        live: &mut crate::LiveCheck,
    ) -> std::result::Result<(), String> {
        mysql::dry_run(core, att, rows, reads, live)
    }

    fn take_touched(
        &self,
        core: &Core,
        att: &Attached,
        pending: &Json,
        _said: &mut String,
    ) -> std::result::Result<(), String> {
        let touched = pending["rows"].as_object().cloned().unwrap_or_default();
        let rows = mysql::reread(att, &touched)?;
        store(core, att, &rows)
    }

    fn refresh(&self, core: &Core, att: &Attached, only: Option<&[usize]>) -> std::result::Result<Refreshed, String> {
        mysql::refresh(core, att, only)
    }
}

/// MongoDB 5.0 or later, a replica set (see `mongo`). In a build without the `mongo` feature every use says so.
#[derive(Debug)]
pub(super) struct Mongo;

#[cfg(not(feature = "mongo"))]
const NO_MONGO: &str = "this folder is attached to MongoDB, and this chronos was built without it: use a build with \
                        MongoDB (cargo build --features mongo)";

impl Source for Mongo {
    fn kind(&self) -> SourceKind {
        SourceKind::Mongo
    }

    fn capabilities(&self) -> Capabilities {
        let built = cfg!(feature = "mongo");
        Capabilities { check_reads: built, dry_run: built, ..Capabilities::WRITE_BACK_ONLY }
    }

    #[allow(unused_variables)]
    fn write_back(
        &self,
        core: &Core,
        att: &Attached,
        world: &str,
        world_id: &str,
        rows: &[Row],
        reads: Option<&Reads>,
        checks: &[crate::checks::MergeCheck],
        who: Option<&ActAs>,
        rules: &Rules,
    ) -> Result<Option<Held>> {
        #[cfg(feature = "mongo")]
        return mongo::write_back(core, att, world, world_id, rows, reads, checks, who, rules);
        #[cfg(not(feature = "mongo"))]
        Err(refused(format!("{NO_MONGO}; nothing was merged")))
    }

    #[allow(unused_variables)]
    fn committed(&self, merge_id: &str) -> std::result::Result<bool, String> {
        #[cfg(feature = "mongo")]
        return mongo::committed(merge_id);
        #[cfg(not(feature = "mongo"))]
        Err(NO_MONGO.into())
    }

    #[allow(unused_variables)]
    fn check_reads(&self, core: &Core, att: &Attached, reads: &Reads) -> std::result::Result<Vec<String>, String> {
        #[cfg(feature = "mongo")]
        return mongo::check_reads(core, att, reads);
        #[cfg(not(feature = "mongo"))]
        Err(NO_MONGO.into())
    }

    #[allow(unused_variables)]
    fn dry_run(
        &self,
        core: &Core,
        att: &Attached,
        rows: &[Row],
        reads: Option<&Reads>,
        live: &mut crate::LiveCheck,
    ) -> std::result::Result<(), String> {
        #[cfg(feature = "mongo")]
        return mongo::dry_run(core, att, rows, reads, live);
        #[cfg(not(feature = "mongo"))]
        Err(NO_MONGO.into())
    }

    #[allow(unused_variables)]
    fn take_touched(
        &self,
        core: &Core,
        att: &Attached,
        pending: &Json,
        _said: &mut String,
    ) -> std::result::Result<(), String> {
        #[cfg(feature = "mongo")]
        {
            let touched = pending["rows"].as_object().cloned().unwrap_or_default();
            let rows = mongo::reread(att, &touched)?;
            store(core, att, &rows)
        }
        #[cfg(not(feature = "mongo"))]
        Err(NO_MONGO.into())
    }

    #[allow(unused_variables)]
    fn refresh(&self, core: &Core, att: &Attached, only: Option<&[usize]>) -> std::result::Result<Refreshed, String> {
        #[cfg(feature = "mongo")]
        return mongo::refresh(core, att, only);
        #[cfg(not(feature = "mongo"))]
        Err(NO_MONGO.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_saved_record_names_its_source() {
        // records made before MySQL say nothing, and ones made before `flavor` say MySQL for MariaDB too
        assert_eq!(from_saved(None, None).kind(), SourceKind::Postgres);
        assert_eq!(from_saved(Some("mysql"), None).kind(), SourceKind::MySql);
        assert_eq!(from_saved(Some("mysql"), Some("mariadb")).kind(), SourceKind::MariaDb);
        assert!(from_saved(None, None).capabilities().follow);
        let my = from_saved(Some("mysql"), None).capabilities();
        assert!(my.dry_run && my.check_reads && !my.follow && !my.act_as);
        assert_eq!(SourceKind::MariaDb.as_str(), "mariadb");
    }
}
