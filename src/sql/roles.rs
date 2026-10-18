//! Roles: CREATE ROLE, membership (GRANT role TO role), SET ROLE and SET SESSION AUTHORIZATION,
//! and who a statement runs as (`Who`), which the privilege checks (`acl`) ask.
//!
//! Roles are the database's, as Postgres's are the cluster's: one row on main
//! (`_sonos_roles/all`) holds every role made with CREATE ROLE and every membership, so every
//! world sees the same roles. Statements that change it write main directly, as CREATE AGENT
//! does: a ROLLBACK doesn't undo them, and UNDO and RESTORE never put the row back.
//!
//! Besides those, there are Postgres's built-in roles: the bootstrap superuser `chronos` (oid 10),
//! which owns whatever no role was made to own, and the predefined `pg_*` roles. A session whose
//! login name is no role (every session before roles existed, and agents) is a superuser acting
//! as `chronos`, as it always was; pg_roles shows its name too, as a superuser (oid 16384).
//!
//! Membership is Postgres 16's: each grant of a role to a member has a grantor and three options.
//! INHERIT: the member has the role's privileges. SET: the member may SET ROLE to it. ADMIN: the
//! member may grant it on (and, with CREATEROLE, alter or drop it).

use super::*;
use std::cell::RefCell;

/// The row every role and membership is kept in, on main.
pub(crate) const REGISTRY: &str = "_sonos_roles/all";
/// The bootstrap superuser: owns what no role was made to own.
pub(crate) const BOOTSTRAP: i64 = 10;
/// The grantee an ACL item gives to everyone.
pub(super) const PUBLIC: i64 = 0;
/// `pg_database_owner`: implicitly, the database's owner (here, `chronos`).
pub(super) const DB_OWNER: i64 = 6171;
/// `pg_signal_backend`: may cancel and terminate other roles' connections (not superusers').
pub(super) const SIGNAL_BACKEND: i64 = 4200;
/// `pg_checkpoint`: may run CHECKPOINT.
pub(super) const CHECKPOINT: i64 = 4544;
/// `pg_read_server_files`: may read the server's files (COPY FROM a file, file_fdw's `filename`).
pub(super) const READ_SERVER_FILES: i64 = 4569;
/// `pg_write_server_files`: may write the server's files (COPY TO a file).
pub(super) const WRITE_SERVER_FILES: i64 = 4570;
/// What pg_roles calls a login name that is no role.
const LOGIN_OID: i64 = 16384;
/// The first oid a role made here gets.
const FIRST_OID: i64 = 16385;

/// Postgres 16's predefined roles: (oid, name).
const PREDEFINED: [(i64, &str); 14] = [
    (3373, "pg_monitor"),
    (3374, "pg_read_all_settings"),
    (3375, "pg_read_all_stats"),
    (3377, "pg_stat_scan_tables"),
    (4200, "pg_signal_backend"),
    (4544, "pg_checkpoint"),
    (4550, "pg_use_reserved_connections"),
    (4569, "pg_read_server_files"),
    (4570, "pg_write_server_files"),
    (4571, "pg_execute_server_program"),
    (6171, "pg_database_owner"),
    (6181, "pg_read_all_data"),
    (6182, "pg_write_all_data"),
    (6304, "pg_create_subscription"),
];

/// A role, as pg_authid has it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Role {
    pub oid: i64,
    pub name: String,
    pub superuser: bool,
    pub inherit: bool,
    pub createrole: bool,
    pub createdb: bool,
    pub login: bool,
    pub replication: bool,
    pub bypassrls: bool,
    /// -1: no limit
    pub connlimit: i64,
    /// a SCRAM-SHA-256 verifier or an md5 hash (see `scram`), never the password
    pub password: Option<String>,
    /// µs since 1970 (`time::TS_INF` for infinity)
    pub valid_until: Option<i64>,
    /// ALTER ROLE ... SET's settings, `name=value`
    pub config: Vec<String>,
}

impl Role {
    fn new(oid: i64, name: &str) -> Role {
        Role {
            oid,
            name: name.into(),
            superuser: false,
            inherit: true,
            createrole: false,
            createdb: false,
            login: false,
            replication: false,
            bypassrls: false,
            connlimit: -1,
            password: None,
            valid_until: None,
            config: vec![],
        }
    }

    fn bootstrap() -> Role {
        Role {
            superuser: true,
            createrole: true,
            createdb: true,
            login: true,
            replication: true,
            bypassrls: true,
            ..Role::new(BOOTSTRAP, "chronos")
        }
    }

    fn to_json(&self) -> Json {
        serde_json::json!({
            "oid": self.oid, "name": self.name, "superuser": self.superuser, "inherit": self.inherit,
            "createrole": self.createrole, "createdb": self.createdb, "login": self.login,
            "replication": self.replication, "bypassrls": self.bypassrls, "connlimit": self.connlimit,
            "password": self.password, "valid_until": self.valid_until, "config": self.config,
        })
    }

    fn from_json(j: &Json) -> Option<Role> {
        let b = |k: &str| j[k].as_bool().unwrap_or(false);
        Some(Role {
            oid: j["oid"].as_i64()?,
            name: j["name"].as_str()?.into(),
            superuser: b("superuser"),
            inherit: j["inherit"].as_bool().unwrap_or(true),
            createrole: b("createrole"),
            createdb: b("createdb"),
            login: b("login"),
            replication: b("replication"),
            bypassrls: b("bypassrls"),
            connlimit: j["connlimit"].as_i64().unwrap_or(-1),
            password: j["password"].as_str().map(String::from),
            valid_until: j["valid_until"].as_i64(),
            config: j["config"].as_array().into_iter().flatten().filter_map(|c| c.as_str().map(String::from)).collect(),
        })
    }
}

/// A grant of a role to a member (pg_auth_members).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Member {
    pub oid: i64,
    pub role: i64,
    pub member: i64,
    pub grantor: i64,
    pub admin: bool,
    pub inherit: bool,
    pub set: bool,
}

/// Every role made with CREATE ROLE, and every membership.
#[derive(Clone, Debug, Default)]
pub(crate) struct Registry {
    pub roles: Vec<Role>,
    pub members: Vec<Member>,
    /// the next oid to give
    next: i64,
}

/// How far `reached` follows memberships.
#[derive(Clone, Copy, PartialEq)]
enum Through {
    /// every grant (MEMBER)
    Any,
    /// grants with INHERIT: whose privileges the member has (USAGE)
    Inherit,
    /// grants with SET: what the member may SET ROLE to
    Set,
}

impl Registry {
    fn parse(raw: &str) -> Registry {
        let j: Json = serde_json::from_str(raw).unwrap_or_default();
        let roles = j["roles"].as_array().into_iter().flatten().filter_map(Role::from_json).collect();
        let members = j["members"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| {
                Some(Member {
                    oid: m["oid"].as_i64()?,
                    role: m["role"].as_i64()?,
                    member: m["member"].as_i64()?,
                    grantor: m["grantor"].as_i64()?,
                    admin: m["admin"].as_bool().unwrap_or(false),
                    inherit: m["inherit"].as_bool().unwrap_or(true),
                    set: m["set"].as_bool().unwrap_or(true),
                })
            })
            .collect();
        Registry { roles, members, next: j["next"].as_i64().unwrap_or(FIRST_OID) }
    }

    fn to_json(&self) -> String {
        let members: Vec<Json> = self
            .members
            .iter()
            .map(|m| {
                serde_json::json!({"oid": m.oid, "role": m.role, "member": m.member, "grantor": m.grantor,
                    "admin": m.admin, "inherit": m.inherit, "set": m.set})
            })
            .collect();
        serde_json::json!({
            "next": self.next,
            "roles": self.roles.iter().map(Role::to_json).collect::<Vec<_>>(),
            "members": members,
        })
        .to_string()
    }

    fn new_oid(&mut self) -> i64 {
        self.next = self.next.max(FIRST_OID);
        self.next += 1;
        self.next - 1
    }

    /// The built-in roles, then the ones made here.
    pub(crate) fn all(&self) -> Vec<Role> {
        let mut out = vec![Role::bootstrap()];
        out.extend(PREDEFINED.iter().map(|(o, n)| Role::new(*o, n)));
        out.extend(self.roles.iter().cloned());
        out
    }

    pub(crate) fn named(&self, name: &str) -> Option<Role> {
        if name == "chronos" {
            return Some(Role::bootstrap());
        }
        if let Some((o, n)) = PREDEFINED.iter().find(|(_, n)| *n == name) {
            return Some(Role::new(*o, n));
        }
        self.roles.iter().find(|r| r.name == name).cloned()
    }

    pub(crate) fn by_oid(&self, oid: i64) -> Option<Role> {
        if oid == BOOTSTRAP {
            return Some(Role::bootstrap());
        }
        if let Some((o, n)) = PREDEFINED.iter().find(|(o, _)| *o == oid) {
            return Some(Role::new(*o, n));
        }
        self.roles.iter().find(|r| r.oid == oid).cloned()
    }

    /// A role's name as an ACL item or a catalog shows it: the oid's digits for one that's gone.
    pub(crate) fn name_of(&self, oid: i64) -> String {
        self.by_oid(oid).map_or_else(|| oid.to_string(), |r| r.name)
    }

    fn superuser(&self, oid: i64) -> bool {
        self.by_oid(oid).is_some_and(|r| r.superuser)
    }

