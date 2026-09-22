//! Private, independent Git snapshots. The journal is durable before touching a notebook.
use super::*;
use git2::{Oid, Repository, Signature};
use std::collections::BTreeMap;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Settings {
    pub max_age_days: u32,
    pub max_revisions: usize,
    #[serde(default = "default_max_bytes")]
    pub max_bytes: u64,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            max_age_days: 30,
            max_revisions: 1000,
            max_bytes: default_max_bytes(),
        }
    }
}
fn default_max_bytes() -> u64 {
    256 * 1024 * 1024
}
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Revision {
    pub id: String,
    pub path: String,
    #[serde(rename = "notebookId")]
    pub notebook_id: String,
    pub timestamp: u64,
    pub reason: String,
    #[serde(rename = "sourceBytes")]
    pub source_bytes: u64,
    pub branch: Option<String>,
    pub head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub renamed_from: Option<String>,
}
#[derive(Default, Serialize, Deserialize)]
struct Catalog {
    revisions: Vec<Revision>,
    aliases: BTreeMap<String, String>,
    #[serde(default)]
    observed: BTreeMap<String, String>,
    settings: Settings,
}
#[derive(Serialize, Deserialize)]
struct Journal {
    entry: Revision,
    destination: Option<String>,
}
pub(super) struct History {
    directory: PathBuf,
    root: PathBuf,
    root_fd: OwnedFd,
}
pub(super) struct Transaction<'a> {
    history: &'a History,
    _lock: File,
    repo: Repository,
    catalog: Catalog,
}
fn err(e: impl std::fmt::Display) -> ApiError {
    ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        "history_io",
        e.to_string(),
    )
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn persist(path: &Path, bytes: &[u8]) -> Result<(), ApiError> {
    let temporary = path.with_extension(format!("tmp-{}", uuid::Uuid::new_v4()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)
        .map_err(err)?;
    file.write_all(bytes).map_err(err)?;
    file.sync_all().map_err(err)?;
    fs::rename(&temporary, path).map_err(err)?;
    File::open(path.parent().unwrap())
        .and_then(|f| f.sync_all())
        .map_err(err)
}
impl History {
    pub fn open(root: &Path) -> Result<Self, String> {
        #[cfg(target_os = "macos")]
        let base = PathBuf::from(std::env::var_os("HOME").ok_or("HOME unavailable for history")?)
            .join("Library/Application Support");
        #[cfg(not(target_os = "macos"))]
        let base = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".local/state")))
            .ok_or("history state directory unavailable")?;
        Self::at(
            root,
            base.join("sidequery/sqlnb-history")
                .join(revision(root.as_os_str().as_bytes())),
        )
    }
    fn at(root: &Path, directory: PathBuf) -> Result<Self, String> {
        fs::create_dir_all(&directory).map_err(|e| e.to_string())?;
        if fs::symlink_metadata(&directory)
            .map_err(|e| e.to_string())?
            .file_type()
            .is_symlink()
        {
            return Err("history directory must not be a symlink".into());
        }
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
            .map_err(|e| e.to_string())?;
        let history = Self {
            directory,
            root: root.to_owned(),
            root_fd: open_directory(root).map_err(|error| error.to_string())?,
        };
        let lock = open_lock_file(&history.directory.join("lock"))?;
        lock_file(&lock, libc::LOCK_EX, "notebook history")?;
        let repo = Repository::init_bare(history.directory.join("snapshots.git"))
            .map_err(|e| e.to_string())?;
        repo.config()
            .and_then(|mut c| c.set_bool("core.fsyncObjectFiles", true))
            .map_err(|e| e.to_string())?;
        Ok(history)
    }
    fn disk_hash(&self, path: &str) -> Result<Option<String>, ApiError> {
        let path = validate_relative_path(path)?;
        let mut components: Vec<_> = path.components().collect();
        let Some(Component::Normal(name)) = components.pop() else {
            return Err(invalid_path());
        };
        let name = cstring_component(name)?;
        let duplicate = unsafe { libc::dup(self.root_fd.as_raw_fd()) };
        if duplicate < 0 {
            return Err(file_io(std::io::Error::last_os_error()));
        }
        let mut directory = unsafe { OwnedFd::from_raw_fd(duplicate) };
        for component in components {
            let Component::Normal(component) = component else {
                return Err(invalid_path());
            };
            match open_directory_at(directory.as_raw_fd(), &cstring_component(component)?) {
                Ok(next) => directory = next,
                Err(error) if error.raw_os_error() == Some(libc::ENOENT) => return Ok(None),
                Err(error) => return Err(file_io(error)),
            }
        }
        let file = match open_file_at(directory.as_raw_fd(), &name) {
            Ok(file) => file,
            Err(error) if error.raw_os_error() == Some(libc::ENOENT) => return Ok(None),
            Err(error) => return Err(file_io(error)),
        };
        let metadata = file.metadata().map_err(file_io)?;
        if !metadata.is_file() || metadata.len() > MAX_NOTEBOOK_BYTES as u64 {
            return Err(err("cannot reconcile non-regular or oversized notebook"));
        }
        let mut bytes = Vec::new();
        file.take((MAX_NOTEBOOK_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(file_io)?;
        if bytes.len() > MAX_NOTEBOOK_BYTES {
            return Err(err("cannot reconcile oversized notebook"));
        }
        Ok(Some(revision(&bytes)))
    }
    pub fn lock(&self) -> Result<Transaction<'_>, ApiError> {
        let lock = open_lock_file(&self.directory.join("lock")).map_err(err)?;
        lock_file(&lock, libc::LOCK_EX, "notebook history").map_err(err)?;
        let path = self.directory.join("catalog.json");
        let catalog = if path.exists() {
            serde_json::from_slice(&fs::read(path).map_err(err)?).map_err(err)?
        } else {
            Catalog::default()
        };
        Ok(Transaction {
            history: self,
            _lock: lock,
            repo: Repository::open_bare(self.directory.join("snapshots.git")).map_err(err)?,
            catalog,
        })
    }
}
impl Transaction<'_> {
    fn save(&self) -> Result<(), ApiError> {
        persist(
            &self.history.directory.join("catalog.json"),
            &serde_json::to_vec(&self.catalog).map_err(err)?,
        )
    }
    fn lineage(&self, path: &str) -> String {
        self.catalog
            .aliases
            .get(path)
            .cloned()
            .unwrap_or_else(|| path.to_owned())
    }
    pub fn list(&self, path: &str) -> Vec<Revision> {
        let lineage = self.lineage(path);
        self.catalog
            .revisions
            .iter()
            .rev()
            .filter(|r| r.notebook_id == lineage)
            .cloned()
            .collect()
    }
    pub fn retained_bytes(&self) -> u64 {
        self.catalog
            .revisions
            .iter()
            .map(|entry| entry.source_bytes)
            .sum()
    }
    pub fn settings(&self) -> Settings {
        self.catalog.settings.clone()
    }
    pub fn read(&self, path: &str, id: &str) -> Result<Vec<u8>, ApiError> {
        if id.len() != 40
            || !id.bytes().all(|b| b.is_ascii_hexdigit())
            || !self.list(path).iter().any(|r| r.id == id)
        {
            return Err(ApiError::new(
                StatusCode::NOT_FOUND,
                "not_found",
                "history revision does not exist for this notebook",
            ));
        }
        self.bytes(id)
    }
    fn bytes(&self, id: &str) -> Result<Vec<u8>, ApiError> {
        let commit = self
            .repo
            .find_commit(Oid::from_str(id).map_err(err)?)
            .map_err(err)?;
        let tree = commit.tree().map_err(err)?;
        let entry = tree
            .get_name("source")
            .ok_or_else(|| err("missing snapshot source"))?;
        Ok(self
            .repo
            .find_blob(entry.id())
            .map_err(err)?
            .content()
            .to_vec())
    }
    fn snapshot(
        &self,
        path: &str,
        bytes: &[u8],
        reason: &str,
        renamed_from: Option<String>,
    ) -> Result<Revision, ApiError> {
        let project = Repository::discover(&self.history.root).ok();
        let head_ref = project.as_ref().and_then(|r| r.head().ok());
        let branch = head_ref
            .as_ref()
            .filter(|h| h.is_branch())
            .and_then(|h| h.shorthand())
            .map(str::to_owned);
        let head = head_ref
            .as_ref()
            .and_then(|h| h.target())
            .map(|h| h.to_string());
        let blob = self.repo.blob(bytes).map_err(err)?;
        let mut builder = self.repo.treebuilder(None).map_err(err)?;
        builder.insert("source", blob, 0o100644).map_err(err)?;
        let tree_id = builder.write().map_err(err)?;
        let tree = self.repo.find_tree(tree_id).map_err(err)?;
        let signature = Signature::now("SQLNB History", "history@localhost").map_err(err)?;
        let message = format!("{reason}: {path}\n{}", uuid::Uuid::new_v4());
        let id = self
            .repo
            .commit(None, &signature, &signature, &message, &tree, &[])
            .map_err(err)?;
        self.repo
            .reference(&format!("refs/snapshots/{id}"), id, false, "snapshot")
            .map_err(err)?;
        // Explicitly sync object and reference directories as well as files: a durable
        // journal must never name an object lost with an unsynced directory entry.
        let objects = self.repo.path().join("objects");
        for object_id in [blob, tree_id, id] {
            let hex = object_id.to_string();
            let directory = objects.join(&hex[..2]);
            let object = directory.join(&hex[2..]);
            if object.exists() {
                File::open(object)
                    .and_then(|file| file.sync_all())
                    .map_err(err)?;
                File::open(directory)
                    .and_then(|file| file.sync_all())
                    .map_err(err)?;
            }
        }
        for path in [
            objects,
            self.repo.path().join(format!("refs/snapshots/{id}")),
            self.repo.path().join("refs/snapshots"),
            self.repo.path().join("refs"),
            self.repo.path().to_owned(),
        ] {
            File::open(path)
                .and_then(|file| file.sync_all())
                .map_err(err)?;
        }
        Ok(Revision {
            id: id.to_string(),
            path: path.to_owned(),
            notebook_id: renamed_from
                .as_deref()
                .map(|old| self.lineage(old))
                .unwrap_or_else(|| self.lineage(path)),
            timestamp: now(),
            reason: reason.into(),
            source_bytes: bytes.len() as u64,
            branch,
            head,
            renamed_from,
        })
    }
    pub fn capture(&mut self, path: &str, bytes: &[u8], reason: &str) -> Result<(), ApiError> {
        self.observe(path, bytes, reason)?;
        self.prune()
    }
    pub fn observe(&mut self, path: &str, bytes: &[u8], reason: &str) -> Result<(), ApiError> {
        let hash = revision(bytes);
        // Observation state survives retention. Otherwise a folder larger than the
        // retention budget would continually recapture unchanged files and repack.
        if reason == "external" && self.catalog.observed.get(path) == Some(&hash) {
            return Ok(());
        }
        if let Some(last) = self.list(path).first() {
            if self.bytes(&last.id)? == bytes {
                if self.catalog.observed.get(path) != Some(&hash) {
                    self.catalog.observed.insert(path.to_owned(), hash);
                    self.save()?;
                }
                return Ok(());
            }
        }
        let entry = self.snapshot(path, bytes, reason, None)?;
        self.catalog.revisions.push(entry);
        self.catalog.observed.insert(path.to_owned(), hash);
        self.save()
    }
    pub fn enforce_retention(&mut self) -> Result<(), ApiError> {
        self.prune()
    }
    pub fn prepare(
        &mut self,
        path: &str,
        bytes: &[u8],
        reason: &str,
        destination: Option<&str>,
    ) -> Result<(), ApiError> {
        let entry = self.snapshot(
            destination.unwrap_or(path),
            bytes,
            reason,
            destination.map(|_| path.to_owned()),
        )?;
        persist(
            &self.history.directory.join("journal.json"),
            &serde_json::to_vec(&Journal {
                entry,
                destination: destination.map(str::to_owned),
            })
            .map_err(err)?,
        )
    }
    /// A prepared snapshot survives even when a process dies after rename and before finalization.
    /// Recovery retains ambiguous intents explicitly instead of guessing whether the write happened.
    pub fn finish(&mut self, recovered: bool) -> Result<(), ApiError> {
        let journal = self.history.directory.join("journal.json");
        if !journal.exists() {
            return Ok(());
        }
        let mut pending: Journal =
            serde_json::from_slice(&fs::read(&journal).map_err(err)?).map_err(err)?;
        // Catalog publication and journal removal are separate durable steps. Once the
        // entry exists, its aliases are already committed: replay must not rotate them.
        if !self
            .catalog
            .revisions
            .iter()
            .any(|entry| entry.id == pending.entry.id)
        {
            let source_hash = revision(&self.bytes(&pending.entry.id)?);
            if recovered {
                pending.entry.reason = "interrupted-write".into();
            }
            if let Some(old) = pending.entry.renamed_from.clone() {
                let moved = !recovered
                    || (self.history.disk_hash(&old)?.is_none()
                        && self.history.disk_hash(&pending.entry.path)?.as_ref()
                            == Some(&source_hash));
                if moved {
                    self.catalog.aliases.insert(
                        pending.entry.path.clone(),
                        pending.entry.notebook_id.clone(),
                    );
                    self.catalog
                        .aliases
                        .insert(old.clone(), uuid::Uuid::new_v4().to_string());
                    self.catalog.observed.remove(&old);
                } else {
                    // Prepared rename does not prove filesystem movement. Keep its source
                    // snapshot without assigning another notebook's destination lineage.
                    pending.entry.reason = "interrupted-rename".into();
                    pending.entry.path = old;
                    pending.entry.renamed_from = None;
                }
            }
            self.catalog
                .observed
                .insert(pending.entry.path.clone(), source_hash);
            self.catalog.revisions.push(pending.entry);
            self.save()?;
        }
        fs::remove_file(journal).map_err(err)?;
        File::open(&self.history.directory)
            .and_then(|f| f.sync_all())
            .map_err(err)?;
        self.prune()
    }
    pub fn cancel(&mut self) -> Result<(), ApiError> {
        let path = self.history.directory.join("journal.json");
        if path.exists() {
            let journal: Journal =
                serde_json::from_slice(&fs::read(&path).map_err(err)?).map_err(err)?;
            self.repo
                .find_reference(&format!("refs/snapshots/{}", journal.entry.id))
                .and_then(|mut r| r.delete())
                .map_err(err)?;
            fs::remove_file(path).map_err(err)?;
        }
        Ok(())
    }
    pub fn configure(&mut self, settings: Settings) -> Result<(), ApiError> {
        self.finish(true)?;
        if !(16 * 1024 * 1024..=1024 * 1024 * 1024 * 1024).contains(&settings.max_bytes)
            || !(1..=36500).contains(&settings.max_age_days)
            || !(1..=100000).contains(&settings.max_revisions)
        {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "invalid_settings",
                "maxAgeDays must be 1–36500, maxRevisions 1–100000 and maxBytes 16 MiB–1 TiB",
            ));
        }
        self.catalog.settings = settings;
        self.save()?;
        self.prune()
    }
    fn prune(&mut self) -> Result<(), ApiError> {
        let cutoff = now().saturating_sub(u64::from(self.catalog.settings.max_age_days) * 86400);
        let excess = self
            .catalog
            .revisions
            .len()
            .saturating_sub(self.catalog.settings.max_revisions);
        let old = self.catalog.revisions.clone();
        self.catalog.revisions = old
            .iter()
            .enumerate()
            .filter(|(i, r)| *i >= excess && r.timestamp >= cutoff)
            .map(|(_, r)| r.clone())
            .collect();
        // Byte budget counts exact source bytes (before Git compression). Keep the newest
        // snapshot even when older on-disk versions exceed a newly reduced budget.
        let mut bytes = 0u64;
        let mut retained = Vec::new();
        for entry in self.catalog.revisions.iter().rev() {
            let size = entry.source_bytes;
            if !retained.is_empty() && bytes.saturating_add(size) > self.catalog.settings.max_bytes
            {
                break;
            }
            bytes = bytes.saturating_add(size);
            retained.push(entry.clone());
        }
        retained.reverse();
        self.catalog.revisions = retained;
        let orphan_refs: Vec<_> = self
            .repo
            .references_glob("refs/snapshots/*")
            .map_err(err)?
            .filter_map(Result::ok)
            .filter(|reference| {
                reference.target().is_some_and(|id| {
                    !self
                        .catalog
                        .revisions
                        .iter()
                        .any(|entry| entry.id == id.to_string())
                })
            })
            .filter_map(|reference| reference.name().map(str::to_owned))
            .collect();
        if old.len() == self.catalog.revisions.len() && orphan_refs.is_empty() {
            return Ok(());
        }
        self.save()?;
        for name in orphan_refs {
            self.repo
                .find_reference(&name)
                .and_then(|mut reference| reference.delete())
                .map_err(err)?;
        }
        // Standalone commits ensure retained revisions do not keep old history reachable.
        let objects = self.repo.path().join("objects");
        let packs = objects.join("pack");
        let previous: Vec<_> = fs::read_dir(&packs)
            .map_err(err)?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .collect();
        let mut builder = self.repo.packbuilder().map_err(err)?;
        for entry in &self.catalog.revisions {
            builder
                .insert_commit(Oid::from_str(&entry.id).map_err(err)?)
                .map_err(err)?;
        }
        if !self.catalog.revisions.is_empty() {
            builder.write(&packs, 0o600).map_err(err)?;
        }
        for entry in fs::read_dir(&packs).map_err(err)? {
            File::open(entry.map_err(err)?.path())
                .and_then(|f| f.sync_all())
                .map_err(err)?;
        }
        File::open(&packs).and_then(|f| f.sync_all()).map_err(err)?;
        // All live objects are durably packed. Remove loose objects and superseded packs only.
        for entry in fs::read_dir(&objects).map_err(err)? {
            let entry = entry.map_err(err)?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.len() == 2 && name.bytes().all(|b| b.is_ascii_hexdigit()) {
                fs::remove_dir_all(entry.path()).map_err(err)?;
            }
        }
        let pack_name = builder.name().map(|s| format!("pack-{s}"));
        for path in previous {
            if pack_name
                .as_ref()
                .is_none_or(|name| path.file_stem().and_then(|s| s.to_str()) != Some(name))
            {
                fs::remove_file(path).map_err(err)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (tempfile::TempDir, NotebookStore) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("notebooks");
        fs::create_dir(&root).unwrap();
        let root_fd = open_directory(&root).unwrap();
        let history = History::at(&root, temp.path().join("private")).unwrap();
        (
            temp,
            NotebookStore {
                root,
                root_fd,
                writes: Mutex::new(()),
                recent_path: None,
                history,
                reconciliation_error: Mutex::new(None),
            },
        )
    }
    #[test]
    fn internal_states_noops_restore_conflict_and_independent_git() {
        let (_temp, store) = fixture();
        let path = Path::new("one.sqlnb");
        let a = store.write(path, b"A", "").unwrap();
        let b = store.write(path, b"B", &a).unwrap();
        store.write(path, b"A", &b).unwrap();
        store.write(path, b"A", &a).unwrap();
        let transaction = store.history.lock().unwrap();
        let entries = transaction.list("one.sqlnb");
        assert_eq!(entries.len(), 3);
        assert_eq!(transaction.read("one.sqlnb", &entries[0].id).unwrap(), b"A");
        assert_eq!(transaction.read("one.sqlnb", &entries[1].id).unwrap(), b"B");
        let id = entries[1].id.clone();
        drop(transaction);
        assert_eq!(
            store
                .write_reason(path, b"B", "stale", "restore")
                .unwrap_err()
                .status,
            StatusCode::CONFLICT
        );
        let source = store
            .history
            .lock()
            .unwrap()
            .read("one.sqlnb", &id)
            .unwrap();
        store.write_reason(path, &source, &a, "restore").unwrap();
        assert_eq!(
            store.history.lock().unwrap().list("one.sqlnb")[0].reason,
            "restore"
        );
        assert!(!store.root.join(".git").exists());
        let transaction = store.history.lock().unwrap();
        for entry in transaction.list("one.sqlnb") {
            assert_eq!(
                transaction
                    .repo
                    .find_commit(Oid::from_str(&entry.id).unwrap())
                    .unwrap()
                    .parent_count(),
                0
            );
        }
    }
    #[test]
    fn baseline_external_edit_rename_and_scope() {
        let (_temp, store) = fixture();
        fs::write(store.root.join("one.sql"), b"baseline\r\n").unwrap();
        store
            .write(Path::new("one.sql"), b"saved", &revision(b"baseline\r\n"))
            .unwrap();
        fs::write(store.root.join("one.sql"), b"external").unwrap();
        store.reconcile().unwrap();
        store
            .move_file(
                Path::new("one.sql"),
                Path::new("two.sql"),
                Some(&revision(b"external")),
            )
            .unwrap();
        let transaction = store.history.lock().unwrap();
        let revisions = transaction.list("two.sql");
        assert_eq!(revisions.len(), 4);
        assert_eq!(revisions[0].renamed_from.as_deref(), Some("one.sql"));
        assert_eq!(
            transaction.read("two.sql", &revisions[3].id).unwrap(),
            b"baseline\r\n"
        );
        assert!(transaction.read("another.sql", &revisions[0].id).is_err());
        let (_other, other) = fixture();
        assert!(other.history.lock().unwrap().list("two.sql").is_empty());
    }
    #[test]
    fn journal_recovery_is_explicit_and_retention_truncates_objects() {
        let (_temp, store) = fixture();
        let path = Path::new("one.sql");
        store.write(path, b"A", "").unwrap();
        {
            let mut transaction = store.history.lock().unwrap();
            transaction.prepare("one.sql", b"B", "save", None).unwrap();
        }
        fs::write(store.root.join(path), b"B").unwrap();
        {
            let mut transaction = store.history.lock().unwrap();
            transaction.finish(true).unwrap();
            assert_eq!(transaction.list("one.sql")[0].reason, "interrupted-write");
        }
        store.write(path, b"C", &revision(b"B")).unwrap();
        let mut transaction = store.history.lock().unwrap();
        let old = transaction.list("one.sql")[2].id.clone();
        transaction
            .configure(Settings {
                max_age_days: 30,
                max_revisions: 1,
                max_bytes: default_max_bytes(),
            })
            .unwrap();
        let remaining = transaction.list("one.sql");
        assert_eq!(remaining.len(), 1);
        assert_eq!(transaction.read("one.sql", &remaining[0].id).unwrap(), b"C");
        drop(transaction);
        let reopened = store.history.lock().unwrap();
        assert!(reopened
            .repo
            .find_commit(Oid::from_str(&old).unwrap())
            .is_err());
        assert!(
            reopened
                .repo
                .path()
                .join("objects/pack")
                .read_dir()
                .unwrap()
                .count()
                >= 2
        );
    }
    #[test]
    fn recreating_a_renamed_path_starts_a_new_lineage() {
        let (_temp, store) = fixture();
        store.write(Path::new("old.sql"), b"original", "").unwrap();
        store
            .move_file(Path::new("old.sql"), Path::new("new.sql"), None)
            .unwrap();
        store
            .write(Path::new("old.sql"), b"new notebook", "")
            .unwrap();
        let transaction = store.history.lock().unwrap();
        assert_eq!(transaction.list("old.sql").len(), 1);
        assert_eq!(transaction.list("new.sql").len(), 2);
        assert!(transaction
            .read("old.sql", &transaction.list("new.sql")[0].id)
            .is_err());
    }

    #[test]
    fn reconciliation_does_not_recapture_evicted_unchanged_files() {
        let (_temp, store) = fixture();
        store
            .history
            .lock()
            .unwrap()
            .configure(Settings {
                max_age_days: 30,
                max_revisions: 2,
                max_bytes: default_max_bytes(),
            })
            .unwrap();
        for index in 0..8 {
            fs::write(
                store.root.join(format!("{index}.sql")),
                format!("select {index}"),
            )
            .unwrap();
        }
        store.reconcile().unwrap();
        let before = fs::read(store.history.directory.join("catalog.json")).unwrap();
        let refs_before: Vec<_> = store
            .history
            .lock()
            .unwrap()
            .catalog
            .revisions
            .iter()
            .map(|r| r.id.clone())
            .collect();
        for _ in 0..3 {
            store.reconcile().unwrap();
        }
        assert_eq!(
            before,
            fs::read(store.history.directory.join("catalog.json")).unwrap()
        );
        assert_eq!(
            refs_before,
            store
                .history
                .lock()
                .unwrap()
                .catalog
                .revisions
                .iter()
                .map(|r| r.id.clone())
                .collect::<Vec<_>>()
        );
        // An internal overwrite still captures the evicted on-disk baseline.
        store
            .write(Path::new("0.sql"), b"changed", &revision(b"select 0"))
            .unwrap();
        let transaction = store.history.lock().unwrap();
        let entries = transaction.list("0.sql");
        assert_eq!(entries.len(), 2);
        assert_eq!(
            transaction.read("0.sql", &entries[1].id).unwrap(),
            b"select 0"
        );
    }

    #[test]
    fn age_and_source_byte_limits_remove_oldest_snapshots() {
        let (_temp, store) = fixture();
        let mut transaction = store.history.lock().unwrap();
        transaction.capture("old.sql", b"old", "external").unwrap();
        transaction.catalog.revisions[0].timestamp = 1;
        transaction.save().unwrap();
        transaction
            .configure(Settings {
                max_age_days: 1,
                max_revisions: 100,
                max_bytes: 16 * 1024 * 1024,
            })
            .unwrap();
        assert!(transaction.list("old.sql").is_empty());
        transaction
            .capture("large.sql", &vec![b'A'; 10 * 1024 * 1024], "save")
            .unwrap();
        transaction
            .capture("large.sql", &vec![b'B'; 10 * 1024 * 1024], "save")
            .unwrap();
        assert_eq!(transaction.list("large.sql").len(), 1);
        let entry = &transaction.list("large.sql")[0];
        assert_eq!(transaction.read("large.sql", &entry.id).unwrap()[0], b'B');
    }

    #[test]
    fn history_preserves_project_index_head_and_records_metadata() {
        let (_temp, store) = fixture();
        let repo = Repository::init(&store.root).unwrap();
        fs::write(store.root.join("one.sql"), b"original").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("one.sql")).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let signature = Signature::now("Test", "test@localhost").unwrap();
        let head = repo
            .commit(Some("HEAD"), &signature, &signature, "initial", &tree, &[])
            .unwrap();
        let index_before = fs::read(repo.path().join("index")).unwrap();
        store
            .write(Path::new("one.sql"), b"saved", &revision(b"original"))
            .unwrap();
        assert_eq!(fs::read(repo.path().join("index")).unwrap(), index_before);
        assert_eq!(repo.head().unwrap().target(), Some(head));
        let entry = store.history.lock().unwrap().list("one.sql")[0].clone();
        assert_eq!(entry.head.as_deref(), Some(head.to_string().as_str()));
        assert!(entry.branch.is_some());
    }

    #[test]
    fn unchanged_save_of_unobserved_file_does_not_create_revision() {
        let (_temp, store) = fixture();
        fs::write(store.root.join("one.sql"), b"same").unwrap();
        store
            .write(Path::new("one.sql"), b"same", &revision(b"same"))
            .unwrap();
        assert!(store.history.lock().unwrap().list("one.sql").is_empty());
    }

    #[test]
    fn rename_recovery_is_idempotent_and_keeps_recreated_source_separate() {
        for published_catalog in [false, true] {
            let (_temp, store) = fixture();
            store.write(Path::new("old.sql"), b"original", "").unwrap();
            let journal_bytes;
            {
                let mut transaction = store.history.lock().unwrap();
                transaction
                    .prepare("old.sql", b"original", "rename", Some("new.sql"))
                    .unwrap();
                journal_bytes = fs::read(store.history.directory.join("journal.json")).unwrap();
                fs::rename(store.root.join("old.sql"), store.root.join("new.sql")).unwrap();
                if published_catalog {
                    transaction.finish(false).unwrap();
                }
            }
            // Reintroducing the same journal models the save-catalog/remove-journal boundary.
            if published_catalog {
                persist(
                    &store.history.directory.join("journal.json"),
                    &journal_bytes,
                )
                .unwrap();
            }
            store.history.lock().unwrap().finish(true).unwrap();
            store
                .write(Path::new("old.sql"), b"new notebook", "")
                .unwrap();
            let transaction = store.history.lock().unwrap();
            assert_eq!(transaction.list("old.sql").len(), 1);
            assert_eq!(transaction.list("new.sql").len(), 2);
            assert!(transaction
                .read("old.sql", &transaction.list("new.sql")[0].id)
                .is_err());
        }
    }

    #[test]
    fn unperformed_rename_does_not_reassign_existing_destination_history() {
        let (_temp, store) = fixture();
        store.write(Path::new("old.sql"), b"original", "").unwrap();
        store
            .write(Path::new("existing.sql"), b"unrelated", "")
            .unwrap();
        let existing_id = store.history.lock().unwrap().list("existing.sql")[0]
            .id
            .clone();
        {
            let mut transaction = store.history.lock().unwrap();
            transaction
                .prepare("old.sql", b"original", "rename", Some("existing.sql"))
                .unwrap();
        }
        store.history.lock().unwrap().finish(true).unwrap();
        let transaction = store.history.lock().unwrap();
        assert_eq!(transaction.list("existing.sql").len(), 1);
        assert_eq!(transaction.list("existing.sql")[0].id, existing_id);
        assert_eq!(transaction.list("old.sql")[0].reason, "interrupted-rename");
    }

    #[test]
    fn git_metadata_is_excluded_from_listing_and_reconciliation() {
        let (_temp, store) = fixture();
        fs::create_dir_all(store.root.join(".git/hooks")).unwrap();
        fs::write(
            store.root.join(".git/hooks/private.sql"),
            b"private metadata",
        )
        .unwrap();
        fs::write(store.root.join("one.sql"), b"source").unwrap();
        let listing = store.list().unwrap();
        assert_eq!(listing.files.len(), 1);
        assert!(listing.directories.is_empty());
        store.reconcile().unwrap();
        assert!(store
            .history
            .lock()
            .unwrap()
            .list(".git/hooks/private.sql")
            .is_empty());
        for path in [".git/hooks/private.sql", "nested/.GIT/private.sql"] {
            assert!(validate_relative_path(path).is_err());
        }
        assert!(validate_folder_path(".git").is_err());
    }

    #[test]
    fn symlink_never_enters_history() {
        let (temp, store) = fixture();
        let outside = temp.path().join("outside.sql");
        fs::write(&outside, b"private").unwrap();
        std::os::unix::fs::symlink(outside, store.root.join("linked.sql")).unwrap();
        store.reconcile().unwrap();
        assert!(store.history.lock().unwrap().list("linked.sql").is_empty());
        assert!(store
            .write(
                Path::new("linked.sql"),
                b"replacement",
                &revision(b"private")
            )
            .is_err());
    }
}
