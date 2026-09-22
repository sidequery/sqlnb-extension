//! Explicit, notebook-scoped access to the user's Git repository.
//!
//! All writes reserve Git's real index lock. An alternate index lets commits use
//! selected staged bytes without including unrelated staging or unstaged edits.
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    ffi::OsString,
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

const OUTPUT_LIMIT: usize = 4 * 1024 * 1024;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct GitStatus {
    pub available: bool,
    pub root: Option<String>,
    pub branch: Option<String>,
    pub head: Option<String>,
    pub upstream: Option<String>,
    pub conflicted: bool,
    pub index_revision: Option<String>,
    pub files: Vec<GitFile>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}
#[derive(Serialize, Debug)]
pub struct GitFile {
    pub path: String,
    pub index: String,
    pub worktree: String,
}

pub struct ProjectGit {
    root: PathBuf,
}
struct Repository {
    root: PathBuf,
    index: PathBuf,
    prefix: PathBuf,
}
struct Output {
    success: bool,
    code: Option<i32>,
    bytes: Vec<u8>,
}

impl ProjectGit {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn discover(&self) -> Result<Option<Repository>, String> {
        let out = run(&self.root, None, &["rev-parse", "--show-toplevel"], None)?;
        if !out.success {
            return Ok(None);
        }
        let root = PathBuf::from(line(&out.bytes)?);
        let root = root
            .canonicalize()
            .map_err(|_| "Cannot resolve repository root")?;
        let notebook_root = self
            .root
            .canonicalize()
            .map_err(|_| "Cannot resolve notebook root")?;
        let prefix = notebook_root
            .strip_prefix(&root)
            .map_err(|_| "Repository is outside notebook scope")?
            .to_owned();
        let index = checked(
            &self.root,
            None,
            &["rev-parse", "--path-format=absolute", "--git-path", "index"],
            None,
        )?;
        Ok(Some(Repository {
            root,
            index: PathBuf::from(line(&index)?),
            prefix,
        }))
    }

    fn repository(&self) -> Result<Repository, String> {
        self.discover()?
            .ok_or_else(|| "Notebook folder is not in a Git repository".into())
    }

    pub fn status(&self) -> Result<GitStatus, String> {
        let mut status = GitStatus {
            available: true,
            root: None,
            branch: None,
            head: None,
            upstream: None,
            conflicted: false,
            index_revision: None,
            files: vec![],
            error: None,
        };
        let repo = match self.discover() {
            Ok(Some(repo)) => repo,
            Ok(None) => return Ok(status),
            Err(error) if error == "Git executable is unavailable" => {
                status.available = false;
                status.error = Some(error);
                return Ok(status);
            }
            Err(error) => return Err(error),
        };
        status.root = Some(repo.root.to_string_lossy().into_owned());
        status.head = head(&repo)?;
        status.index_revision = Some(index_revision(&read_index(&repo.index)?));
        status.branch = optional(&repo, &["symbolic-ref", "--quiet", "--short", "HEAD"])?;
        status.upstream = optional(
            &repo,
            &[
                "rev-parse",
                "--abbrev-ref",
                "--symbolic-full-name",
                "@{upstream}",
            ],
        )?;
        status.conflicted = conflicts(&repo)?;
        let bytes = checked(
            &repo.root,
            None,
            &[
                "status",
                "--porcelain=v1",
                "-z",
                "--untracked-files=all",
                "--no-renames",
            ],
            None,
        )?;
        for entry in bytes.split(|b| *b == 0).filter(|entry| !entry.is_empty()) {
            if entry.len() < 4 {
                return Err("Invalid Git status response".into());
            }
            let path = std::str::from_utf8(&entry[3..]).map_err(|_| "Git paths must be UTF-8")?;
            let Ok(scoped) = Path::new(path).strip_prefix(&repo.prefix) else {
                continue;
            };
            let Some(scoped) = scoped.to_str() else {
                continue;
            };
            if self.validate_path(scoped).is_err() {
                continue;
            }
            status.files.push(GitFile {
                path: scoped.to_owned(),
                index: (entry[0] as char).to_string(),
                worktree: (entry[1] as char).to_string(),
            });
        }
        Ok(status)
    }

    pub fn diff(&self, path: &str, staged: bool) -> Result<String, String> {
        self.validate_path(path)?;
        let repo = self.repository()?;
        let relative = repo_path(&repo, path)?;
        let mut args = vec!["diff", "--no-ext-diff", "--no-textconv", "--no-color"];
        if staged {
            args.push("--cached");
        }
        args.extend(["--", relative.as_str()]);
        let mut bytes = checked(&repo.root, None, &args, None)?;
        if !staged && bytes.is_empty() && self.root.join(path).is_file() {
            let tracked = checked(&repo.root, None, &["ls-files", "-z", "--", &relative], None)?;
            if tracked.is_empty() {
                // --no-index returns 1 for a valid diff, just like diff(1).
                let output = run(
                    &repo.root,
                    None,
                    &[
                        "diff",
                        "--no-index",
                        "--no-ext-diff",
                        "--no-textconv",
                        "--no-color",
                        "--",
                        "/dev/null",
                        &relative,
                    ],
                    None,
                )?;
                if output.code != Some(0) && output.code != Some(1) {
                    return Err("Cannot read untracked notebook diff".into());
                }
                bytes = output.bytes;
            }
        }
        String::from_utf8(bytes).map_err(|_| "Notebook diff is not UTF-8".into())
    }