    /// The roles `member` reaches through grants of the kind `through`: itself first.
    fn reached(&self, member: i64, through: Through) -> Vec<i64> {
        let mut out = vec![member];
        let mut i = 0;
        while i < out.len() {
            let m = out[i];
            for g in self.members.iter().filter(|g| g.member == m) {
                let follows = match through {
                    Through::Any => true,
                    Through::Inherit => g.inherit,
                    Through::Set => g.set,
                };
                if follows && !out.contains(&g.role) {
                    out.push(g.role);
                }
            }
            // the database's owner is pg_database_owner, implicitly
            if m == BOOTSTRAP && !out.contains(&DB_OWNER) {
                out.push(DB_OWNER);
            }
            i += 1;
        }
        out
    }

    /// Whether `member` has the privileges of `role` (pg_has_role's USAGE).
    pub(crate) fn has_privs_of(&self, member: i64, role: i64) -> bool {
        member == role || self.superuser(member) || self.reached(member, Through::Inherit).contains(&role)
    }

    /// Whether `member` is a member of `role`, through any grants (MEMBER).
    pub(crate) fn is_member_of(&self, member: i64, role: i64) -> bool {
        member == role || self.superuser(member) || self.reached(member, Through::Any).contains(&role)
    }

    /// Whether `member` may SET ROLE to `role`.
    pub(crate) fn can_set(&self, member: i64, role: i64) -> bool {
        member == role || self.superuser(member) || self.reached(member, Through::Set).contains(&role)
    }

    /// Whether `member` holds ADMIN OPTION on `role` through itself or any role it is a member of, through any grant
    /// (as Postgres 16's is_admin_of_role follows ROLERECURSE_MEMBERS): what may change or drop a role, and
    /// pg_has_role's WITH ADMIN OPTION; a superuser does.
    pub(crate) fn is_admin_of(&self, member: i64, role: i64) -> bool {
        if self.superuser(member) {
            return true;
        }
        member != role
            && self
                .reached(member, Through::Any)
                .into_iter()
                .any(|r| self.members.iter().any(|g| g.member == r && g.role == role && g.admin))
    }

    /// The role through which `member` holds ADMIN OPTION on `role` (itself or one whose privileges it
    /// has: what a GRANT is made as), if any; a superuser's is the bootstrap superuser.
    pub(crate) fn admin_of(&self, member: i64, role: i64) -> Option<i64> {
        if self.superuser(member) {
            return Some(BOOTSTRAP);
        }
        if member == role {
            return None;
        }
        self.reached(member, Through::Inherit)
            .into_iter()
            .find(|r| self.members.iter().any(|g| g.member == *r && g.role == role && g.admin))
    }

    /// Every role whose privileges `oid` has, itself first.
    pub(crate) fn privs_of(&self, oid: i64) -> Vec<i64> {
        self.reached(oid, Through::Inherit)
    }
}

/// Registries read, by the raw row they were parsed from, per database.
static PARSED: LazyLock<Mutex<HashMap<usize, Parsed>>> = LazyLock::new(Default::default);

/// A registry and the raw row it was parsed from.
type Parsed = (Arc<str>, Arc<Registry>);

/// The roles as they are now on main.
pub(crate) fn registry(core: &crate::Core) -> Result<Arc<Registry>> {
    if !core.has_privileges() {
        return Ok(Arc::default());
    }
    let raw = core.view(crate::MAIN)?.get(REGISTRY)?;
    let Some(raw) = raw else { return Ok(Arc::default()) };
    let key = core as *const crate::Core as usize;
    let mut cache = PARSED.lock().unwrap_or_else(|p| p.into_inner());
    if let Some((r, reg)) = cache.get(&key)
        && (Arc::ptr_eq(r, &raw) || **r == *raw)
    {
        return Ok(reg.clone());
    }
    let reg = Arc::new(Registry::parse(&raw));
    cache.insert(key, (raw, reg.clone()));
    Ok(reg)
}

/// Changes the roles: `f` on the registry as it is on main, written back unless it fails.
fn change<T>(db: &Db, f: impl FnOnce(&mut Registry) -> Result<T>) -> Result<T> {
    let (_, out) = db.batch_with::<T, SqlError>(crate::MAIN, |v| {
        let mut reg = match v.get(REGISTRY)? {
            Some(raw) => Registry::parse(&raw),
            None => Registry::default(),
        };
        let out = f(&mut reg)?;
        Ok((vec![(REGISTRY.to_string(), Some(reg.to_json()))], out))
    })?;
    Ok(out)
}

// ---------------------------------------------------------------- who a statement runs as

/// Who a statement runs as: the role privileges are checked for, and the roles whose
/// privileges it has.
#[derive(Clone, Debug)]
pub(crate) struct Who {
    /// the role (BOOTSTRAP for a login that is no role)
    pub oid: i64,
    /// current_user
    pub name: String,
    pub superuser: bool,
    /// itself, and every role its memberships with INHERIT reach
    pub privs_of: Vec<i64>,
    pub reg: Arc<Registry>,
}

impl Who {
    /// A login that is no role: a superuser, acting as the bootstrap superuser.
    pub(crate) fn boot(name: &str) -> Who {
        Who { oid: BOOTSTRAP, name: name.into(), superuser: true, privs_of: vec![BOOTSTRAP], reg: Arc::default() }
    }

    /// The identity of role `oid` (the bootstrap superuser, PUBLIC, or any other), as the
    /// `has_*_privilege` functions ask about one.
    pub(crate) fn for_role(reg: &Arc<Registry>, oid: i64) -> Who {
        match oid {
            BOOTSTRAP => Who { reg: reg.clone(), ..Who::boot("chronos") },
            PUBLIC => {
                Who { oid: PUBLIC, name: "public".into(), superuser: false, privs_of: vec![PUBLIC], reg: reg.clone() }
            }
            o => Who::of(reg, o),
        }
    }

    fn of(reg: &Arc<Registry>, oid: i64) -> Who {
        match reg.by_oid(oid) {
            Some(r) => {
                Who { oid, name: r.name.clone(), superuser: r.superuser, privs_of: reg.privs_of(oid), reg: reg.clone() }
            }
            // a role dropped while a session was it: no privileges at all
            None => Who { oid, name: oid.to_string(), superuser: false, privs_of: vec![oid], reg: reg.clone() },
        }
    }

    /// Whether it has the privileges of `role`.
    pub(crate) fn has_privs_of(&self, role: i64) -> bool {
        self.superuser || self.privs_of.contains(&role)
    }

    /// The same role's identity as another role (`oid`): what a view's or a SECURITY DEFINER
    /// function's owner runs as.
    pub(crate) fn as_role(&self, oid: i64) -> Who {
        if oid == BOOTSTRAP {
            return Who { reg: self.reg.clone(), ..Who::boot("chronos") };
        }
        Who::of(&self.reg, oid)
    }
}

thread_local! {
    /// Who the running statement runs as (see `Session::execute`); None: no session, a superuser.
    static WHO: RefCell<Option<Arc<Who>>> = const { RefCell::new(None) };
}

/// Who the running statement runs as, if it isn't a superuser: what the privilege checks check.
pub(crate) fn checked() -> Option<Arc<Who>> {
    WHO.with_borrow(|w| w.clone().filter(|w| !w.superuser))
}

/// Who the running statement runs as (None outside a session's statement).
pub(crate) fn current() -> Option<Arc<Who>> {
    WHO.with_borrow(Clone::clone)
}

/// Whether the running statement may reach the server's files as predefined role `role`
/// (`READ_SERVER_FILES`, `WRITE_SERVER_FILES`) lets one, as Postgres's has_privs_of_role checks it: a
/// superuser, or a role with that role's privileges. Never an agent's session (the guest's too:
/// it logged in with a token, not as a role), and never outside a session's statement.
pub(crate) fn may_use_server_files(role: i64) -> bool {
    !super::acl::agents() && current().is_some_and(|w| w.has_privs_of(role))
}

/// Runs `f` as `who` (a SECURITY DEFINER function's owner), then goes back.
pub(crate) fn acting<T>(who: Option<Arc<Who>>, f: impl FnOnce() -> T) -> T {
    let before = WHO.replace(who);
    struct Back(Option<Option<Arc<Who>>>);
    impl Drop for Back {
        fn drop(&mut self) {
            if let Some(w) = self.0.take() {
                WHO.set(w);
            }
        }
    }
    let _back = Back(Some(before));
    f()
}

/// Sets who the thread's statement runs as, returning who it was (for `restore`).
pub(crate) fn enter(who: Option<Arc<Who>>) -> Option<Arc<Who>> {
    WHO.replace(who)
}

pub(crate) fn restore(who: Option<Arc<Who>>) {
    WHO.set(who);
}

/// A session's identity: the role its login name is (None: a login that is no role), and what
/// SET SESSION AUTHORIZATION and SET ROLE made it (None: not set).
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Ident {
    pub login: Option<i64>,
    pub auth: Option<i64>,
    pub role: Option<i64>,
}

/// Who a session is: (current_user's identity, session_user's identity); `login` is the name it
/// logged in with (an agent's, or `chronos`).
pub(crate) fn who(core: &crate::Core, ident: &Ident, agent: Option<&str>, login: &str) -> Result<(Who, Who)> {
    let (current, session) = who_shared(core, ident, agent, login)?;
    Ok((Arc::unwrap_or_clone(current), Arc::unwrap_or_clone(session)))
}

