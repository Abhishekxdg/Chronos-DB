//! Object storage for pages (feature `s3`): finished pack files, their indexes and the manifest
//! are uploaded, so a database can be reopened on another machine (or after its local folder is
//! gone) and idle databases cost only storage. Works with any S3-compatible service.
//!
//!   s3://bucket/prefix   credentials and endpoint from the usual AWS_* environment variables
//!                        (AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY, AWS_REGION, AWS_ENDPOINT,
//!                        AWS_ALLOW_HTTP=true for a local MinIO)
//!   file:///some/dir     a local folder standing in for a bucket (tests, NFS)
//!
//! One writer per prefix, enforced by a lease: a `lease` object created only if absent and naming
//! the owning folder. A second folder gets an error; `takeover` replaces a dead owner's lease; the
//! owner re-checks its lease before publishing each manifest, so a writer that was taken over
//! stops publishing (fencing). Closing releases the lease.
//! ponytail: the re-check and the publish are two requests, not one conditional write, so a
//! takeover landing between them can still lose to one last manifest.

use std::io;

#[cfg(feature = "s3")]
mod imp {
    use object_store::path::Path;
    use object_store::{ObjectStore, ObjectStoreExt, PutMode, PutOptions, PutPayload};
    use std::io;
    use std::sync::Arc;

    pub struct Remote {
        store: Arc<dyn ObjectStore>,
        prefix: Path,
        rt: tokio::runtime::Runtime,
        url: String,
    }

    fn err(e: object_store::Error) -> io::Error {
        match e {
            object_store::Error::NotFound { .. } => io::Error::new(io::ErrorKind::NotFound, e.to_string()),
            e => io::Error::other(format!("object storage: {e}")),
        }
    }

    impl Remote {
        pub fn new(url: &str) -> io::Result<Remote> {
            let bad = |why: &str| io::Error::new(io::ErrorKind::InvalidInput, format!("remote '{url}': {why}"));
            let (store, prefix): (Arc<dyn ObjectStore>, Path) = if let Some(rest) = url.strip_prefix("s3://") {
                let (bucket, prefix) = rest.split_once('/').unwrap_or((rest, ""));
                if bucket.is_empty() {
                    return Err(bad("expected s3://bucket/prefix"));
                }
                let s3 = object_store::aws::AmazonS3Builder::from_env()
                    .with_bucket_name(bucket)
                    .build()
                    .map_err(|e| bad(&e.to_string()))?;
                (Arc::new(s3), Path::from(prefix))
            } else if let Some(dir) = url.strip_prefix("file://") {
                std::fs::create_dir_all(dir)?;
                let fs = object_store::local::LocalFileSystem::new_with_prefix(dir).map_err(|e| bad(&e.to_string()))?;
                (Arc::new(fs), Path::from(""))
            } else {
                return Err(bad("use s3://bucket/prefix or file:///path"));
            };
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
            Ok(Remote { store, prefix, rt, url: url.into() })
        }

        pub fn url(&self) -> &str {
            &self.url
        }

        fn path(&self, name: &str) -> Path {
            self.prefix.clone().join(name)
        }

        pub fn put(&self, name: &str, bytes: Vec<u8>) -> io::Result<()> {
            self.rt.block_on(self.store.put(&self.path(name), PutPayload::from(bytes))).map_err(err)?;
            Ok(())
        }

        /// Creates the object only if it doesn't exist yet; false if it already did.
        pub fn put_new(&self, name: &str, bytes: Vec<u8>) -> io::Result<bool> {
            let opts = PutOptions { mode: PutMode::Create, ..Default::default() };
            match self.rt.block_on(self.store.put_opts(&self.path(name), PutPayload::from(bytes), opts)) {
                Ok(_) => Ok(true),
                Err(object_store::Error::AlreadyExists { .. }) => Ok(false),
                Err(e) => Err(err(e)),
            }
        }

        /// None if the object doesn't exist.
        pub fn get(&self, name: &str) -> io::Result<Option<Vec<u8>>> {
            let r = self.rt.block_on(async {
                let r = self.store.get(&self.path(name)).await?;
                r.bytes().await
            });
            match r {
                Ok(b) => Ok(Some(b.to_vec())),
                Err(object_store::Error::NotFound { .. }) => Ok(None),
                Err(e) => Err(err(e)),
            }
        }

        pub fn delete(&self, name: &str) -> io::Result<()> {
            match self.rt.block_on(self.store.delete(&self.path(name))) {
                Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
                Err(e) => Err(err(e)),
            }
        }