    pub fn stage(
        &self,
        paths: &[String],
        expected_head: Option<&str>,
        expected_index: &str,
    ) -> Result<(), String> {
        self.change_index(paths, expected_head, expected_index, true)
    }

    pub fn unstage(
        &self,
        paths: &[String],
        expected_head: Option<&str>,
        expected_index: &str,
    ) -> Result<(), String> {
        self.change_index(paths, expected_head, expected_index, false)
    }

    fn change_index(
        &self,
        paths: &[String],
        expected_head: Option<&str>,
        expected_index: &str,
        stage: bool,
    ) -> Result<(), String> {
        let repo = self.repository()?;
        let paths = self.paths(&repo, paths)?;
        let mut lock = IndexLock::acquire(&repo)?;
        lock.ensure_revision(expected_index)?;
        ensure_head(&repo, expected_head)?;
        ensure_no_conflicts(&repo)?;
        lock.copy_index(&repo)?;
        let mut args = if stage {
            vec!["add", "--all"]
        } else if expected_head.is_some() {
            vec!["reset", "--quiet", "HEAD"]
        } else {
            vec!["update-index", "--force-remove"]
        };
        args.push("--");
        args.extend(paths.iter().map(String::as_str));
        checked(&repo.root, Some(&lock.temporary), &args, None)?;
        ensure_head(&repo, expected_head)?;
        // Revalidate after Git reads the working tree, before publishing its index.
        for path in paths.iter() {
            let scoped = Path::new(path)
                .strip_prefix(&repo.prefix)
                .map_err(|_| "Invalid scope")?;
            self.validate_path(scoped.to_str().ok_or("Invalid path")?)?;
        }
        lock.publish(&repo)
    }

    pub fn commit(
        &self,
        message: &str,
        paths: &[String],
        expected_head: Option<&str>,
        expected_index: &str,
    ) -> Result<String, String> {
        if message.trim().is_empty() || message.len() > 64 * 1024 || message.contains('\0') {
            return Err("Commit message must be nonempty and at most 64 KiB".into());
        }
        let repo = self.repository()?;
        let paths = self.paths(&repo, paths)?;
        let mut lock = IndexLock::acquire(&repo)?;
        lock.ensure_revision(expected_index)?;
        ensure_head(&repo, expected_head)?;
        ensure_no_conflicts(&repo)?;
        let original_staged = checked(
            &repo.root,
            None,
            &[
                "diff",
                "--cached",
                "--name-only",
                "--no-renames",
                "--ita-visible-in-index",
                "-z",
            ],
            None,
        )?;
        if let Some(head) = expected_head {
            checked(
                &repo.root,
                Some(&lock.temporary),
                &["read-tree", head],
                None,
            )?;
        } else {
            checked(
                &repo.root,
                Some(&lock.temporary),
                &["read-tree", "--empty"],
                None,
            )?;
        }
        let mut listing = vec!["ls-files", "--stage", "-z", "--"];
        listing.extend(paths.iter().map(String::as_str));
        let selected = checked(&repo.root, None, &listing, None)?;
        let mut removal = vec!["update-index", "--force-remove", "--"];
        removal.extend(paths.iter().map(String::as_str));
        checked(&repo.root, Some(&lock.temporary), &removal, None)?;
        checked(
            &repo.root,
            Some(&lock.temporary),
            &["update-index", "-z", "--index-info"],
            Some(&selected),
        )?;
        let changed = run(
            &repo.root,
            Some(&lock.temporary),
            &["diff", "--cached", "--quiet", "--exit-code"],
            None,
        )?;
        if changed.success {
            return Err("Selected paths have no staged changes".into());
        }
        ensure_head(&repo, expected_head)?;
        lock.ensure_unchanged(&repo)?;
        // Normal git commit is intentional: hooks, signing and user configuration
        // remain in force. -F - keeps user text out of command-line arguments.
        checked(
            &repo.root,
            Some(&lock.temporary),
            &["commit", "--file=-"],
            Some(message.as_bytes()),
        )
        .map_err(|error| {
            format!("{error}. A commit may have been created; inspect Git history before retrying")
        })?;
        let new_head = head(&repo)?.ok_or("Git did not create a commit")?;
        let parents = checked(
            &repo.root,
            None,
            &["rev-list", "--parents", "-n", "1", &new_head],
            None,
        )?;
        let parents = line(&parents)?;
        let actual: Vec<_> = parents.split_whitespace().skip(1).collect();
        let expected: Vec<_> = expected_head.into_iter().collect();
        if actual != expected || lock.ensure_unchanged(&repo).is_err() {
            return Err("Repository changed concurrently; a commit may have been created. Inspect Git history before retrying".into());
        }
        // Hooks are trusted user programs and may expand the commit beyond the
        // selection. Detect that after Git runs the hooks once; never undo the
        // resulting commit. Reconcile their newly committed paths, except where
        // doing so would overwrite unrelated staging that predates the operation.
        let committed = checked(
            &repo.root,
            None,
            &[
                "diff-tree",
                "--root",
                "--no-commit-id",
                "--name-only",
                "--no-renames",
                "-r",
                "-z",
                &new_head,
            ],
            None,
        )
        .map_err(|_| {
            "Commit was created, but its paths could not be checked; inspect Git before retrying"
        })?;
        let mut reconcile_paths = paths.clone();
        let mut hook_expanded_scope = false;
        let mut preserved_hook_overlap = false;
        for path in committed
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
        {
            if paths.iter().any(|selected| selected.as_bytes() == path) {
                continue;
            }
            hook_expanded_scope = true;
            if original_staged
                .split(|byte| *byte == 0)
                .any(|staged| staged == path)
            {
                preserved_hook_overlap = true;
                continue;
            }
            let path = std::str::from_utf8(path).map_err(|_| "Commit was created with a non-UTF-8 hook path; inspect Git and its index before retrying")?;
            reconcile_paths.push(path.to_owned());
        }
        let reconcile = (|| {
            lock.copy_index(&repo)?;
            let mut reset = vec!["reset", "--quiet", new_head.as_str(), "--"];
            reset.extend(reconcile_paths.iter().map(String::as_str));
            checked(&repo.root, Some(&lock.temporary), &reset, None)?;
            ensure_head(&repo, Some(&new_head))?;
            lock.publish(&repo)
        })();
        reconcile.map_err(|_: String| "Commit was created, but the index could not be reconciled; refresh and inspect Git before retrying".to_owned())?;
        if hook_expanded_scope {
            return Err(if preserved_hook_overlap {
                "Commit was created, but a Git hook included unselected paths that overlap pre-existing staging. That staging was preserved and may now differ from the commit; inspect Git history and staged changes before continuing."
            } else {
                "Commit was created, but a Git hook included unselected paths. The index was reconciled; inspect Git history before continuing."
            }.into());
        }
        Ok(new_head)
    }