/// `who`, shared: the same answers a thread's last statement had, while nothing they rest on changed.
pub(crate) fn who_shared(
    core: &crate::Core,
    ident: &Ident,
    agent: Option<&str>,
    login: &str,
) -> Result<(Arc<Who>, Arc<Who>)> {
    // (the last answer on this thread, while the roles' row is the one it was worked out from: a connection asks
    // before each of its statements)
    let raw = match core.has_privileges() {
        true => core.view(crate::MAIN)?.get(REGISTRY)?,
        false => None,
    };
    let at = core as *const crate::Core as usize;
    let kept = WHO_KEPT.with_borrow(|k| {
        let k = k.as_ref()?;
        let same_roles = match (&k.raw, &raw) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b) || **a == **b,
            (a, b) => a.is_none() && b.is_none(),
        };
        (k.core == at && same_roles && k.ident == *ident && k.agent.as_deref() == agent && k.login == login)
            .then(|| k.who.clone())
    });
    if let Some(w) = kept {
        return Ok(w);
    }
    let (current, session) = who_now(core, ident, agent, login)?;
    let out = (Arc::new(current), Arc::new(session));
    WHO_KEPT.set(Some(WhoKept {
        core: at,
        raw,
        ident: ident.clone(),
        agent: agent.map(String::from),
        login: login.into(),
        who: out.clone(),
    }));
    Ok(out)
}

/// `who`'s last answer on a thread, and what it was worked out from.
struct WhoKept {
    core: usize,
    raw: Option<Arc<str>>,
    ident: Ident,
    agent: Option<String>,
    login: String,
    who: (Arc<Who>, Arc<Who>),
}

thread_local! {
    static WHO_KEPT: RefCell<Option<WhoKept>> = const { RefCell::new(None) };
}

/// `who`, worked out from the roles as they are now.
fn who_now(core: &crate::Core, ident: &Ident, agent: Option<&str>, login: &str) -> Result<(Who, Who)> {
    let reg = registry(core)?;
    let agent_role = match (ident.login, agent) {
        (None, Some(a)) => reg.named(a).map(|r| r.oid),
        _ => None,
    };
    let session = match ident.auth.or(ident.login).or(agent_role) {
        Some(oid) => Who::of(&reg, oid),
        None => Who { reg: reg.clone(), ..Who::boot(login) },
    };
    let current = match ident.role {
        Some(oid) => Who::of(&reg, oid),
        None => session.clone(),
    };
    Ok((current, session))
}

/// Whether the session's authenticated login is a superuser (SET SESSION AUTHORIZATION's rule).
fn authenticated_superuser(reg: &Registry, ident: &Ident) -> bool {
    ident.login.is_none_or(|o| reg.superuser(o))
}

/// Checks that a login as `name` may go on: None if it isn't a role (a superuser, as before roles),
/// else its oid, unless it may not log in (28000) or its password expired (28P01).
pub(crate) fn login(core: &crate::Core, name: &str) -> Result<Option<i64>> {
    let reg = registry(core)?;
    // (a predefined role is one too, and logs in as none: never as the superuser a name that is no role is)
    let Some(r) = reg.named(name).filter(|r| r.oid != BOOTSTRAP) else { return Ok(None) };
    if !r.login {
        return err("28000", format!("role \"{name}\" is not permitted to log in"));
    }
    if r.valid_until.is_some_and(|t| t < now_us()) {
        return err("28P01", format!("password authentication failed for user \"{name}\""));
    }
    Ok(Some(r.oid))
}

/// The password role `name` keeps, for a login to prove it knows, and whether it is past its
/// VALID UNTIL: None if `name` is no role (as `login` has it: a predefined role is one); a None
/// password if the role has none.
pub(crate) fn password(core: &crate::Core, name: &str) -> Result<Option<(Option<String>, bool)>> {
    let reg = registry(core)?;
    let role = reg.named(name).filter(|r| r.oid != BOOTSTRAP);
    Ok(role.map(|r| (r.password.clone(), r.valid_until.is_some_and(|t| t < now_us()))))
}

// ---------------------------------------------------------------- statements

/// How a statement names a role: by name, or as the session sees itself.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Spec {
    Name(String),
    CurrentUser,
    SessionUser,
    /// PUBLIC, where a grantee may be it
    Public,
}

impl Spec {
    /// The role it names, or 42704.
    pub(crate) fn resolve(&self, reg: &Registry) -> Result<Role> {
        let (current, session) = names();
        let name = match self {
            Spec::Name(n) => n.clone(),
            Spec::CurrentUser => current.clone(),
            Spec::SessionUser => session.clone(),
            Spec::Public => return err("42704", "role \"public\" does not exist"),
        };
        match reg.named(&name) {
            Some(r) => Ok(r),
            // a login that is no role is the bootstrap superuser under another name, and so is
            // `postgres`, the name Postgres's dumps give it
            None if matches!(self, Spec::CurrentUser | Spec::SessionUser) => Ok(Role::bootstrap()),
            None if name == current || name == session || name == "postgres" => Ok(Role::bootstrap()),
            None => err("42704", format!("role \"{name}\" does not exist")),
        }
    }

    /// Its oid: PUBLIC's is 0.
    pub(crate) fn oid(&self, reg: &Registry) -> Result<i64> {
        match self {
            Spec::Public => Ok(PUBLIC),
            s => s.resolve(reg).map(|r| r.oid),
        }
    }
}

/// current_user and session_user of the running statement.
fn names() -> (String, String) {
    let current = current().map_or_else(|| "chronos".to_string(), |w| w.name.clone());
    let session = STOP.with_borrow(|s| s.as_ref().and_then(|s| s.ctx.as_ref().map(|c| c.session_user.clone())));
    (current.clone(), session.unwrap_or(current))
}

/// CREATE ROLE's and ALTER ROLE's options; None: not given.
#[derive(Clone, Debug, Default)]
pub(crate) struct Opts {
    superuser: Option<bool>,
    createdb: Option<bool>,
    createrole: Option<bool>,
    inherit: Option<bool>,
    login: Option<bool>,
    replication: Option<bool>,
    bypassrls: Option<bool>,
    connlimit: Option<i64>,
    /// Some(None): PASSWORD NULL
    password: Option<Option<String>>,
    valid_until: Option<String>,
    in_role: Vec<String>,
    role: Vec<String>,
    admin: Vec<String>,
    /// SYSID n was written: a noise word now, which CREATE ROLE says so of
    sysid: bool,
}

/// A statement about roles.
#[derive(Clone, Debug)]
pub(crate) enum RoleStmt {
    /// CREATE ROLE | USER | GROUP: USER logs in unless told otherwise
    Create {
        name: String,
        user: bool,
        opts: Opts,
    },
    Alter {
        name: Spec,
        opts: Opts,
    },
    Rename {
        name: String,
        to: String,
    },
    /// ALTER ROLE name SET x = v | RESET x | RESET ALL (`setting`: None for RESET ALL)
    Config {
        name: Spec,
        setting: Option<(String, Option<String>)>,
    },
    Drop {
        names: Vec<String>,
        if_exists: bool,
    },
    /// ALTER GROUP group ADD | DROP USER members: a GRANT or REVOKE of the group, by a role with ADMIN OPTION on it
    AlterGroup {
        group: String,
        members: Vec<Spec>,
        add: bool,
    },
    /// GRANT roles TO members [WITH ADMIN | INHERIT | SET option] [GRANTED BY]
    Grant {
        roles: Vec<String>,
        members: Vec<Spec>,
        admin: Option<bool>,
        inherit: Option<bool>,
        set: Option<bool>,
        by: Option<Spec>,
    },
    /// REVOKE [ADMIN | INHERIT | SET OPTION FOR] roles FROM members [GRANTED BY] [CASCADE]
    Revoke {
        roles: Vec<String>,
        members: Vec<Spec>,
        option: Option<&'static str>,
        by: Option<Spec>,
        cascade: bool,
    },
}

impl Parser {
    /// A role's name where Postgres reads a RoleSpec: a name, CURRENT_USER, CURRENT_ROLE or SESSION_USER
    /// (and PUBLIC, where `public` allows it).
    pub(super) fn role_spec(&mut self, public: bool) -> Result<Spec> {
        if self.kw("current_user") || self.kw("current_role") {
            return Ok(Spec::CurrentUser);
        }
        if self.kw("session_user") {
            return Ok(Spec::SessionUser);
        }
        if public && self.kw("public") {
            return Ok(Spec::Public);
        }
        Ok(Spec::Name(self.role_name()?))
    }

    /// A role's own name (not CURRENT_USER and the like): what CREATE ROLE and IN ROLE take.
    pub(super) fn role_name(&mut self) -> Result<String> {
        match self.peek() {
            Some(Tok::Word(w)) if matches!(w.as_str(), "public" | "none") => {
                let w = w.clone();
                self.i += 1;
                err("42939", format!("role name \"{w}\" is reserved"))
            }
            Some(Tok::Word(w)) if matches!(w.as_str(), "current_user" | "current_role" | "session_user" | "user") => {
                self.fail("expected a role name")
            }
            Some(Tok::Word(w)) => {
                let w = w.clone();
                self.i += 1;
                Ok(w)
            }
            _ => self.name(),
        }
    }

    fn role_names(&mut self) -> Result<Vec<String>> {
        let mut out = vec![self.role_name()?];
        while self.sym(",") {
            out.push(self.role_name()?);
        }
        Ok(out)
    }

    fn role_specs(&mut self, public: bool) -> Result<Vec<Spec>> {
        let mut out = vec![];
        loop {
            self.kw("group");
            out.push(self.role_spec(public)?);
            if !self.sym(",") {
                return Ok(out);
            }
        }
    }