        /// Names and sizes of the objects directly under the prefix: not in folders below it
        /// (`s3://b/db/sub/pack-00000007` is another database's pack, not this one's).
        pub fn list(&self) -> io::Result<Vec<(String, u64)>> {
            let found = self.rt.block_on(self.store.list_with_delimiter(Some(&self.prefix))).map_err(err)?;
            let direct = |p: &Path| p.prefix_match(&self.prefix).is_some_and(|rest| rest.count() == 1);
            Ok(found
                .objects
                .into_iter()
                .filter(|m| direct(&m.location))
                .filter_map(|m| Some((m.location.filename()?.to_string(), m.size)))
                .collect())
        }
    }
}

#[cfg(not(feature = "s3"))]
mod imp {
    use std::io;

    /// Without the `s3` feature there is no remote: this type has no values.
    pub enum Remote {}

    impl Remote {
        pub fn new(url: &str) -> io::Result<Remote> {
            Err(io::Error::other(format!(
                "remote '{url}': this build has no cloud storage; rebuild with `--features s3`"
            )))
        }
        pub fn url(&self) -> &str {
            match *self {}
        }
        pub fn put(&self, _: &str, _: Vec<u8>) -> io::Result<()> {
            match *self {}
        }
        pub fn get(&self, _: &str) -> io::Result<Option<Vec<u8>>> {
            match *self {}
        }
        pub fn put_new(&self, _: &str, _: Vec<u8>) -> io::Result<bool> {
            match *self {}
        }
        pub fn delete(&self, _: &str) -> io::Result<()> {
            match *self {}
        }
        pub fn list(&self) -> io::Result<Vec<(String, u64)>> {
            match *self {}
        }
    }
}

pub use imp::Remote;

/// Downloads `name` into `path` (via a temp file and rename) unless it's already there.
pub fn fetch(remote: &Remote, name: &str, path: &std::path::Path) -> io::Result<bool> {
    if path.exists() {
        return Ok(true);
    }
    let Some(bytes) = remote.get(name)? else { return Ok(false) };
    let tmp = path.with_extension("download");
    std::fs::write(&tmp, bytes)?;
    std::fs::File::open(&tmp)?.sync_all()?;
    std::fs::rename(&tmp, path)?;
    Ok(true)
}

/// This folder's identity as a writer: random, kept in the folder, so a restart after a crash
/// still owns the lease it held.
pub fn owner(dir: &std::path::Path) -> io::Result<String> {
    let path = dir.join("owner");
    if let Ok(id) = std::fs::read_to_string(&path) {
        return Ok(id.trim().to_string());
    }
    let seed = format!("{:?} {} {:?}", std::time::SystemTime::now(), std::process::id(), dir);
    let id = crate::pages::hex(blake3::hash(seed.as_bytes()).as_bytes())[..32].to_string();
    crate::disk::write_file(&path, id.as_bytes())?;
    Ok(id)
}

/// Where a folder notes the last manifest it knows the remote has (its hash): one it published
/// or started from.
const SEEN: &str = "remote.seen";

fn digest(manifest: &[u8]) -> String {
    crate::pages::hex(blake3::hash(manifest).as_bytes())
}

/// Notes that the remote has `manifest`, just published or fetched (see `adopt`).
pub fn saw(dir: &std::path::Path, manifest: &[u8]) -> io::Result<()> {
    crate::disk::write_file(&dir.join(SEEN), digest(manifest).as_bytes())
}