    pub fn push(&self, expected_head: &str) -> Result<(), String> {
        let repo = self.repository()?;
        let _lock = IndexLock::acquire(&repo)?;
        ensure_head(&repo, Some(expected_head))?;
        ensure_no_conflicts(&repo)?;
        let branch = optional(&repo, &["symbolic-ref", "--quiet", "--short", "HEAD"])?
            .ok_or("Cannot push a detached HEAD")?;
        let remote_key = format!("branch.{branch}.remote");
        let merge_key = format!("branch.{branch}.merge");
        let remote = optional(&repo, &["config", "--get", &remote_key])?
            .ok_or("Configure an upstream before pushing")?;
        let target = optional(&repo, &["config", "--get", &merge_key])?
            .ok_or("Configure an upstream before pushing")?;
        if remote.is_empty() || !target.starts_with("refs/heads/") || target.contains('\n') {
            return Err("Invalid configured upstream".into());
        }
        let refspec = format!("{expected_head}:{target}");
        checked(
            &repo.root,
            None,
            &["push", "--porcelain", "--", &remote, &refspec],
            None,
        )?;
        ensure_head(&repo, Some(expected_head))
    }

    pub fn init(&self) -> Result<(), String> {
        if self.discover()?.is_some() {
            return Err("Notebook folder is already in a Git repository".into());
        }
        if fs::symlink_metadata(self.root.join(".git")).is_ok() {
            return Err(
                "Git metadata already exists; inspect the repository before initializing".into(),
            );
        }
        let meta = fs::symlink_metadata(&self.root).map_err(|_| "Cannot inspect notebook root")?;
        if !meta.is_dir() || meta.file_type().is_symlink() {
            return Err("Notebook root must be a real directory".into());
        }
        checked(&self.root, None, &["init", "--", "."], None)?;
        Ok(())
    }

    fn paths(&self, repo: &Repository, paths: &[String]) -> Result<Vec<String>, String> {
        if paths.is_empty() || paths.len() > 1000 {
            return Err("Select between 1 and 1000 notebook paths".into());
        }
        paths
            .iter()
            .map(|path| {
                self.validate_path(path)?;
                repo_path(repo, path)
            })
            .collect()
    }