    /// CREATE ROLE's (and ALTER ROLE's, `create` false) options, after the name and an optional WITH.
    fn role_opts(&mut self, create: bool) -> Result<Opts> {
        self.kw("with");
        let mut o = Opts::default();
        let twice = || err(SYNTAX, "conflicting or redundant options");
        macro_rules! flag {
            ($f:ident, $v:expr) => {{
                if o.$f.is_some() {
                    return twice();
                }
                o.$f = Some($v);
            }};
        }
        while let Some(Tok::Word(w)) = self.peek().cloned() {
            self.i += 1;
            match w.as_str() {
                "superuser" => flag!(superuser, true),
                "nosuperuser" => flag!(superuser, false),
                "createdb" => flag!(createdb, true),
                "nocreatedb" => flag!(createdb, false),
                "createrole" => flag!(createrole, true),
                "nocreaterole" => flag!(createrole, false),
                "inherit" => flag!(inherit, true),
                "noinherit" => flag!(inherit, false),
                "login" => flag!(login, true),
                "nologin" => flag!(login, false),
                "replication" => flag!(replication, true),
                "noreplication" => flag!(replication, false),
                "bypassrls" => flag!(bypassrls, true),
                "nobypassrls" => flag!(bypassrls, false),
                "connection" => {
                    self.expect_kw("limit")?;
                    let neg = self.sym("-");
                    let n = match self.next() {
                        Some(Tok::Num(n)) => n.parse::<i64>().map_err(|_| SqlError {
                            code: "22003",
                            message: format!("value \"{n}\" is out of range for type integer"),
                            info: None,
                        })?,
                        _ => return self.fail("expected a number"),
                    };
                    let n = if neg { -n } else { n };
                    if n < -1 {
                        return err("22023", format!("invalid connection limit: {n}"));
                    }
                    flag!(connlimit, n);
                }
                "encrypted" | "unencrypted" | "password" => {
                    if w == "unencrypted" {
                        return err(UNSUPPORTED, "UNENCRYPTED PASSWORD is no longer supported");
                    }
                    if w == "encrypted" {
                        self.expect_kw("password")?;
                    }
                    let p = match self.next() {
                        Some(Tok::Str(s)) => Some(s),
                        Some(Tok::Word(n)) if n == "null" => None,
                        _ => return self.fail("expected a password or NULL"),
                    };
                    flag!(password, p);
                }
                "valid" => {
                    self.expect_kw("until")?;
                    let Some(Tok::Str(s)) = self.next() else { return self.fail("expected a time, like '2030-01-01'") };
                    flag!(valid_until, s);
                }
                "sysid" if create => {
                    if !matches!(self.next(), Some(Tok::Num(_))) {
                        return self.fail("expected a number");
                    }
                    o.sysid = true;
                }
                "in" if create => {
                    if !self.kw("role") {
                        self.expect_kw("group")?;
                    }
                    o.in_role.extend(self.role_names()?);
                }
                "role" | "user" if create => o.role.extend(self.role_names()?),
                "admin" if create => o.admin.extend(self.role_names()?),
                _ => {
                    self.i -= 1;
                    break;
                }
            }
        }
        Ok(o)
    }

    /// After CREATE: ROLE | USER | GROUP name [WITH] options (the caller saw the word).
    pub(super) fn create_role(&mut self, user: bool) -> Result<Stmt> {
        let name = self.role_name()?;
        let opts = self.role_opts(true)?;
        Ok(Stmt::Role(Box::new(RoleStmt::Create { name, user, opts })))
    }

    /// After ALTER ROLE | USER | GROUP.
    pub(super) fn alter_role(&mut self, group: bool) -> Result<Stmt> {
        if self.kw("all") {
            let setting = self.role_setting()?;
            return Ok(Stmt::Role(Box::new(RoleStmt::Config { name: Spec::Public, setting })));
        }
        let name = self.role_spec(false)?;
        if self.kw("rename") {
            self.expect_kw("to")?;
            let Spec::Name(from) = name else {
                return err(SYNTAX, "cannot rename a role named as CURRENT_USER or SESSION_USER: name it");
            };
            return Ok(Stmt::Role(Box::new(RoleStmt::Rename { name: from, to: self.role_name()? })));
        }
        if group && (self.is_kw("add") || self.is_kw("drop")) {
            // ALTER GROUP g ADD | DROP USER u, ...: GRANT g TO u / REVOKE g FROM u
            let add = self.kw("add");
            if !add {
                self.expect_kw("drop")?;
            }
            self.expect_kw("user")?;
            let members = self.role_specs(false)?;
            let Spec::Name(g) = name else { return self.fail("expected a group's name") };
            return Ok(Stmt::Role(Box::new(RoleStmt::AlterGroup { group: g, members, add })));
        }
        if self.kw("in") {
            self.expect_kw("database")?;
            self.name()?;
            return err(
                UNSUPPORTED,
                "ALTER ROLE ... IN DATABASE isn't supported: ALTER ROLE ... SET applies in every world",
            );
        }
        if self.is_kw("set") || self.is_kw("reset") {
            let setting = self.role_setting()?;
            return Ok(Stmt::Role(Box::new(RoleStmt::Config { name, setting })));
        }
        Ok(Stmt::Role(Box::new(RoleStmt::Alter { name, opts: self.role_opts(false)? })))
    }

    /// `SET name {= | TO} value | FROM CURRENT`, `RESET name`, `RESET ALL`: (name, value), None for RESET ALL.
    fn role_setting(&mut self) -> Result<Option<(String, Option<String>)>> {
        if self.kw("reset") {
            if self.kw("all") {
                return Ok(None);
            }
            return Ok(Some((self.setting_name()?, None)));
        }
        self.expect_kw("set")?;
        let name = self.setting_name()?;
        if !(self.sym("=") || self.kw("to")) {
            return self.fail("expected = or TO");
        }
        let value = self.setting_value()?;
        Ok(Some((name, value)))
    }

    /// After DROP ROLE | USER | GROUP.
    pub(super) fn drop_role(&mut self) -> Result<Stmt> {
        let if_exists = self.if_exists()?;
        let mut names = vec![];
        loop {
            if self.is_kw("current_user") || self.is_kw("current_role") || self.is_kw("session_user") {
                return err("22023", "cannot use special role specifier in DROP ROLE");
            }
            names.push(self.role_name()?);
            if !self.sym(",") {
                break;
            }
        }
        Ok(Stmt::Role(Box::new(RoleStmt::Drop { names, if_exists })))
    }

    /// GRANT roles TO members ..., after the role names (GRANT's own parser tells it from a GRANT
    /// of privileges by the ON that one has).
    pub(super) fn grant_role(&mut self, roles: Vec<String>) -> Result<Stmt> {
        self.expect_kw("to")?;
        let members = self.role_specs(false)?;
        let (mut admin, mut inherit, mut set) = (None, None, None);
        if self.kw("with") {
            loop {
                let which = if self.kw("admin") {
                    &mut admin
                } else if self.kw("inherit") {
                    &mut inherit
                } else if self.kw("set") {
                    &mut set
                } else {
                    return self.fail("expected ADMIN, INHERIT or SET");
                };
                let v = if self.kw("option") || self.kw("true") {
                    true
                } else if self.kw("false") {
                    false
                } else {
                    return self.fail("expected OPTION, TRUE or FALSE");
                };
                *which = Some(v);
                if !self.sym(",") {
                    break;
                }
            }
        }
        let by = if self.kw("granted") {
            self.expect_kw("by")?;
            Some(self.role_spec(false)?)
        } else {
            None
        };
        Ok(Stmt::Role(Box::new(RoleStmt::Grant { roles, members, admin, inherit, set, by })))
    }

    /// REVOKE [option FOR] roles FROM members ..., after the role names.
    pub(super) fn revoke_role(&mut self, option: Option<&'static str>, roles: Vec<String>) -> Result<Stmt> {
        self.expect_kw("from")?;
        let members = self.role_specs(false)?;
        let by = if self.kw("granted") {
            self.expect_kw("by")?;
            Some(self.role_spec(false)?)
        } else {
            None
        };
        let cascade = self.cascade();
        Ok(Stmt::Role(Box::new(RoleStmt::Revoke { roles, members, option, by, cascade })))
    }

    /// SET [SESSION | LOCAL] ROLE name | NONE, after ROLE; RESET ROLE is `reset`. SET ROLE { TO | = } DEFAULT is
    /// Postgres's generic `SET var TO DEFAULT`, which for the role is RESET ROLE.
    pub(super) fn set_role(&mut self, local: bool, reset: bool) -> Result<Stmt> {
        if reset {
            return Ok(Stmt::SetRole(None, false));
        }
        let assigned = self.sym("=") || self.kw("to");
        if assigned && self.kw("default") {
            return Ok(Stmt::SetRole(None, local));
        }
        if self.kw("none") {
            return Ok(Stmt::SetRole(None, local));
        }
        let name = match self.next() {
            Some(Tok::Str(s)) if s == "none" => return Ok(Stmt::SetRole(None, local)),
            Some(Tok::Str(s) | Tok::Word(s) | Tok::Ident(s)) => s,
            _ => return self.fail("expected a role's name"),
        };
        Ok(Stmt::SetRole(Some(name), local))
    }