/// Lines this folder up with the remote before it opens, with the lease held. A fresh folder
/// starts from the remote's manifest. A folder with a database of its own must be where the remote
/// is: the remote's manifest is its own, or the last one it published. Otherwise another folder
/// has written since (it took the lease over, or this folder was attached to a remote that already
/// held a database), and opening would replay this folder's stale log and publish over that
/// work, reusing its pack numbers. That's refused; with `takeover` this folder's own database is
/// set aside in `stale-<ms>` (kept, not deleted) and the remote's is opened.
/// True if it set this folder's database aside.
pub fn adopt(remote: &Remote, dir: &std::path::Path, takeover: bool) -> io::Result<bool> {
    use std::fs;
    let Some(theirs) = remote.get("manifest")? else { return Ok(false) }; // nothing yet: ours goes up
    let ours = match fs::read(dir.join("manifest")) {
        Ok(b) => Some(b),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    let seen = fs::read_to_string(dir.join(SEEN)).ok();
    if ours.as_deref() == Some(&theirs[..]) || ours.is_some() && seen.is_some_and(|s| s.trim() == digest(&theirs)) {
        return Ok(false); // ours, or ours is newer (published next, or cut short before it was)
    }
    let packs = |d: &std::path::Path| -> io::Result<bool> {
        Ok(match fs::read_dir(d) {
            Ok(rd) => rd.flatten().any(|e| {
                let n = e.file_name();
                let n = n.to_string_lossy();
                n.starts_with("pack-") && !n.ends_with(".idx") && e.metadata().is_ok_and(|m| m.len() > 0)
            }),
            Err(e) if e.kind() == io::ErrorKind::NotFound => false,
            Err(e) => return Err(e),
        })
    };
    let mut set_aside = false;
    if ours.is_some() || !crate::disk::segments(dir)?.is_empty() || packs(&dir.join("pages"))? {
        if !takeover {
            return Err(io::Error::other(format!(
                "{} has changes this folder hasn't seen (another folder wrote to it since, or it already held \
                 a database when this folder was attached); open it from a new, empty folder, or with takeover \
                 to set this folder's database aside",
                remote.url()
            )));
        }
        set_aside = true;
        let aside = dir.join(format!("stale-{}", crate::disk::now_ms()));
        fs::create_dir(&aside)?;
        for e in fs::read_dir(dir)? {
            let name = e?.file_name();
            let n = name.to_string_lossy();
            if ["manifest", "history", "pages", SEEN].contains(&n.as_ref()) || n.starts_with("wal-") {
                fs::rename(dir.join(&name), aside.join(&name))?;
            }
        }
        crate::disk::sync_dir(dir)?;
    }
    crate::disk::write_file(&dir.join("manifest"), &theirs)?;
    saw(dir, &theirs)?;
    Ok(set_aside)
}

fn holder(remote: &Remote) -> io::Result<Option<String>> {
    Ok(remote.get("lease")?.map(|b| String::from_utf8_lossy(&b).into_owned()))
}

/// Takes the lease for `owner`, or explains who holds it. `takeover` replaces another owner's.
/// True if `owner` held it already (from before a crash).
pub fn acquire(remote: &Remote, owner: &str, takeover: bool) -> io::Result<bool> {
    let host = std::env::var("HOSTNAME").unwrap_or_else(|_| "this machine".into());
    let me = format!("{owner}\n{host}, pid {}", std::process::id());
    if remote.put_new("lease", me.clone().into_bytes())? {
        return Ok(false);
    }
    match holder(remote)? {
        Some(h) if h.lines().next() == Some(owner) => remote.put("lease", me.into_bytes()).map(|()| true), // ours, from before a crash
        Some(_) if takeover => remote.put("lease", me.into_bytes()).map(|()| false),
        Some(h) => Err(io::Error::other(format!(
            "{} is open elsewhere ({}); if that process is gone, open with takeover",
            remote.url(),
            h.lines().nth(1).unwrap_or("unknown")
        ))),
        None => acquire(remote, owner, takeover), // released between our two requests
    }
}

/// Errors unless `owner` still holds the lease: call before publishing.
pub fn check(remote: &Remote, owner: &str) -> io::Result<()> {
    match holder(remote)? {
        Some(h) if h.lines().next() == Some(owner) => Ok(()),
        h => Err(io::Error::other(format!(
            "lost the lease on {} to {}; not publishing",
            remote.url(),
            h.as_deref().and_then(|h| h.lines().nth(1)).unwrap_or("nobody")
        ))),
    }
}

pub fn release(remote: &Remote, owner: &str) -> io::Result<()> {
    if check(remote, owner).is_ok() {
        remote.delete("lease")?;
    }
    Ok(())
}

#[cfg(all(test, feature = "s3"))]
mod tests {
    use super::*;

    /// A remote's objects are the ones directly under its prefix: a pack in a folder below it is
    /// another database's, and counting it as ours would leave our own pack of that number out of
    /// the uploads (and `evict_local` would delete it).
    #[test]
    fn list_leaves_out_folders_below_the_prefix() {
        let dir = std::env::temp_dir().join(format!("chronos-remote-list-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("db/sub")).unwrap();
        std::fs::write(dir.join("db/sub/pack-00000007"), b"another database's").unwrap();
        std::fs::write(dir.join("db/pack-00000001"), b"ours").unwrap();
        let r = Remote::new(&format!("file://{}", dir.join("db").display())).unwrap();
        assert_eq!(r.list().unwrap(), vec![("pack-00000001".to_string(), 4)]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