    fn validate_path(&self, path: &str) -> Result<(), String> {
        let relative = Path::new(path);
        if path.is_empty()
            || path.contains('\0')
            || path.contains('\\')
            || path
                .split('/')
                .any(|part| matches!(part, "" | "." | ".." | ".git"))
            || relative
                .components()
                .any(|c| !matches!(c, Component::Normal(_)))
            || !matches!(
                relative.extension().and_then(|s| s.to_str()),
                Some("sql" | "sqlnb")
            )
        {
            return Err("Path must be a relative .sql or .sqlnb path without traversal".into());
        }
        let mut current = self.root.clone();
        for component in relative.components() {
            current.push(component);
            match fs::symlink_metadata(&current) {
                Ok(meta) if meta.file_type().is_symlink() => {
                    return Err("Symlink notebook paths are not allowed".into())
                }
                Ok(meta) if current == self.root.join(relative) && !meta.is_file() => {
                    return Err("Notebook must be a regular file".into())
                }
                Ok(meta) if current != self.root.join(relative) && !meta.is_dir() => {
                    return Err("Notebook parent must be a directory".into())
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return Err("Cannot inspect notebook path".into()),
            }
        }
        Ok(())
    }
}

fn repo_path(repo: &Repository, path: &str) -> Result<String, String> {
    repo.prefix
        .join(path)
        .to_str()
        .map(str::to_owned)
        .ok_or_else(|| "Git path must be UTF-8".into())
}
fn line(bytes: &[u8]) -> Result<String, String> {
    Ok(std::str::from_utf8(bytes)
        .map_err(|_| "Git response must be UTF-8")?
        .trim_end_matches('\n')
        .to_owned())
}
fn optional(repo: &Repository, args: &[&str]) -> Result<Option<String>, String> {
    let out = run(&repo.root, None, args, None)?;
    if out.success {
        Ok(Some(line(&out.bytes)?))
    } else {
        Ok(None)
    }
}
fn head(repo: &Repository) -> Result<Option<String>, String> {
    optional(repo, &["rev-parse", "--verify", "HEAD"])
}
fn ensure_head(repo: &Repository, expected: Option<&str>) -> Result<(), String> {
    if head(repo)?.as_deref() != expected {
        return Err("Repository HEAD changed; refresh before retrying".into());
    }
    Ok(())
}
fn conflicts(repo: &Repository) -> Result<bool, String> {
    Ok(!checked(&repo.root, None, &["ls-files", "--unmerged", "-z"], None)?.is_empty())
}
fn ensure_no_conflicts(repo: &Repository) -> Result<(), String> {
    if conflicts(repo)? {
        return Err("Resolve repository conflicts before continuing".into());
    }
    // Committing a subset during a merge/rebase would misrepresent the operation.
    for name in [
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "rebase-merge",
        "rebase-apply",
    ] {
        let path = checked(
            &repo.root,
            None,
            &["rev-parse", "--path-format=absolute", "--git-path", name],
            None,
        )?;
        if Path::new(&line(&path)?).exists() {
            return Err("Finish the current Git operation before continuing".into());
        }
    }
    Ok(())
}

struct IndexLock {
    path: PathBuf,
    temporary: PathBuf,
    original: Option<Vec<u8>>,
    original_modified: Option<std::time::SystemTime>,
}
impl IndexLock {
    fn acquire(repo: &Repository) -> Result<Self, String> {
        let mut name: OsString = repo.index.as_os_str().to_owned();
        name.push(".lock");
        let path = PathBuf::from(name);
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|_| "Git index is busy; retry after the other Git operation finishes")?;
        let temporary = repo
            .index
            .with_file_name(format!("notebook-index-{}", uuid::Uuid::new_v4()));
        let mut lock = Self {
            path,
            temporary,
            original: None,
            original_modified: None,
        };
        lock.original = read_index(&repo.index)?;
        lock.original_modified = fs::metadata(&repo.index)
            .ok()
            .and_then(|m| m.modified().ok());
        Ok(lock)
    }
    fn ensure_revision(&self, expected: &str) -> Result<(), String> {
        if index_revision(&self.original) != expected {
            return Err("Git index changed; refresh before retrying".into());
        }
        Ok(())
    }
    fn copy_index(&self, repo: &Repository) -> Result<(), String> {
        if let Some(bytes) = &self.original {
            fs::write(&self.temporary, bytes).map_err(|_| "Cannot prepare Git index")?;
            if let Some(modified) = self.original_modified {
                set_modified(&self.temporary, modified)?;
            }
        } else {
            checked(
                &repo.root,
                Some(&self.temporary),
                &["read-tree", "--empty"],
                None,
            )?;
        }
        Ok(())
    }
    fn ensure_unchanged(&self, repo: &Repository) -> Result<(), String> {
        if read_index(&repo.index)? != self.original {
            return Err("Git index changed concurrently; refresh before retrying".into());
        }
        Ok(())
    }
    fn publish(&mut self, repo: &Repository) -> Result<(), String> {
        self.ensure_unchanged(repo)?;
        fs::copy(&self.temporary, &self.path).map_err(|_| "Cannot prepare Git index update")?;
        let modified = fs::metadata(&self.temporary)
            .and_then(|m| m.modified())
            .map_err(|_| "Cannot inspect Git index timestamp")?;
        set_modified(&self.path, modified)?;
        fs::rename(&self.path, &repo.index).map_err(|_| "Cannot publish Git index update")?;
        Ok(())
    }
}
// Git uses the index timestamp to detect racily clean entries. Rewriting an
// identical index with a newer timestamp can hide a same-size working-tree edit.
fn set_modified(path: &Path, modified: std::time::SystemTime) -> Result<(), String> {
    fs::File::options()
        .write(true)
        .open(path)
        .and_then(|file| file.set_times(fs::FileTimes::new().set_modified(modified)))
        .map_err(|_| "Cannot preserve Git index timestamp".into())
}
impl Drop for IndexLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
        let _ = fs::remove_file(&self.temporary);
        let mut temporary_lock = self.temporary.as_os_str().to_owned();
        temporary_lock.push(".lock");
        let _ = fs::remove_file(PathBuf::from(temporary_lock));
    }
}
fn index_revision(bytes: &Option<Vec<u8>>) -> String {
    let mut digest = Sha256::new();
    digest.update([u8::from(bytes.is_some())]);
    if let Some(bytes) = bytes {
        digest.update(bytes);
    }
    format!("{:x}", digest.finalize())
}
fn read_index(path: &Path) -> Result<Option<Vec<u8>>, String> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err("Cannot read Git index".into()),
    }
}
fn checked(
    cwd: &Path,
    index: Option<&Path>,
    args: &[&str],
    input: Option<&[u8]>,
) -> Result<Vec<u8>, String> {
    let output = run(cwd, index, args, input)?;
    if output.success {
        Ok(output.bytes)
    } else {
        Err(format!(
            "Git {} failed; check repository configuration, hooks, signing, or authentication",
            args.first().unwrap_or(&"operation")
        ))
    }
}
fn bounded_read(
    mut reader: impl Read,
    finished: Arc<AtomicBool>,
    deadline: Instant,
) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut chunk = [0; 8192];
    loop {
        if Instant::now() >= deadline {
            break;
        }
        let count = match reader.read(&mut chunk) {
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if finished.load(Ordering::Acquire) {
                    break;
                }
                thread::sleep(Duration::from_millis(5));
                continue;
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        if count == 0 {
            break;
        }
        if bytes.len() <= OUTPUT_LIMIT {
            let remaining = (OUTPUT_LIMIT + 1).saturating_sub(bytes.len());
            bytes.extend_from_slice(&chunk[..count.min(remaining)]);
        }
    }
    Ok(bytes)
}
fn run(
    cwd: &Path,
    index: Option<&Path>,
    args: &[&str],
    input: Option<&[u8]>,
) -> Result<Output, String> {
    run_with_timeout(cwd, index, args, input, COMMAND_TIMEOUT)
}