    /// SET [SESSION | LOCAL] SESSION AUTHORIZATION name | DEFAULT, after AUTHORIZATION.
    pub(super) fn set_auth(&mut self, local: bool, reset: bool) -> Result<Stmt> {
        if reset || self.kw("default") {
            return Ok(Stmt::SetAuth(None, local));
        }
        let name = match self.next() {
            Some(Tok::Str(s) | Tok::Word(s) | Tok::Ident(s)) => s,
            _ => return self.fail("expected a role's name"),
        };
        Ok(Stmt::SetAuth(Some(name), local))
    }
}

/// Postgres's reserved role names.
fn reserved(name: &str) -> Result<()> {
    if name.starts_with("pg_") {
        return Err(SqlError {
            code: "42939",
            message: format!("role name \"{name}\" is reserved"),
            info: Some(Box::new(Violation {
                detail: Some("Role names starting with \"pg_\" are reserved.".into()),
                ..Violation::default()
            })),
        });
    }
    if matches!(name, "public" | "none") {
        return err("42939", format!("role name \"{name}\" is reserved"));
    }
    Ok(())
}

/// 42501 with Postgres's DETAIL.
pub(super) fn denied(message: impl Into<String>, detail: impl Into<String>) -> SqlError {
    SqlError {
        code: "42501",
        message: message.into(),
        info: Some(Box::new(Violation { detail: Some(detail.into()), ..Violation::default() })),
    }
}

/// Who the running statement runs as, the bootstrap superuser outside a session.
fn me() -> Who {
    current().map_or_else(|| Who::boot("chronos"), |w| (*w).clone())
}

/// Whether `who` has CREATEROLE (a superuser has everything).
fn createrole(who: &Who) -> bool {
    who.superuser || who.reg.by_oid(who.oid).is_some_and(|r| r.createrole)
}

fn has_attr(who: &Who, f: impl Fn(&Role) -> bool) -> bool {
    who.superuser || who.reg.by_oid(who.oid).is_some_and(|r| f(&r))
}

/// `valid until`'s text as a moment: µs since 1970.
fn valid_until(text: &str) -> Result<i64> {
    match coerce(Value::Text(text.into()), Type::TimestampTz)? {
        Value::TimestampTz(t) => Ok(t),
        _ => err("22007", format!("invalid input syntax for type timestamp with time zone: \"{text}\"")),
    }
}

/// The password a role keeps, as `password_encryption` says (see `scram`).
fn kept_password(p: &Option<String>, role: &str) -> Result<Option<String>> {
    let Some(p) = p else { return Ok(None) };
    let encryption = setting_now("password_encryption").unwrap_or_else(|| "scram-sha-256".into());
    // (a hash of the empty password is as good as the empty password)
    let empty_hash = super::scram::is_md5(p) && *p == super::scram::md5_hash("", role)
        || super::scram::parse(p).is_some_and(|v| super::scram::verifier("", &v.salt, v.iterations) == *p);
    if p.is_empty() || empty_hash {
        notice("NOTICE", "empty string is not a valid password, clearing password".into());
        return Ok(None);
    }
    super::scram::kept(p, role, &encryption, scram_iterations()).map(Some)
}

/// The iteration count a new SCRAM verifier gets: the running statement's session's
/// `scram_iterations`, as Postgres has it.
pub(super) fn scram_iterations() -> u32 {
    setting_now("scram_iterations").and_then(|n| n.parse().ok()).filter(|n| *n > 0).unwrap_or(super::scram::ITERATIONS)
}

/// A setting as the running statement's session has it, if it set it.
pub(super) fn setting_now(name: &str) -> Option<String> {
    let ctx = STOP.with_borrow(|s| s.as_ref().and_then(|s| s.ctx.clone()))?;
    let st = ctx.state.lock().unwrap();
    st.local.get(name).or_else(|| st.settings.get(name)).cloned()
}

/// Runs a role statement (it changes main's registry, whichever world it runs in).
pub(super) fn run(db: &Db, st: &RoleStmt) -> Result<Output> {
    let who = me();
    match st {
        RoleStmt::Create { name, user, opts } => create(db, &who, name, *user, opts),
        RoleStmt::Alter { name, opts } => alter(db, &who, name, opts),
        RoleStmt::Rename { name, to } => rename(db, &who, name, to),
        RoleStmt::Config { name, setting } => config(db, &who, name, setting),
        RoleStmt::Drop { names, if_exists } => drop_roles(db, &who, names, *if_exists),
        RoleStmt::AlterGroup { group, members, add } => {
            // (ADMIN OPTION on the group first, as Postgres's AlterRole asks it)
            if let Some(r) = who.reg.named(group)
                && !who.superuser
                && !who.reg.is_admin_of(who.oid, r.oid)
            {
                return Err(denied(
                    "permission denied to alter role",
                    format!("Only roles with the ADMIN option on role \"{group}\" may add or drop members."),
                ));
            }
            let roles = std::slice::from_ref(group);
            match add {
                true => grant(db, &who, roles, members, (None, None, None), None),
                false => revoke(db, &who, roles, members, None, None, false),
            }
        }
        RoleStmt::Grant { roles, members, admin, inherit, set, by } => {
            grant(db, &who, roles, members, (*admin, *inherit, *set), by.as_ref())
        }
        RoleStmt::Revoke { roles, members, option, by, cascade } => {
            revoke(db, &who, roles, members, *option, by.as_ref(), *cascade)
        }
    }
}

/// Whether `name` already names a role though none was made with it: the bootstrap superuser's
/// names (`chronos`, and `postgres`, which Postgres's dumps give it), and a login that is no role,
/// a superuser: the session's own, and every open connection's that isn't an agent's. A role of
/// such a name would take that login over from its next connection (as a role of the agent's
/// name may, on purpose), so CREATE ROLE and RENAME TO refuse it as Postgres refuses a name taken
/// (42710).
fn taken(db: &Db, reg: &Registry, name: &str) -> bool {
    if reg.named(name).is_some() || name == "postgres" {
        return true;
    }
    let (_, session) = names();
    if session == name {
        return true;
    }
    db.agent(name).is_err() && backends().iter().any(|(_, b)| b.role.is_none() && b.user == name)
}

fn already(name: &str) -> SqlError {
    SqlError { code: "42710", message: format!("role \"{name}\" already exists"), info: None }
}

fn create(db: &Db, who: &Who, name: &str, user: bool, o: &Opts) -> Result<Output> {
    // (as Postgres reads the options, before it checks anything)
    if o.sysid {
        notice("NOTICE", "SYSID can no longer be specified".into());
    }
    reserved(name)?;
    let detail =
        |attr: &str| format!("Only roles with the {attr} attribute may create roles with the {attr} attribute.");
    if !createrole(who) {
        return Err(denied(
            "permission denied to create role",
            "Only roles with the CREATEROLE attribute may create roles.",
        ));
    }
    if o.superuser == Some(true) && !who.superuser {
        return Err(denied("permission denied to create role", detail("SUPERUSER")));
    }
    if o.createdb == Some(true) && !has_attr(who, |r| r.createdb) {
        return Err(denied("permission denied to create role", detail("CREATEDB")));
    }
    if o.replication == Some(true) && !has_attr(who, |r| r.replication) {
        return Err(denied("permission denied to create role", detail("REPLICATION")));
    }
    if o.bypassrls == Some(true) && !has_attr(who, |r| r.bypassrls) {
        return Err(denied("permission denied to create role", detail("BYPASSRLS")));
    }
    let until = o.valid_until.as_deref().map(valid_until).transpose()?;
    let password = match &o.password {
        Some(p) => kept_password(p, name)?,
        None => None,
    };
    if taken(db, &who.reg, name) {
        return Err(already(name));
    }
    let self_grant = super::session_setting("createrole_self_grant");
    change(db, |reg| {
        if reg.named(name).is_some() {
            return Err(already(name));
        }
        let oid = reg.new_oid();
        let r = Role {
            oid,
            name: name.into(),
            superuser: o.superuser.unwrap_or(false),
            inherit: o.inherit.unwrap_or(true),
            createrole: o.createrole.unwrap_or(false),
            createdb: o.createdb.unwrap_or(false),
            login: o.login.unwrap_or(user),
            replication: o.replication.unwrap_or(false),
            bypassrls: o.bypassrls.unwrap_or(false),
            connlimit: o.connlimit.unwrap_or(-1),
            password: password.clone(),
            valid_until: until,
            config: vec![],
        };
        reg.roles.push(r);
        // a role made by a CREATEROLE user who isn't a superuser: its maker gets ADMIN OPTION on it,
        // granted by the bootstrap superuser; and with createrole_self_grant set (`set, inherit`), a membership
        // with those options too, granted by the maker itself (as Postgres 16's CreateRole)
        if !who.superuser {
            let g = reg.new_oid();
            reg.members.push(Member {
                oid: g,
                role: oid,
                member: who.oid,
                grantor: BOOTSTRAP,
                admin: true,
                inherit: false,
                set: false,
            });
            let options = self_grant.as_deref().unwrap_or_default().to_lowercase();
            let has = |o: &str| options.split(',').any(|x| x.trim() == o);
            if has("set") || has("inherit") {
                let g = reg.new_oid();
                reg.members.push(Member {
                    oid: g,
                    role: oid,
                    member: who.oid,
                    grantor: who.oid,
                    admin: false,
                    inherit: has("inherit"),
                    set: has("set"),
                });
            }
        }
        let snapshot = Who { reg: Arc::new(reg.clone()), ..who.clone() };
        for r in &o.in_role {
            add_member(reg, &snapshot, r, oid, (None, None, None), None)?;
        }
        for m in &o.role {
            let member = reg.named(m).ok_or_else(|| missing(m))?;
            add_member(reg, &snapshot, name, member.oid, (None, None, None), None)?;
        }
        for m in &o.admin {
            let member = reg.named(m).ok_or_else(|| missing(m))?;
            add_member(reg, &snapshot, name, member.oid, (Some(true), None, None), None)?;
        }
        Ok(())
    })?;
    tag("CREATE ROLE")
}

fn missing(name: &str) -> SqlError {
    SqlError { code: "42704", message: format!("role \"{name}\" does not exist"), info: None }
}

/// Whether `who` may change role `r` at all (ALTER, RENAME, DROP): CREATEROLE and ADMIN OPTION on it.
fn may_manage(who: &Who, r: &Role) -> bool {
    who.superuser || (createrole(who) && who.reg.is_admin_of(who.oid, r.oid))
}

fn alter(db: &Db, who: &Who, name: &Spec, o: &Opts) -> Result<Output> {
    let r = name.resolve(&who.reg)?;
    let no = |detail: String| Err(denied("permission denied to alter role", detail));
    let attr_change = |attr: &str| format!("Only roles with the {attr} attribute may change the {attr} attribute.");
    if r.superuser && !who.superuser {
        return no("Only roles with the SUPERUSER attribute may alter roles with the SUPERUSER attribute.".into());
    }
    if o.superuser.is_some() && !who.superuser {
        return no(attr_change("SUPERUSER"));
    }
    // (as Postgres 16's AlterRole: most changes take CREATEROLE and ADMIN OPTION on the role; with those, CREATEDB,
    // REPLICATION and BYPASSRLS are changed only by a role that has them itself)
    if !may_manage(who, &r) {
        let others = o.inherit.is_some()
            || o.createrole.is_some()
            || o.createdb.is_some()
            || o.login.is_some()
            || o.connlimit.is_some()
            || o.valid_until.is_some()
            || o.replication.is_some()
            || o.bypassrls.is_some();
        if others {
            return no(format!(
                "Only roles with the CREATEROLE attribute and the ADMIN option on role \"{}\" may alter this role.",
                r.name
            ));
        }
        if o.password.is_some() && r.oid != who.oid {
            return no("To change another role's password, the current user must have the CREATEROLE attribute and the ADMIN option on the role.".into());
        }
    } else if !who.superuser {
        if o.createdb.is_some() && !has_attr(who, |x| x.createdb) {
            return no(attr_change("CREATEDB"));
        }
        if o.replication.is_some() && !has_attr(who, |x| x.replication) {
            return no(attr_change("REPLICATION"));
        }
        if o.bypassrls.is_some() && !has_attr(who, |x| x.bypassrls) {
            return no(attr_change("BYPASSRLS"));
        }
    }
    if r.oid == BOOTSTRAP || r.oid < FIRST_OID {
        return no(format!(
            "The bootstrap superuser and the predefined roles can't be changed here: \"{}\" is one.",
            r.name
        ));
    }
    let until = o.valid_until.as_deref().map(valid_until).transpose()?;
    let password = match &o.password {
        Some(p) => Some(kept_password(p, &r.name)?),
        None => None,
    };
    change(db, |reg| {
        let Some(x) = reg.roles.iter_mut().find(|x| x.oid == r.oid) else { return Err(missing(&r.name)) };
        let set = |f: &mut bool, v: Option<bool>| {
            if let Some(v) = v {
                *f = v;
            }
        };
        set(&mut x.superuser, o.superuser);
        set(&mut x.inherit, o.inherit);
        set(&mut x.createrole, o.createrole);
        set(&mut x.createdb, o.createdb);
        set(&mut x.login, o.login);
        set(&mut x.replication, o.replication);
        set(&mut x.bypassrls, o.bypassrls);
        if let Some(n) = o.connlimit {
            x.connlimit = n;
        }
        if let Some(p) = password {
            x.password = p;
        }
        if until.is_some() {
            x.valid_until = until;
        }
        Ok(())
    })?;
    tag("ALTER ROLE")
}

fn rename(db: &Db, who: &Who, name: &str, to: &str) -> Result<Output> {
    let r = who.reg.named(name).ok_or_else(|| missing(name))?;
    let (current, session) = names();
    if r.name == session {
        return err(UNSUPPORTED, "session user cannot be renamed");
    }
    if r.name == current {
        return err(UNSUPPORTED, "current user cannot be renamed");
    }
    if r.oid < FIRST_OID {
        return err(UNSUPPORTED, format!("role name \"{name}\" is reserved"));
    }
    reserved(to)?;
    if taken(db, &who.reg, to) {
        return Err(already(to));
    }
    if r.superuser && !who.superuser {
        return Err(denied(
            "permission denied to rename role",
            "Only roles with the SUPERUSER attribute may rename roles with the SUPERUSER attribute.",
        ));
    }
    if !may_manage(who, &r) {
        return Err(denied(
            "permission denied to rename role",
            format!(
                "Only roles with the CREATEROLE attribute and the ADMIN option on role \"{name}\" may rename this role."
            ),
        ));
    }
    change(db, |reg| {
        if reg.named(to).is_some() {
            return Err(already(to));
        }
        let Some(x) = reg.roles.iter_mut().find(|x| x.oid == r.oid) else { return Err(missing(name)) };
        x.name = to.into();
        // an md5 hash is of the password and the name: it no longer matches
        if x.password.as_deref().is_some_and(super::scram::is_md5) {
            x.password = None;
            notice("NOTICE", "MD5 password cleared because of role rename".into());
        }
        Ok(())
    })?;
    tag("ALTER ROLE")
}

fn config(db: &Db, who: &Who, name: &Spec, setting: &Option<(String, Option<String>)>) -> Result<Output> {
    if *name == Spec::Public {
        return err(UNSUPPORTED, "ALTER ROLE ALL ... SET isn't supported");
    }
    let r = name.resolve(&who.reg)?;
    if !may_manage(who, &r) && r.oid != who.oid {
        return Err(denied(
            "permission denied to alter role",
            format!(
                "Only roles with the CREATEROLE attribute and the ADMIN option on role \"{}\" may alter this role.",
                r.name
            ),
        ));
    }
    if r.oid < FIRST_OID {
        return err(UNSUPPORTED, format!("the settings of role \"{}\" can't be changed here", r.name));
    }
    // the value as SET would take it, kept as Postgres writes it: `name=value`
    let entry = match setting {
        Some((n, Some(v))) => {
            let checked = checked_setting(n, Some(v))?;
            let shown = settings::find(n).map_or_else(|| n.to_lowercase(), |d| d.name.to_string());
            Some((shown.clone(), Some(format!("{shown}={}", checked.unwrap_or_default()))))
        }
        Some((n, None)) => Some((settings::find(n).map_or_else(|| n.to_lowercase(), |d| d.name.to_string()), None)),
        None => None,
    };
    change(db, |reg| {
        let Some(x) = reg.roles.iter_mut().find(|x| x.oid == r.oid) else { return Err(missing(&r.name)) };
        match &entry {
            None => x.config.clear(),
            Some((n, v)) => {
                let at =
                    x.config.iter().position(|c| c.split_once('=').is_some_and(|(k, _)| k.eq_ignore_ascii_case(n)));
                match (at, v) {
                    (Some(i), Some(v)) => x.config[i] = v.clone(),
                    (None, Some(v)) => x.config.push(v.clone()),
                    (Some(i), None) => {
                        x.config.remove(i);
                    }
                    (None, None) => {}
                }
            }
        }
        Ok(())
    })?;
    tag("ALTER ROLE")
}

fn drop_roles(db: &Db, who: &Who, names: &[String], if_exists: bool) -> Result<Output> {
    if !createrole(who) {
        return Err(denied(
            "permission denied to drop role",
            "Only roles with the CREATEROLE attribute and the ADMIN option on the target roles may drop roles.",
        ));
    }
    let (current, session) = names_now();
    // (the roles it drops: a grant of one, or to one, goes with it)
    let dropping: Vec<i64> = names.iter().filter_map(|n| who.reg.named(n)).map(|r| r.oid).collect();
    let mut gone = vec![];
    for name in names {
        // (`postgres` is the bootstrap superuser's other name)
        let Some(r) = who.reg.named(name).or_else(|| (name == "postgres").then(Role::bootstrap)) else {
            if if_exists {
                notice("NOTICE", format!("role \"{name}\" does not exist, skipping"));
                continue;
            }
            return Err(missing(name));
        };
        if r.oid < FIRST_OID {
            return err("2BP01", format!("cannot drop role {name} because it is required by the database system"));
        }
        if r.name == current {
            return err("55006", "current user cannot be dropped");
        }
        if r.name == session {
            return err("55006", "session user cannot be dropped");
        }
        if r.superuser && !who.superuser {
            return Err(denied(
                "permission denied to drop role",
                "Only roles with the SUPERUSER attribute may drop roles with the SUPERUSER attribute.",
            ));
        }
        if !may_manage(who, &r) {
            return Err(denied(
                "permission denied to drop role",
                format!(
                    "Only roles with the CREATEROLE attribute and the ADMIN option on role \"{name}\" may drop this role."
                ),
            ));
        }
        // what it owns or has been granted, in any world; and the grants of roles it made (with its ADMIN OPTION)
        let mut deps = super::acl::dependents(db, r.oid)?;
        for g in who.reg.members.iter() {
            if g.grantor == r.oid && !dropping.contains(&g.role) && !dropping.contains(&g.member) {
                deps.push(format!(
                    "privileges for membership of role {} in role {}",
                    who.reg.name_of(g.member),
                    who.reg.name_of(g.role)
                ));
            }
        }
        if !deps.is_empty() {
            return Err(SqlError {
                code: "2BP01",
                message: format!("role \"{name}\" cannot be dropped because some objects depend on it"),
                info: Some(Box::new(Violation { detail: Some(deps.join("\n")), ..Violation::default() })),
            });
        }
        gone.push(r.oid);
    }
    if !gone.is_empty() {
        change(db, |reg| {
            reg.roles.retain(|r| !gone.contains(&r.oid));
            reg.members.retain(|m| !gone.contains(&m.role) && !gone.contains(&m.member));
            // grants it made go too, as Postgres 16's DROP ROLE takes them
            reg.members.retain(|m| !gone.contains(&m.grantor));
            Ok(())
        })?;
    }
    tag("DROP ROLE")
}