#[cfg(unix)]
fn nonblocking(pipe: &impl std::os::fd::AsRawFd) -> std::io::Result<()> {
    let fd = pipe.as_raw_fd();
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

fn run_with_timeout(
    cwd: &Path,
    index: Option<&Path>,
    args: &[&str],
    input: Option<&[u8]>,
    timeout: Duration,
) -> Result<Output, String> {
    let start = Instant::now();
    let deadline = start + timeout;
    let mut command = Command::new("git");
    command
        .current_dir(cwd)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("GIT_LITERAL_PATHSPECS", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0");
    // Repository selection must come from the notebook root, never inherited
    // process variables pointing at another checkout or object database.
    for key in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_COMMON_DIR",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    ] {
        command.env_remove(key);
    }
    if let Some(index) = index {
        command.env("GIT_INDEX_FILE", index);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Descendant hooks/SSH helpers must be killed with a timed-out Git child.
        unsafe {
            command.pre_exec(|| {
                if libc::setpgid(0, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    let mut child = command.spawn().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            "Git executable is unavailable".to_owned()
        } else {
            "Cannot start Git".to_owned()
        }
    })?;
    let stdout = child.stdout.take().ok_or("Cannot read Git output")?;
    let stderr = child.stderr.take().ok_or("Cannot read Git output")?;
    let mut stdin = child.stdin.take().ok_or("Cannot write Git input")?;
    #[cfg(unix)]
    if nonblocking(&stdout)
        .and_then(|_| nonblocking(&stderr))
        .and_then(|_| nonblocking(&stdin))
        .is_err()
    {
        let _ = child.kill();
        let _ = child.wait();
        return Err("Cannot configure bounded Git pipes".into());
    }
    let finished = Arc::new(AtomicBool::new(false));
    let out_finished = finished.clone();
    let err_finished = finished.clone();
    let input_finished = finished.clone();
    let out_thread = thread::spawn(move || bounded_read(stdout, out_finished, deadline));
    let err_thread = thread::spawn(move || bounded_read(stderr, err_finished, deadline));
    let input = input.unwrap_or_default().to_vec();
    let input_thread = thread::spawn(move || {
        let mut remaining = input.as_slice();
        while !remaining.is_empty()
            && Instant::now() < deadline
            && !input_finished.load(Ordering::Acquire)
        {
            match stdin.write(remaining) {
                Ok(0) => break,
                Ok(count) => remaining = &remaining[count..],
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5))
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
    });
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if start.elapsed() < timeout => thread::sleep(Duration::from_millis(10)),
            _ => {
                #[cfg(unix)]
                unsafe {
                    libc::kill(-(child.id() as i32), libc::SIGKILL);
                }
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    // Kill any hook helper retaining the pipes after its parent exits.
    #[cfg(unix)]
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    // A daemonized helper can escape the process group. Nonblocking pipes and
    // this completion flag keep its inherited descriptors from hanging joins.
    finished.store(true, Ordering::Release);
    let bytes = out_thread
        .join()
        .map_err(|_| "Cannot read Git output")?
        .map_err(|_| "Cannot read Git output")?;
    let errors = err_thread
        .join()
        .map_err(|_| "Cannot read Git output")?
        .map_err(|_| "Cannot read Git output")?;
    let _ = input_thread.join();
    let status = status.ok_or("Git operation timed out")?;
    if bytes.len() > OUTPUT_LIMIT || errors.len() > OUTPUT_LIMIT {
        return Err("Git output exceeds the 4 MiB limit".into());
    }
    // Never forward stderr (remote URLs, credential helper diagnostics and hooks
    // may print authentication secrets).
    Ok(Output {
        success: status.success(),
        code: status.code(),
        bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn git(root: &Path, args: &[&str]) -> String {
        line(&checked(root, None, args, None).unwrap()).unwrap()
    }
    fn fixture() -> (TempDir, ProjectGit) {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "--initial-branch=main"]);
        git(dir.path(), &["config", "user.name", "Notebook Test"]);
        git(
            dir.path(),
            &["config", "user.email", "notebook@example.invalid"],
        );
        git(dir.path(), &["config", "commit.gpgsign", "false"]);
        git(dir.path(), &["config", "core.hooksPath", ".git/test-hooks"]);
        let project = ProjectGit::new(dir.path().to_owned());
        (dir, project)
    }
    fn write(root: &Path, name: &str, text: &str) {
        let path = root.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    fn stage(project: &ProjectGit, names: &[&str]) {
        let status = project.status().unwrap();
        project
            .stage(
                &names.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
                status.head.as_deref(),
                status.index_revision.as_deref().unwrap(),
            )
            .unwrap();
    }
    fn commit(project: &ProjectGit, names: &[&str]) -> Result<String, String> {
        let status = project.status().unwrap();
        project.commit(
            "Notebook change",
            &names.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            status.head.as_deref(),
            status.index_revision.as_deref().unwrap(),
        )
    }

    #[test]
    fn subprocess_timeout_is_bounded() {
        let (dir, _) = fixture();
        let start = Instant::now();
        let error = run_with_timeout(
            dir.path(),
            None,
            &["-c", "alias.wait=!sleep 5", "wait"],
            None,
            Duration::from_millis(100),
        )
        .err()
        .unwrap();
        assert!(error.contains("timed out"));
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn diffs_cover_untracked_staged_and_unstaged_content() {
        let (dir, project) = fixture();
        write(dir.path(), "a.sql", "select 1;\n");
        assert!(project.diff("a.sql", false).unwrap().contains("+select 1;"));
        stage(&project, &["a.sql"]);
        assert!(project.diff("a.sql", true).unwrap().contains("+select 1;"));
        write(dir.path(), "a.sql", "select 2;\n");
        let diff = project.diff("a.sql", false).unwrap();
        assert!(diff.contains("-select 1;"));
        assert!(diff.contains("+select 2;"));
    }

    #[test]
    fn selected_commit_preserves_unrelated_staging_and_unstaged_bytes() {
        let (dir, project) = fixture();
        write(dir.path(), "a.sqlnb", "original\n");
        write(dir.path(), "other.txt", "unrelated staged\n");
        git(dir.path(), &["add", "other.txt"]);
        stage(&project, &["a.sqlnb"]);
        write(dir.path(), "a.sqlnb", "unstaged\n");
        commit(&project, &["a.sqlnb"]).unwrap();
        assert_eq!(git(dir.path(), &["show", "HEAD:a.sqlnb"]), "original");
        assert_eq!(git(dir.path(), &["show", ":other.txt"]), "unrelated staged");
        assert!(
            !run(
                dir.path(),
                None,
                &["cat-file", "-e", "HEAD:other.txt"],
                None
            )
            .unwrap()
            .success
        );
        assert_eq!(
            git(dir.path(), &["diff", "--cached", "--name-only"]),
            "other.txt"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("a.sqlnb")).unwrap(),
            "unstaged\n"
        );
        assert_eq!(git(dir.path(), &["diff", "--name-only"]), "a.sqlnb");
    }

    #[test]
    fn unborn_unstage_and_deleted_notebook() {
        let (dir, project) = fixture();
        write(dir.path(), "a.sql", "select 1\n");
        stage(&project, &["a.sql"]);
        let status = project.status().unwrap();
        project
            .unstage(
                &["a.sql".into()],
                None,
                status.index_revision.as_deref().unwrap(),
            )
            .unwrap();
        assert!(git(dir.path(), &["ls-files"]).is_empty());
        stage(&project, &["a.sql"]);
        commit(&project, &["a.sql"]).unwrap();
        fs::remove_file(dir.path().join("a.sql")).unwrap();
        stage(&project, &["a.sql"]);
        commit(&project, &["a.sql"]).unwrap();
        assert!(git(dir.path(), &["ls-tree", "--name-only", "HEAD"]).is_empty());
    }

    #[cfg(unix)]
    fn hook(root: &Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        write(root, ".git/test-hooks/pre-commit", body);
        fs::set_permissions(
            root.join(".git/test-hooks/pre-commit"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    }
    #[test]
    #[cfg(unix)]
    fn hook_rejection_preserves_head_and_index_and_redacts_output() {
        let (dir, project) = fixture();
        write(dir.path(), "a.sqlnb", "content");
        stage(&project, &["a.sqlnb"]);
        let before = fs::read(dir.path().join(".git/index")).unwrap();
        hook(dir.path(), "#!/bin/sh\nprintf rejected > hook-added.txt\ngit add -- hook-added.txt\necho secret-token >&2\nexit 1\n");
        let error = commit(&project, &["a.sqlnb"]).unwrap_err();
        assert!(!error.contains("secret-token"));
        assert!(project.status().unwrap().head.is_none());
        assert_eq!(fs::read(dir.path().join(".git/index")).unwrap(), before);
        assert!(!dir.path().join(".git/index.lock").exists());
        assert!(fs::read_dir(dir.path().join(".git"))
            .unwrap()
            .all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("notebook-index-")));
        // A hook's working-tree edits are user-program output, not disposable
        // module artifacts. Keep them, even when the hook rejects the commit.
        assert_eq!(
            fs::read_to_string(dir.path().join("hook-added.txt")).unwrap(),
            "rejected"
        );
    }
    #[test]
    #[cfg(unix)]
    fn hook_added_unselected_path_is_reported_and_index_reconciled() {
        let (dir, project) = fixture();
        write(dir.path(), "a.sqlnb", "selected");
        stage(&project, &["a.sqlnb"]);
        hook(
            dir.path(),
            "#!/bin/sh\nprintf added > hook-added.txt\ngit add -- hook-added.txt\n",
        );
        let error = commit(&project, &["a.sqlnb"]).unwrap_err();
        assert!(error.contains("Commit was created"));
        assert!(error.contains("unselected paths"));
        assert_eq!(git(dir.path(), &["show", "HEAD:hook-added.txt"]), "added");
        assert_eq!(git(dir.path(), &["show", ":hook-added.txt"]), "added");
        assert!(git(dir.path(), &["diff", "--cached", "--name-only"]).is_empty());
        assert!(git(dir.path(), &["diff", "--name-only"]).is_empty());
    }
    #[test]
    #[cfg(unix)]
    fn hook_overlap_preserves_unrelated_staged_bytes_and_reports_conflict() {
        let (dir, project) = fixture();
        write(dir.path(), "a.sqlnb", "selected");
        write(dir.path(), "other.txt", "original staging");
        git(dir.path(), &["add", "other.txt"]);
        let staged_entry = git(dir.path(), &["ls-files", "--stage", "--", "other.txt"]);
        stage(&project, &["a.sqlnb"]);
        hook(
            dir.path(),
            "#!/bin/sh\nprintf hook-content > other.txt\ngit add -- other.txt\n",
        );
        let error = commit(&project, &["a.sqlnb"]).unwrap_err();
        assert!(error.contains("pre-existing staging"));
        assert_eq!(git(dir.path(), &["show", "HEAD:other.txt"]), "hook-content");
        assert_eq!(git(dir.path(), &["show", ":other.txt"]), "original staging");
        assert_eq!(
            git(dir.path(), &["ls-files", "--stage", "--", "other.txt"]),
            staged_entry
        );
        assert_eq!(
            git(dir.path(), &["diff", "--cached", "--name-only"]),
            "other.txt"
        );
    }
    #[test]
    #[cfg(unix)]
    fn hook_rewrite_selected_staging_reconciles_index() {
        let (dir, project) = fixture();
        write(dir.path(), "a.sqlnb", "before");
        write(dir.path(), "other.txt", "other");
        git(dir.path(), &["add", "other.txt"]);
        stage(&project, &["a.sqlnb"]);
        hook(
            dir.path(),
            "#!/bin/sh\nprintf after > a.sqlnb\ngit add -- a.sqlnb\n",
        );
        commit(&project, &["a.sqlnb"]).unwrap();
        assert_eq!(git(dir.path(), &["show", "HEAD:a.sqlnb"]), "after");
        assert_eq!(
            git(dir.path(), &["diff", "--cached", "--name-only"]),
            "other.txt"
        );
    }
    #[test]
    fn stale_head_and_index_are_rejected() {
        let (dir, project) = fixture();
        write(dir.path(), "a.sql", "one");
        let stale = project.status().unwrap();
        stage(&project, &["a.sql"]);
        assert!(project
            .commit(
                "no",
                &["a.sql".into()],
                None,
                stale.index_revision.as_deref().unwrap()
            )
            .unwrap_err()
            .contains("index changed"));
        commit(&project, &["a.sql"]).unwrap();
        let current = project.status().unwrap();
        assert!(project
            .stage(
                &["a.sql".into()],
                None,
                current.index_revision.as_deref().unwrap()
            )
            .unwrap_err()
            .contains("HEAD changed"));
    }
    #[test]
    #[cfg(unix)]
    fn ancestor_scope_rejects_traversal_and_symlinks_and_pathspecs_are_literal() {
        use std::os::unix::fs::symlink;
        let (dir, _) = fixture();
        fs::create_dir(dir.path().join("notebooks")).unwrap();
        let project = ProjectGit::new(dir.path().join("notebooks"));
        write(dir.path(), "outside.sql", "outside");
        write(dir.path(), "notebooks/:(glob)*.sql", "literal");
        symlink(
            dir.path().join("outside.sql"),
            dir.path().join("notebooks/link.sql"),
        )
        .unwrap();
        symlink(dir.path(), dir.path().join("notebooks/parent")).unwrap();
        for path in [
            "../outside.sql",
            "/outside.sql",
            "link.sql",
            "parent/outside.sql",
            ".git/config.sql",
            "a/./b.sql",
        ] {
            assert!(project.validate_path(path).is_err(), "{path}");
        }
        stage(&project, &[":(glob)*.sql"]);
        commit(&project, &[":(glob)*.sql"]).unwrap();
        assert_eq!(
            git(dir.path(), &["ls-tree", "-r", "--name-only", "HEAD"]),
            "notebooks/:(glob)*.sql"
        );
        assert!(project.status().unwrap().files.is_empty());
        assert!(project.init().is_err());
    }
    #[test]
    fn merge_conflicts_block_mutations() {
        let (dir, project) = fixture();
        write(dir.path(), "a.sql", "base\n");
        stage(&project, &["a.sql"]);
        commit(&project, &["a.sql"]).unwrap();
        git(dir.path(), &["checkout", "-b", "other"]);
        write(dir.path(), "a.sql", "other\n");
        stage(&project, &["a.sql"]);
        commit(&project, &["a.sql"]).unwrap();
        git(dir.path(), &["checkout", "main"]);
        write(dir.path(), "a.sql", "main\n");
        stage(&project, &["a.sql"]);
        commit(&project, &["a.sql"]).unwrap();
        assert!(
            !run(dir.path(), None, &["merge", "other"], None)
                .unwrap()
                .success
        );
        let status = project.status().unwrap();
        assert!(status.conflicted);
        assert!(project
            .stage(
                &["a.sql".into()],
                status.head.as_deref(),
                status.index_revision.as_deref().unwrap()
            )
            .unwrap_err()
            .contains("conflicts"));
    }
    #[test]
    fn linked_worktree_uses_its_own_index_and_head() {
        let (dir, project) = fixture();
        write(dir.path(), "a.sql", "base");
        stage(&project, &["a.sql"]);
        let base = commit(&project, &["a.sql"]).unwrap();
        let linked_parent = tempfile::tempdir().unwrap();
        let linked_path = linked_parent.path().join("linked");
        git(
            dir.path(),
            &[
                "worktree",
                "add",
                "-b",
                "linked",
                linked_path.to_str().unwrap(),
            ],
        );
        let linked = ProjectGit::new(linked_path.clone());
        write(&linked_path, "a.sql", "linked");
        stage(&linked, &["a.sql"]);
        let changed = commit(&linked, &["a.sql"]).unwrap();
        assert_ne!(changed, base);
        assert_eq!(
            project.status().unwrap().head.as_deref(),
            Some(base.as_str())
        );
        assert_eq!(linked.status().unwrap().branch.as_deref(), Some("linked"));
    }
    #[test]
    fn explicit_init_and_configured_local_upstream_push() {
        let root = tempfile::tempdir().unwrap();
        let project = ProjectGit::new(root.path().to_owned());
        assert!(project.status().unwrap().root.is_none());
        project.init().unwrap();
        assert!(project.status().unwrap().root.is_some());
        let (dir, project) = fixture();
        write(dir.path(), "a.sql", "one");
        stage(&project, &["a.sql"]);
        let revision = commit(&project, &["a.sql"]).unwrap();
        assert!(project.push(&revision).unwrap_err().contains("upstream"));
        let remote = tempfile::tempdir().unwrap();
        git(remote.path(), &["init", "--bare"]);
        git(
            dir.path(),
            &["remote", "add", "origin", remote.path().to_str().unwrap()],
        );
        git(dir.path(), &["config", "branch.main.remote", "origin"]);
        git(
            dir.path(),
            &["config", "branch.main.merge", "refs/heads/main"],
        );
        project.push(&revision).unwrap();
        assert_eq!(
            git(remote.path(), &["rev-parse", "refs/heads/main"]),
            revision
        );
    }
}