/// current_user and session_user, for the statement running now.
fn names_now() -> (String, String) {
    names()
}

/// Adds (or updates) the grant of `role` to `member` by `who` (as GRANTED BY `by` says, else as
/// Postgres picks): the checks GRANT role makes.
fn add_member(
    reg: &mut Registry,
    who: &Who,
    role: &str,
    member: i64,
    (admin, inherit, set): (Option<bool>, Option<bool>, Option<bool>),
    by: Option<i64>,
) -> Result<Option<String>> {
    let r = reg.named(role).ok_or_else(|| missing(role))?;
    let m = reg.by_oid(member).ok_or_else(|| missing(&member.to_string()))?;
    if r.oid == DB_OWNER {
        return err("0LP01", "role \"pg_database_owner\" cannot have explicit members");
    }
    let grantor = grantor(who, &r, by)?;
    if r.oid == m.oid || reg.is_member_of_any(r.oid, m.oid) {
        return err("0LP01", format!("role \"{}\" is a member of role \"{}\"", r.name, m.name));
    }
    // ADMIN OPTION back to a role the grantor holds it from: were every grant of the role to the member gone (and
    // what rests on them), the grantor would have no ADMIN OPTION left to make it, so the chains of grants would
    // loop (Postgres keeps them acyclic, so REVOKE ... CASCADE can always clean up)
    if admin == Some(true) && grantor != BOOTSTRAP {
        let mut without = reg.clone();
        without.members.retain(|g| !(g.role == r.oid && g.member == m.oid));
        drop_dependent(&mut without, r.oid, m.oid);
        if !without.members.iter().any(|g| g.role == r.oid && g.member == grantor && g.admin) {
            return err("0LP01", "ADMIN option cannot be granted back to your own grantor");
        }
    }
    if let Some(g) = reg.members.iter_mut().find(|g| g.role == r.oid && g.member == m.oid && g.grantor == grantor) {
        let before = (g.admin, g.inherit, g.set);
        if let Some(a) = admin {
            g.admin = a;
        }
        if let Some(i) = inherit {
            g.inherit = i;
        }
        if let Some(s) = set {
            g.set = s;
        }
        if before == (g.admin, g.inherit, g.set) {
            return Ok(Some(format!(
                "role \"{}\" has already been granted membership in role \"{}\" by role \"{}\"",
                m.name,
                r.name,
                reg.name_of(grantor)
            )));
        }
        return Ok(None);
    }
    let oid = reg.new_oid();
    reg.members.push(Member {
        oid,
        role: r.oid,
        member: m.oid,
        grantor,
        admin: admin.unwrap_or(false),
        inherit: inherit.unwrap_or(m.inherit),
        set: set.unwrap_or(true),
    });
    Ok(None)
}

impl Registry {
    /// `is_member_of` through grants only (not as a superuser): what makes a grant circular.
    fn is_member_of_any(&self, member: i64, role: i64) -> bool {
        self.reached(member, Through::Any).contains(&role)
    }
}

/// Who a grant of role `r` is recorded as made by: the bootstrap superuser for a superuser, else
/// the role through which `who` holds ADMIN OPTION on it (or GRANTED BY's, which must hold it).
fn grantor(who: &Who, r: &Role, by: Option<i64>) -> Result<i64> {
    grantor_as(who, r, by, true)
}

/// `grantor`, for a GRANT (`granting`) or a REVOKE, as Postgres's check_role_grantor: a REVOKE ... GRANTED BY needs
/// only the grantor's privileges (whether it still holds ADMIN OPTION doesn't matter), and its messages say revoke.
fn grantor_as(who: &Who, r: &Role, by: Option<i64>, granting: bool) -> Result<i64> {
    if let Some(b) = by {
        let name = who.reg.name_of(b);
        if !who.has_privs_of(b) {
            return Err(match granting {
                true => denied(
                    format!("permission denied to grant privileges as role \"{name}\""),
                    format!("Only roles with privileges of role \"{name}\" may grant privileges as this role."),
                ),
                false => denied(
                    format!("permission denied to revoke privileges granted by role \"{name}\""),
                    format!(
                        "Only roles with privileges of role \"{name}\" may revoke privileges granted by this role."
                    ),
                ),
            });
        }
        if !granting {
            return Ok(if who.reg.superuser(b) { BOOTSTRAP } else { b });
        }
        if b != BOOTSTRAP && who.reg.admin_of(b, r.oid) != Some(b) && !who.reg.superuser(b) {
            return Err(denied(
                format!("permission denied to grant privileges as role \"{name}\""),
                format!("The grantor must have the ADMIN option on role \"{}\".", r.name),
            ));
        }
        return Ok(if who.reg.superuser(b) { BOOTSTRAP } else { b });
    }
    if who.superuser {
        return Ok(BOOTSTRAP);
    }
    let verb = if granting { "grant" } else { "revoke" };
    // (to grant or revoke a superuser role, you have to be one: Postgres's check_role_membership_authorization)
    if r.superuser {
        return Err(denied(
            format!("permission denied to {verb} role \"{}\"", r.name),
            format!("Only roles with the SUPERUSER attribute may {verb} roles with the SUPERUSER attribute."),
        ));
    }
    who.reg.admin_of(who.oid, r.oid).ok_or_else(|| {
        denied(
            format!("permission denied to {verb} role \"{}\"", r.name),
            format!("Only roles with the ADMIN option on role \"{}\" may {verb} this role.", r.name),
        )
    })
}

fn grant(
    db: &Db,
    who: &Who,
    roles: &[String],
    members: &[Spec],
    opts: (Option<bool>, Option<bool>, Option<bool>),
    by: Option<&Spec>,
) -> Result<Output> {
    let by = by.map(|b| b.resolve(&who.reg).map(|r| r.oid)).transpose()?;
    let members: Vec<i64> = members.iter().map(|m| m.resolve(&who.reg).map(|r| r.oid)).collect::<Result<_>>()?;
    let notes = change(db, |reg| {
        let mut notes = vec![];
        for r in roles {
            for m in &members {
                let snapshot = Who { reg: Arc::new(reg.clone()), ..who.clone() };
                notes.extend(add_member(reg, &snapshot, r, *m, opts, by)?);
            }
        }
        Ok(notes)
    })?;
    for n in notes {
        notice("NOTICE", n);
    }
    tag("GRANT ROLE")
}

fn revoke(
    db: &Db,
    who: &Who,
    roles: &[String],
    members: &[Spec],
    option: Option<&'static str>,
    by: Option<&Spec>,
    cascade: bool,
) -> Result<Output> {
    let by = by.map(|b| b.resolve(&who.reg).map(|r| r.oid)).transpose()?;
    let members: Vec<Role> = members.iter().map(|m| m.resolve(&who.reg)).collect::<Result<_>>()?;
    let warnings = change(db, |reg| {
        let mut warnings = vec![];
        for name in roles {
            let r = reg.named(name).ok_or_else(|| missing(name))?;
            let grantor = grantor_as(&Who { reg: Arc::new(reg.clone()), ..who.clone() }, &r, by, false)?;
            for m in &members {
                let Some(at) =
                    reg.members.iter().position(|g| g.role == r.oid && g.member == m.oid && g.grantor == grantor)
                else {
                    warnings.push(format!(
                        "role \"{}\" has not been granted membership in role \"{}\" by role \"{}\"",
                        m.name,
                        r.name,
                        reg.name_of(grantor)
                    ));
                    continue;
                };
                // grants the member made with its ADMIN OPTION depend on it, unless another grant
                // still gives the member that option
                let loses_admin = option.is_none_or(|o| o == "admin")
                    && reg.members[at].admin
                    && !reg
                        .members
                        .iter()
                        .enumerate()
                        .any(|(i, g)| i != at && g.role == r.oid && g.member == m.oid && g.admin);
                if loses_admin {
                    let dependent: Vec<i64> =
                        reg.members.iter().filter(|g| g.role == r.oid && g.grantor == m.oid).map(|g| g.oid).collect();
                    if !dependent.is_empty() && !cascade {
                        return Err(SqlError {
                            code: "2BP01",
                            message: "dependent privileges exist".into(),
                            info: Some(Box::new(Violation {
                                hint: Some("Use CASCADE to revoke them too.".into()),
                                ..Violation::default()
                            })),
                        });
                    }
                    drop_dependent(reg, r.oid, m.oid);
                }
                let Some(at) =
                    reg.members.iter().position(|g| g.role == r.oid && g.member == m.oid && g.grantor == grantor)
                else {
                    continue;
                };
                match option {
                    None => {
                        reg.members.remove(at);
                    }
                    Some("admin") => reg.members[at].admin = false,
                    Some("inherit") => reg.members[at].inherit = false,
                    _ => reg.members[at].set = false,
                }
            }
        }
        Ok(warnings)
    })?;
    for w in warnings {
        notice("WARNING", w);
    }
    tag("REVOKE ROLE")
}

/// DROP OWNED BY roles `oids`: the grants of roles they made go (and those made from them in turn), as Postgres 16's
/// DROP OWNED revokes them.
pub(crate) fn drop_grants_by(db: &Db, oids: &[i64]) -> Result<()> {
    if !registry(&db.core)?.members.iter().any(|g| oids.contains(&g.grantor)) {
        return Ok(());
    }
    change(db, |reg| {
        for &o in oids {
            let roles: Vec<i64> = reg.members.iter().filter(|g| g.grantor == o).map(|g| g.role).collect();
            for r in roles {
                drop_dependent(reg, r, o);
            }
        }
        Ok(())
    })
}

/// Drops the grants of `role` that `grantor` made, and those made from them in turn.
fn drop_dependent(reg: &mut Registry, role: i64, grantor: i64) {
    let made: Vec<Member> = reg.members.iter().filter(|g| g.role == role && g.grantor == grantor).cloned().collect();
    reg.members.retain(|g| !(g.role == role && g.grantor == grantor));
    // (and what those members granted in turn, unless another grant still gives them the option)
    for g in made.iter().filter(|g| g.admin) {
        if !reg.members.iter().any(|x| x.role == role && x.member == g.member && x.admin) {
            drop_dependent(reg, role, g.member);
        }
    }
}

// ---------------------------------------------------------------- SET ROLE

/// The role `name` is, for SET ROLE and SET SESSION AUTHORIZATION: 22023 if there's none.
fn set_target(reg: &Registry, name: &str) -> Result<Role> {
    reg.named(name).ok_or_else(|| SqlError {
        code: "22023",
        message: format!("role \"{name}\" does not exist"),
        info: None,
    })
}

/// What SET ROLE `name` makes the session's role (None: NONE), checked against the session user,
/// as Postgres checks it.
pub(crate) fn set_role(
    core: &crate::Core,
    ident: &Ident,
    agent: Option<&str>,
    login: &str,
    name: Option<&str>,
) -> Result<Option<i64>> {
    let Some(name) = name else { return Ok(None) };
    let reg = registry(core)?;
    let r = set_target(&reg, name)?;
    let (_, session) = who(core, ident, agent, login)?;
    if !(session.superuser || reg.can_set(session.oid, r.oid)) {
        return err("42501", format!("permission denied to set role \"{name}\""));
    }
    Ok(Some(r.oid))
}

/// What SET SESSION AUTHORIZATION `name` makes the session user (None: the login's).
pub(crate) fn set_auth(core: &crate::Core, ident: &Ident, name: Option<&str>) -> Result<Option<i64>> {
    let Some(name) = name else { return Ok(None) };
    let reg = registry(core)?;
    let r = set_target(&reg, name)?;
    if !(authenticated_superuser(&reg, ident) || ident.login == Some(r.oid)) {
        return err("42501", "permission denied to set session authorization");
    }
    Ok(Some(r.oid))
}

/// SHOW role: the role SET ROLE set, else `none`.
pub(crate) fn shown_role(core: &crate::Core, ident: &Ident) -> String {
    match ident.role {
        Some(oid) => registry(core).map_or_else(|_| oid.to_string(), |r| r.name_of(oid)),
        None => "none".into(),
    }
}

// ---------------------------------------------------------------- the catalog and functions

/// pg_has_role(...): whether `member` has privilege `mode` (USAGE, MEMBER, SET, and WITH ADMIN
/// OPTION) on `role`.
pub(crate) fn has_role(reg: &Registry, member: i64, role: i64, mode: &str) -> bool {
    let mode = mode.trim().to_lowercase();
    let with_admin = mode.ends_with("with admin option") || mode.ends_with("with grant option");
    if with_admin {
        return reg.is_admin_of(member, role);
    }
    match mode.as_str() {
        "usage" => reg.has_privs_of(member, role),
        "member" => reg.is_member_of(member, role),
        "set" => reg.can_set(member, role),
        _ => false,
    }
}

/// A role named in a `has_*_privilege` or `pg_has_role` call: by name or oid (as text or a number).
/// A login that is no role is the bootstrap superuser under its own name; `public` is 0 where
/// `public` allows it.
pub(crate) fn find(reg: &Registry, v: &Value, public: bool) -> Result<i64> {
    let session = STOP.with_borrow(|s| s.as_ref().and_then(|s| s.ctx.as_ref().map(|c| c.session_user.clone())));
    match v {
        // (the oid pg_roles gives a login that is no role: the bootstrap superuser it is)
        Value::Int(o) if *o == LOGIN_OID && reg.by_oid(*o).is_none() => Ok(BOOTSTRAP),
        Value::Int(o) => Ok(*o),
        v => {
            let name = v.to_text();
            if public && name == "public" {
                return Ok(PUBLIC);
            }
            if let Some(r) = reg.named(&name) {
                return Ok(r.oid);
            }
            if session.as_deref() == Some(name.as_str())
                || current().is_some_and(|w| w.name == name)
                || name == "postgres"
            {
                return Ok(BOOTSTRAP);
            }
            err("42704", format!("role \"{name}\" does not exist"))
        }
    }
}

/// pg_roles' and pg_authid's rows (`authid`: the passwords themselves), with the session's own
/// login name as a superuser when it is no role.
pub(crate) fn role_rows(reg: &Registry, login: Option<&str>, authid: bool) -> Vec<Vec<Value>> {
    let mut roles = reg.all();
    if let Some(l) = login.filter(|l| reg.named(l).is_none()) {
        roles.push(Role { oid: LOGIN_OID, ..Role { name: l.into(), ..Role::bootstrap() } });
    }
    roles
        .iter()
        .map(|r| {
            let t = |s: &str| Value::Text(s.into());
            vec![
                Value::Int(r.oid),
                t(&r.name),
                Value::Bool(r.superuser),
                Value::Bool(r.inherit),
                Value::Bool(r.createrole),
                Value::Bool(r.createdb),
                Value::Bool(r.login),
                Value::Bool(r.replication),
                Value::Int(r.connlimit),
                match authid {
                    true => r.password.as_deref().map_or(Value::Null, t),
                    false => t("********"),
                },
                r.valid_until.map_or(Value::Null, Value::TimestampTz),
                Value::Bool(r.bypassrls),
                config_value(&r.config),
            ]
        })
        .collect()
}

/// A role's settings as Postgres's text[] shows them.
fn config_value(c: &[String]) -> Value {
    if c.is_empty() {
        return Value::Null;
    }
    Value::Text(format!("{{{}}}", c.iter().map(|s| array_elem(s)).collect::<Vec<_>>().join(",")))
}

/// An element of an array's text, quoted where Postgres quotes it.
pub(crate) fn array_elem(s: &str) -> String {
    let plain = !s.is_empty()
        && !s.eq_ignore_ascii_case("null")
        && !s.chars().any(|c| matches!(c, '{' | '}' | ',' | '"' | '\\') || c.is_whitespace());
    if plain { s.to_string() } else { format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"")) }
}

/// pg_user's and pg_shadow's rows: the roles that may log in.
pub(crate) fn user_rows(reg: &Registry, login: Option<&str>, shadow: bool) -> Vec<Vec<Value>> {
    role_rows(reg, login, true)
        .into_iter()
        .filter(|r| r[6] == Value::Bool(true))
        .map(|r| {
            let password = if shadow { r[9].clone() } else { Value::Text("********".into()) };
            // usename, usesysid, usecreatedb, usesuper, userepl, usebypassrls, passwd, valuntil, useconfig
            vec![
                r[1].clone(),
                r[0].clone(),
                r[5].clone(),
                r[2].clone(),
                r[7].clone(),
                r[11].clone(),
                password,
                r[10].clone(),
                r[12].clone(),
            ]
        })
        .collect()
}

/// pg_auth_members' rows.
pub(crate) fn member_rows(reg: &Registry) -> Vec<Vec<Value>> {
    reg.members
        .iter()
        .map(|m| {
            vec![
                Value::Int(m.oid),
                Value::Int(m.role),
                Value::Int(m.member),
                Value::Int(m.grantor),
                Value::Bool(m.admin),
                Value::Bool(m.inherit),
                Value::Bool(m.set),
            ]
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn membership_follows_options() {
        let mut reg = Registry::default();
        for (o, n) in [(20000, "a"), (20001, "b"), (20002, "c")] {
            reg.roles.push(Role::new(o, n));
        }
        let grant = |reg: &mut Registry, role, member, inherit, set| {
            let oid = reg.new_oid();
            reg.members.push(Member { oid, role, member, grantor: BOOTSTRAP, admin: false, inherit, set });
        };
        grant(&mut reg, 20001, 20000, true, false); // b to a: inherit, no set
        grant(&mut reg, 20002, 20001, false, true); // c to b: set, no inherit
        assert!(reg.has_privs_of(20000, 20001) && !reg.can_set(20000, 20001));
        assert!(!reg.has_privs_of(20000, 20002), "inheritance stops at a grant without INHERIT");
        assert!(reg.is_member_of(20000, 20002), "membership goes through every grant");
        assert!(reg.can_set(20001, 20002) && !reg.can_set(20000, 20002));
        assert!(reg.has_privs_of(BOOTSTRAP, 20002), "a superuser has every role's privileges");
        assert_eq!(Registry::parse(&reg.to_json()).members, reg.members);
    }
}
