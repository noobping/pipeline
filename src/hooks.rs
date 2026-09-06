//! Git repository discovery and safe Pipeline hook installation.

use std::collections::BTreeSet;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{self, Write};
use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::{NamedTempFile, TempDir, TempPath};
use thiserror::Error;

pub const MANAGED_HOOK_MARKER: &str = "# managed-by: pipeline";
pub const PIPELINE_DIRECTORY: &str = "pipeline";
pub const INSTALLED_PIPELINE_NAME: &str = "pipeline";
pub const INSTALLED_JUST_NAME: &str = "just";

/// Hook names documented by the current `githooks(5)` manual.
pub const SUPPORTED_HOOKS: &[&str] = &[
    "applypatch-msg",
    "pre-applypatch",
    "post-applypatch",
    "pre-commit",
    "pre-merge-commit",
    "prepare-commit-msg",
    "commit-msg",
    "post-commit",
    "pre-rebase",
    "post-checkout",
    "post-merge",
    "pre-push",
    "pre-receive",
    "update",
    "proc-receive",
    "post-receive",
    "post-update",
    "reference-transaction",
    "push-to-checkout",
    "pre-auto-gc",
    "post-rewrite",
    "sendemail-validate",
    "fsmonitor-watchman",
    "p4-changelist",
    "p4-prepare-changelist",
    "p4-post-changelist",
    "p4-pre-submit",
    "post-index-change",
];

pub type HookResult<T> = Result<T, HookError>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Repository {
    /// Directory from which discovery was requested.
    pub invocation_dir: PathBuf,
    /// Worktree-specific Git directory (`.git` for an ordinary repository).
    pub git_dir: PathBuf,
    /// Shared Git directory. Hook wrappers and installed binaries live here.
    pub common_dir: PathBuf,
    /// Worktree root, or `None` for a bare repository.
    pub work_tree: Option<PathBuf>,
    pub bare: bool,
}

impl Repository {
    pub fn discover(start: impl AsRef<Path>) -> HookResult<Self> {
        let invocation_dir = discovery_directory(start.as_ref())?;
        let git_dir = git_text(
            &invocation_dir,
            &["rev-parse", "--path-format=absolute", "--git-dir"],
        )?;
        let common_dir = git_text(
            &invocation_dir,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?;
        let bare = git_text(&invocation_dir, &["rev-parse", "--is-bare-repository"])? == "true";

        reject_configured_hooks_path(&invocation_dir)?;

        let git_dir = canonicalize_existing(Path::new(&git_dir))?;
        let common_dir = canonicalize_existing(Path::new(&common_dir))?;

        let work_tree = if bare {
            None
        } else {
            Some(discover_work_tree(&invocation_dir, &git_dir)?)
        };

        Ok(Self {
            invocation_dir: canonicalize_existing(&invocation_dir)?,
            git_dir,
            common_dir,
            work_tree,
            bare,
        })
    }

    pub fn hooks_dir(&self) -> PathBuf {
        self.common_dir.join("hooks")
    }

    pub fn pipeline_dir(&self) -> PathBuf {
        self.common_dir.join(PIPELINE_DIRECTORY)
    }

    pub fn installed_pipeline(&self) -> PathBuf {
        self.pipeline_dir().join(INSTALLED_PIPELINE_NAME)
    }

    pub fn installed_just(&self) -> PathBuf {
        self.pipeline_dir().join(INSTALLED_JUST_NAME)
    }

    pub fn policy_path(&self) -> PathBuf {
        self.pipeline_dir().join("config.yml")
    }

    pub fn root(&self) -> &Path {
        self.work_tree.as_deref().unwrap_or(&self.common_dir)
    }

    /// Return whether `path` is tracked by the current worktree's index.
    /// Bare materializations consist exclusively of tracked files and return
    /// true for paths below their temporary root by construction.
    pub fn is_tracked(&self, path: impl AsRef<Path>) -> HookResult<bool> {
        let Some(work_tree) = &self.work_tree else {
            return Ok(false);
        };
        let path = path.as_ref();
        let absolute;
        let path = if path.is_absolute() {
            path
        } else {
            absolute = work_tree.join(path);
            &absolute
        };
        let relative = path
            .strip_prefix(work_tree)
            .map_err(|_| HookError::OutsideRepository {
                path: path.to_path_buf(),
                root: work_tree.clone(),
            })?;
        let output = Command::new("git")
            .arg("-C")
            .arg(work_tree)
            .args(["ls-files", "--error-unmatch", "--"])
            .arg(relative)
            .output()
            .map_err(|source| HookError::GitStart { source })?;
        match output.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(git_failure("git ls-files", output)),
        }
    }

    /// Resolve `revision` to a commit object without accepting non-commit
    /// objects. The process inherits Git's quarantine environment.
    pub fn resolve_commit(&self, revision: &str) -> HookResult<String> {
        if revision.trim().is_empty() {
            return Err(HookError::InvalidRevision(revision.to_owned()));
        }
        let expression = format!("{revision}^{{commit}}");
        let output = self
            .git_command()
            .args(["rev-parse", "--verify", "--end-of-options"])
            .arg(expression)
            .output()
            .map_err(|source| HookError::GitStart { source })?;
        if !output.status.success() {
            return Err(HookError::InvalidRevision(revision.to_owned()));
        }
        let commit = output_text("git rev-parse", output)?;
        if commit.is_empty() {
            Err(HookError::InvalidRevision(revision.to_owned()))
        } else {
            Ok(commit)
        }
    }

    /// Materialize one tracked commit into a private temporary directory using
    /// an isolated index. This never changes the repository's index or refs.
    pub fn materialize_commit(&self, revision: &str) -> HookResult<MaterializedTree> {
        let commit = self.resolve_commit(revision)?;
        let temp = tempfile::Builder::new()
            .prefix("pipeline-tree-")
            .tempdir()
            .map_err(HookError::Io)?;
        let root = temp.path().join("worktree");
        fs::create_dir(&root).map_err(HookError::Io)?;
        let index = temp.path().join("index");

        let read_tree = self
            .git_command()
            .env("GIT_INDEX_FILE", &index)
            .args(["read-tree", "--"])
            .arg(&commit)
            .output()
            .map_err(|source| HookError::GitStart { source })?;
        ensure_git_success("git read-tree", read_tree)?;

        let checkout = self
            .git_command()
            .env("GIT_INDEX_FILE", &index)
            .env("GIT_LFS_SKIP_SMUDGE", "1")
            .arg(format!("--work-tree={}", root.display()))
            .args(["checkout-index", "--all", "--force"])
            .output()
            .map_err(|source| HookError::GitStart { source })?;
        ensure_git_success("git checkout-index", checkout)?;

        Ok(MaterializedTree {
            _temp: temp,
            root,
            index,
            commit,
        })
    }

    fn git_command(&self) -> Command {
        let mut command = Command::new("git");
        // A linked worktree's HEAD and index live in its worktree-specific
        // Git directory. Its `commondir` file still gives Git access to shared
        // objects and refs, so this also works for materialization.
        command.arg(format!("--git-dir={}", self.git_dir.display()));
        command
    }
}

#[derive(Debug)]
pub struct MaterializedTree {
    _temp: TempDir,
    pub root: PathBuf,
    pub index: PathBuf,
    pub commit: String,
}

impl MaterializedTree {
    pub fn path(&self) -> &Path {
        &self.root
    }

    pub fn index_path(&self) -> &Path {
        &self.index
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum InstallMode {
    #[default]
    Auto,
    Link,
    Copy,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct InstallModes {
    pub pipeline: InstallMode,
    pub just: InstallMode,
}

impl InstallModes {
    pub fn new(
        global: InstallMode,
        pipeline: Option<InstallMode>,
        just: Option<InstallMode>,
    ) -> Self {
        Self {
            pipeline: pipeline.unwrap_or(global),
            just: just.unwrap_or(global),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HookSelection {
    All,
    Managed,
    Explicit(BTreeSet<String>),
}

impl HookSelection {
    pub fn from_cli(names: &[String], managed: bool) -> HookResult<Self> {
        if managed && !names.is_empty() {
            return Err(HookError::ManagedWithExplicitHooks);
        }
        if managed {
            Ok(Self::Managed)
        } else if names.is_empty() {
            Ok(Self::All)
        } else {
            let hooks = names.iter().cloned().collect::<BTreeSet<_>>();
            validate_hooks(hooks.iter().map(String::as_str))?;
            Ok(Self::Explicit(hooks))
        }
    }

    pub fn for_add(&self, declared: &BTreeSet<String>) -> HookResult<BTreeSet<String>> {
        match self {
            Self::All => Ok(all_supported_hooks()),
            Self::Managed => {
                validate_hooks(declared.iter().map(String::as_str))?;
                Ok(declared.clone())
            }
            Self::Explicit(hooks) => Ok(hooks.clone()),
        }
    }

    pub fn for_remove(
        &self,
        repository: &Repository,
        declared: &BTreeSet<String>,
    ) -> HookResult<BTreeSet<String>> {
        let installed = installed_managed_hooks(repository)?;
        match self {
            Self::All => Ok(installed),
            Self::Managed => {
                validate_hooks(declared.iter().map(String::as_str))?;
                Ok(installed.intersection(declared).cloned().collect())
            }
            Self::Explicit(hooks) => Ok(hooks.clone()),
        }
    }
}

pub fn all_supported_hooks() -> BTreeSet<String> {
    SUPPORTED_HOOKS
        .iter()
        .map(|name| (*name).to_owned())
        .collect()
}

pub fn is_supported_hook(name: &str) -> bool {
    SUPPORTED_HOOKS.contains(&name)
}

pub fn validate_hooks<'a>(hooks: impl IntoIterator<Item = &'a str>) -> HookResult<()> {
    for hook in hooks {
        if !is_supported_hook(hook) {
            return Err(HookError::UnsupportedHook(hook.to_owned()));
        }
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub struct InstallSources {
    pub pipeline: PathBuf,
    pub just: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstallReport {
    pub hooks: BTreeSet<String>,
    pub pipeline_mode: InstallMode,
    pub just_mode: InstallMode,
}

/// Validate every deterministic install condition that does not depend on a
/// resolved Just runtime. Callers use this before a container fallback so a
/// layout or ownership conflict cannot cause an unnecessary image pull.
pub fn preflight_install(
    repository: &Repository,
    hooks: &BTreeSet<String>,
    pipeline_source: &Path,
    pipeline_mode: InstallMode,
) -> HookResult<()> {
    validate_hooks(hooks.iter().map(String::as_str))?;
    validate_hook_destinations(repository, hooks)?;
    validate_install_layout(repository)?;
    resolve_target_install_mode(
        "pipeline",
        pipeline_source,
        &repository.installed_pipeline(),
        pipeline_mode,
    )?;
    Ok(())
}

/// Install both executables and marker-owned hook wrappers.
///
/// All hook destinations are checked before any path is changed, so an
/// unmanaged collision leaves the installation untouched.
pub fn install_hooks(
    repository: &Repository,
    hooks: &BTreeSet<String>,
    sources: &InstallSources,
    modes: InstallModes,
) -> HookResult<InstallReport> {
    validate_hooks(hooks.iter().map(String::as_str))?;
    validate_hook_destinations(repository, hooks)?;
    validate_install_layout(repository)?;
    if hooks.is_empty() {
        return Ok(InstallReport {
            hooks: BTreeSet::new(),
            pipeline_mode: modes.pipeline,
            just_mode: modes.just,
        });
    }

    let pipeline_mode = resolve_target_install_mode(
        "pipeline",
        &sources.pipeline,
        &repository.installed_pipeline(),
        modes.pipeline,
    )?;
    let just_mode = resolve_target_install_mode(
        "just",
        &sources.just,
        &repository.installed_just(),
        modes.just,
    )?;

    let pipeline_directory_existed = repository.pipeline_dir().is_dir();
    let hooks_directory_existed = repository.hooks_dir().is_dir();
    fs::create_dir_all(repository.pipeline_dir()).map_err(HookError::Io)?;
    if let Err(error) = fs::create_dir_all(repository.hooks_dir()) {
        if !pipeline_directory_existed {
            let _ = fs::remove_dir(repository.pipeline_dir());
        }
        return Err(HookError::Io(error));
    }
    let installation = (|| {
        let mut staged = Vec::new();
        if let Some(pipeline) = stage_executable(
            &sources.pipeline,
            &repository.installed_pipeline(),
            pipeline_mode,
        )? {
            staged.push(pipeline);
        }
        if let Some(just) =
            stage_executable(&sources.just, &repository.installed_just(), just_mode)?
        {
            staged.push(just);
        }
        for hook in hooks {
            staged.push(stage_hook_wrapper(repository, hook)?);
        }
        commit_staged(staged)
    })();
    if let Err(error) = installation {
        if !pipeline_directory_existed {
            let _ = fs::remove_dir(repository.pipeline_dir());
        }
        if !hooks_directory_existed {
            let _ = fs::remove_dir(repository.hooks_dir());
        }
        return Err(error);
    }

    Ok(InstallReport {
        hooks: hooks.clone(),
        pipeline_mode,
        just_mode,
    })
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RemovalReport {
    pub hooks: BTreeSet<String>,
    pub removed_binaries: bool,
}

pub fn remove_hooks(
    repository: &Repository,
    hooks: &BTreeSet<String>,
) -> HookResult<RemovalReport> {
    validate_hooks(hooks.iter().map(String::as_str))?;

    // Validate the whole operation before removing the first wrapper.
    for hook in hooks {
        let path = repository.hooks_dir().join(hook);
        if path_exists(&path) && !is_managed_hook(&path)? {
            return Err(HookError::UnmanagedHook(path));
        }
    }

    let mut removed = BTreeSet::new();
    for hook in hooks {
        let path = repository.hooks_dir().join(hook);
        if is_managed_hook(&path)? {
            fs::remove_file(&path).map_err(HookError::Io)?;
            removed.insert(hook.clone());
        }
    }

    let removed_binaries = installed_managed_hooks(repository)?.is_empty();
    if removed_binaries {
        remove_file_if_present(&repository.installed_pipeline())?;
        remove_file_if_present(&repository.installed_just())?;
        // Keep pipeline/config.yml. Remove the directory only when empty.
        let _ = fs::remove_dir(repository.pipeline_dir());
    }

    Ok(RemovalReport {
        hooks: removed,
        removed_binaries,
    })
}

pub fn installed_managed_hooks(repository: &Repository) -> HookResult<BTreeSet<String>> {
    let mut hooks = BTreeSet::new();
    for hook in SUPPORTED_HOOKS {
        if is_managed_hook(&repository.hooks_dir().join(hook))? {
            hooks.insert((*hook).to_owned());
        }
    }
    Ok(hooks)
}

pub fn is_managed_hook(path: &Path) -> HookResult<bool> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(HookError::Io(error)),
    };
    // Pipeline always writes regular wrappers. Never follow a symlink when
    // deciding whether a path is safe to replace or delete.
    if !metadata.file_type().is_file() {
        return Ok(false);
    }
    let contents = fs::read(path).map_err(HookError::Io)?;
    Ok(contents
        .split(|byte| *byte == b'\n')
        .take(4)
        .any(|line| line == MANAGED_HOOK_MARKER.as_bytes()))
}

pub fn hook_environment(
    hook: &str,
    arguments: &[OsString],
) -> HookResult<Vec<(OsString, OsString)>> {
    if !is_supported_hook(hook) {
        return Err(HookError::UnsupportedHook(hook.to_owned()));
    }
    let mut environment = Vec::with_capacity(arguments.len() + 2);
    environment.push(("PIPELINE_HOOK".into(), hook.into()));
    environment.push((
        "PIPELINE_HOOK_ARGC".into(),
        arguments.len().to_string().into(),
    ));
    environment.extend(arguments.iter().enumerate().map(|(index, argument)| {
        (
            format!("PIPELINE_HOOK_ARG_{index}").into(),
            argument.clone(),
        )
    }));
    Ok(environment)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RefUpdate {
    pub old_oid: String,
    pub new_oid: String,
    pub reference: String,
}

pub fn parse_ref_updates(input: &[u8]) -> HookResult<Vec<RefUpdate>> {
    let text = std::str::from_utf8(input).map_err(|_| HookError::InvalidRefInput)?;
    let mut updates = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let mut fields = line.split_whitespace();
        let (Some(old_oid), Some(new_oid), Some(reference), None) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            return Err(HookError::InvalidRefInput);
        };
        updates.push(RefUpdate {
            old_oid: old_oid.to_owned(),
            new_oid: new_oid.to_owned(),
            reference: reference.to_owned(),
        });
    }
    Ok(updates)
}

pub fn incoming_commits(repository: &Repository, updates: &[RefUpdate]) -> HookResult<Vec<String>> {
    let mut commits = BTreeSet::new();
    for update in updates {
        if is_zero_oid(&update.new_oid) {
            continue;
        }
        // Tags and other objects are accepted only when Git can peel them to a
        // commit. Non-commit updates are intentionally skipped.
        match repository.resolve_commit(&update.new_oid) {
            Ok(commit) => {
                commits.insert(commit);
            }
            Err(HookError::InvalidRevision(_)) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(commits.into_iter().collect())
}

pub fn is_zero_oid(oid: &str) -> bool {
    !oid.is_empty() && oid.bytes().all(|byte| byte == b'0')
}

fn validate_hook_destinations(repository: &Repository, hooks: &BTreeSet<String>) -> HookResult<()> {
    for hook in hooks {
        let path = repository.hooks_dir().join(hook);
        if path_exists(&path) && !is_managed_hook(&path)? {
            return Err(HookError::UnmanagedHook(path));
        }
    }
    Ok(())
}

fn validate_install_layout(repository: &Repository) -> HookResult<()> {
    for directory in [repository.pipeline_dir(), repository.hooks_dir()] {
        match fs::symlink_metadata(&directory) {
            Ok(metadata) if metadata.file_type().is_dir() => {}
            Ok(_) => return Err(HookError::InvalidInstallPath(directory)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(HookError::Io(error)),
        }
    }
    for target in [repository.installed_pipeline(), repository.installed_just()] {
        match fs::symlink_metadata(&target) {
            Ok(metadata) if metadata.file_type().is_file() || metadata.file_type().is_symlink() => {
            }
            Ok(_) => return Err(HookError::InvalidInstallPath(target)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(HookError::Io(error)),
        }
    }
    Ok(())
}

struct StagedInstall {
    temporary: TempPath,
    target: PathBuf,
    require_managed_target: bool,
}

fn stage_executable(
    source: &Path,
    target: &Path,
    mode: InstallMode,
) -> HookResult<Option<StagedInstall>> {
    let source = executable_source(source)?;
    if source == target && mode == InstallMode::Copy {
        return Ok(None);
    }
    if path_exists(target)
        && mode == InstallMode::Link
        && fs::symlink_metadata(target)
            .map_err(HookError::Io)?
            .file_type()
            .is_symlink()
        && fs::read_link(target).map_err(HookError::Io)? == source
    {
        return Ok(None);
    }

    match mode {
        InstallMode::Auto => unreachable!("install mode must be resolved first"),
        InstallMode::Copy => stage_copy(&source, target, 0o755).map(Some),
        InstallMode::Link => stage_symlink(&source, target).map(Some),
    }
}

fn stage_hook_wrapper(repository: &Repository, hook: &str) -> HookResult<StagedInstall> {
    let path = repository.hooks_dir().join(hook);
    let mut staged = stage_bytes(&path, hook_wrapper(hook).as_bytes(), 0o755)?;
    staged.require_managed_target = true;
    Ok(staged)
}

fn hook_wrapper(hook: &str) -> String {
    format!(
        "#!/bin/sh\n{MANAGED_HOOK_MARKER}\nhook_dir=$(CDPATH= cd \"$(dirname \"$0\")\" && pwd) || exit 126\nrunner=\"$hook_dir/../pipeline/pipeline\"\nif [ ! -x \"$runner\" ]; then\n  echo \"pipeline: installed runner is missing; rerun 'pipeline add'\" >&2\n  exit 126\nfi\nexec \"$runner\" __hook {hook} \"$@\"\n"
    )
}

fn stage_copy(source: &Path, target: &Path, mode: u32) -> HookResult<StagedInstall> {
    let parent = target
        .parent()
        .ok_or_else(|| HookError::InvalidInstallPath(target.to_path_buf()))?;
    let mut temp = NamedTempFile::new_in(parent).map_err(HookError::Io)?;
    let mut input = File::open(source).map_err(HookError::Io)?;
    io::copy(&mut input, temp.as_file_mut()).map_err(HookError::Io)?;
    finish_staged_file(temp, target, mode)
}

fn stage_bytes(target: &Path, contents: &[u8], mode: u32) -> HookResult<StagedInstall> {
    let parent = target
        .parent()
        .ok_or_else(|| HookError::InvalidInstallPath(target.to_path_buf()))?;
    let mut temp = NamedTempFile::new_in(parent).map_err(HookError::Io)?;
    temp.write_all(contents).map_err(HookError::Io)?;
    finish_staged_file(temp, target, mode)
}

fn finish_staged_file(
    mut temp: NamedTempFile,
    target: &Path,
    mode: u32,
) -> HookResult<StagedInstall> {
    temp.as_file_mut().flush().map_err(HookError::Io)?;
    temp.as_file()
        .set_permissions(fs::Permissions::from_mode(mode))
        .map_err(HookError::Io)?;
    Ok(StagedInstall {
        temporary: temp.into_temp_path(),
        target: target.to_path_buf(),
        require_managed_target: false,
    })
}

fn stage_symlink(source: &Path, target: &Path) -> HookResult<StagedInstall> {
    let parent = target
        .parent()
        .ok_or_else(|| HookError::InvalidInstallPath(target.to_path_buf()))?;
    let temporary = vacant_temp_path(parent)?;
    symlink(source, &temporary).map_err(HookError::Io)?;
    Ok(StagedInstall {
        temporary,
        target: target.to_path_buf(),
        require_managed_target: false,
    })
}

struct AppliedInstall {
    target: PathBuf,
    backup: Option<TempPath>,
}

fn commit_staged(staged: Vec<StagedInstall>) -> HookResult<()> {
    let mut applied = Vec::new();
    for entry in staged {
        if entry.require_managed_target && path_exists(&entry.target) {
            match is_managed_hook(&entry.target) {
                Ok(true) => {}
                Ok(false) => {
                    return rollback_error(HookError::UnmanagedHook(entry.target), &mut applied)
                }
                Err(error) => return rollback_error(error, &mut applied),
            }
        }
        let backup = if path_exists(&entry.target) {
            let parent = entry
                .target
                .parent()
                .expect("validated installation target has a parent");
            let backup = match vacant_temp_path(parent) {
                Ok(backup) => backup,
                Err(error) => return rollback_error(error, &mut applied),
            };
            if let Err(error) = fs::rename(&entry.target, &backup) {
                return rollback_error(HookError::Io(error), &mut applied);
            }
            Some(backup)
        } else {
            None
        };

        if let Err(error) = fs::rename(&entry.temporary, &entry.target) {
            let error = if let Some(backup) = backup {
                match restore_backup(backup, &entry.target) {
                    Ok(()) => HookError::Io(error),
                    Err(restore) => HookError::InstallTransaction(format!(
                        "{}; restoring {} also failed: {restore}",
                        HookError::Io(error),
                        entry.target.display()
                    )),
                }
            } else {
                HookError::Io(error)
            };
            return rollback_error(error, &mut applied);
        }
        applied.push(AppliedInstall {
            target: entry.target,
            backup,
        });
    }
    Ok(())
}

fn vacant_temp_path(parent: &Path) -> HookResult<TempPath> {
    let path = NamedTempFile::new_in(parent)
        .map_err(HookError::Io)?
        .into_temp_path();
    fs::remove_file(&path).map_err(HookError::Io)?;
    Ok(path)
}

fn rollback_error(error: HookError, applied: &mut Vec<AppliedInstall>) -> HookResult<()> {
    let mut rollback_errors = Vec::new();
    for entry in applied.drain(..).rev() {
        let result = if let Some(backup) = entry.backup {
            restore_backup(backup, &entry.target)
        } else {
            fs::remove_file(&entry.target).map_err(|error| error.to_string())
        };
        if let Err(error) = result {
            rollback_errors.push(format!("{}: {error}", entry.target.display()));
        }
    }
    let rollback = if rollback_errors.is_empty() {
        String::new()
    } else {
        format!("; rollback also failed for {}", rollback_errors.join(", "))
    };
    Err(HookError::InstallTransaction(format!("{error}{rollback}")))
}

fn restore_backup(backup: TempPath, target: &Path) -> std::result::Result<(), String> {
    if let Err(error) = fs::rename(&backup, target) {
        let recovery = backup.to_path_buf();
        let preservation = backup
            .keep()
            .map(|_| String::new())
            .unwrap_or_else(|keep| format!("; could not preserve backup: {keep}"));
        return Err(format!(
            "{error}; original retained at {}{preservation}",
            recovery.display()
        ));
    }
    Ok(())
}

#[cfg(test)]
fn install_hook_wrapper(repository: &Repository, hook: &str) -> HookResult<()> {
    let path = repository.hooks_dir().join(hook);
    atomic_write(&path, hook_wrapper(hook).as_bytes(), 0o755)
}

pub fn resolve_install_mode(
    command: &str,
    source: &Path,
    requested: InstallMode,
) -> HookResult<InstallMode> {
    let source = executable_source(source)?;
    let from_path = resolve_in_path(command)
        .and_then(|path| fs::canonicalize(path).ok())
        .is_some_and(|path| same_file_or_path(&source, &path));

    match requested {
        InstallMode::Auto if from_path => Ok(InstallMode::Link),
        InstallMode::Auto => Ok(InstallMode::Copy),
        InstallMode::Copy => Ok(InstallMode::Copy),
        InstallMode::Link if from_path => Ok(InstallMode::Link),
        InstallMode::Link => Err(HookError::LinkSourceNotInPath {
            command: command.to_owned(),
            path: source,
        }),
    }
}

fn resolve_target_install_mode(
    command: &str,
    source: &Path,
    target: &Path,
    requested: InstallMode,
) -> HookResult<InstallMode> {
    let source = executable_source(source)?;
    if source == target && !fs::symlink_metadata(target)?.file_type().is_symlink() {
        return match requested {
            InstallMode::Link => Err(HookError::LinkTargetIsSource(target.to_path_buf())),
            InstallMode::Auto | InstallMode::Copy => Ok(InstallMode::Copy),
        };
    }
    resolve_install_mode(command, &source, requested)
}

#[cfg(test)]
fn install_executable(source: &Path, target: &Path, mode: InstallMode) -> HookResult<()> {
    let source = executable_source(source)?;
    let parent = target
        .parent()
        .ok_or_else(|| HookError::InvalidInstallPath(target.to_path_buf()))?;
    fs::create_dir_all(parent).map_err(HookError::Io)?;
    if path_exists(target) && same_file_or_path(&source, target) {
        let target_is_symlink = fs::symlink_metadata(target)
            .map_err(HookError::Io)?
            .file_type()
            .is_symlink();
        // A requested copy must replace an existing link even though both
        // paths currently resolve to the same inode. Other same-file cases are
        // already in the requested form, or would create a link to itself.
        if !(mode == InstallMode::Copy && target_is_symlink) {
            return Ok(());
        }
    }
    match mode {
        InstallMode::Auto => unreachable!("install mode must be resolved first"),
        InstallMode::Copy => atomic_copy(&source, target, 0o755),
        InstallMode::Link => atomic_symlink(&source, target),
    }
}

fn executable_source(path: &Path) -> HookResult<PathBuf> {
    let canonical = fs::canonicalize(path).map_err(|source| HookError::ExecutableSource {
        path: path.to_path_buf(),
        source,
    })?;
    let metadata = fs::metadata(&canonical).map_err(HookError::Io)?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
        return Err(HookError::NotExecutable(canonical));
    }
    Ok(canonical)
}

fn resolve_in_path(command: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|directory| directory.join(command))
        .find(|candidate| {
            fs::metadata(candidate)
                .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
        })
}

fn same_file_or_path(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    let Ok(left) = fs::metadata(left) else {
        return false;
    };
    let Ok(right) = fs::metadata(right) else {
        return false;
    };
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(test)]
fn atomic_copy(source: &Path, target: &Path, mode: u32) -> HookResult<()> {
    let parent = target
        .parent()
        .ok_or_else(|| HookError::InvalidInstallPath(target.to_path_buf()))?;
    let mut temp = NamedTempFile::new_in(parent).map_err(HookError::Io)?;
    let mut input = File::open(source).map_err(HookError::Io)?;
    io::copy(&mut input, temp.as_file_mut()).map_err(HookError::Io)?;
    temp.as_file_mut().flush().map_err(HookError::Io)?;
    temp.as_file()
        .set_permissions(fs::Permissions::from_mode(mode))
        .map_err(HookError::Io)?;
    temp.persist(target)
        .map_err(|error| HookError::Io(error.error))?;
    Ok(())
}

#[cfg(test)]
fn atomic_write(target: &Path, contents: &[u8], mode: u32) -> HookResult<()> {
    let parent = target
        .parent()
        .ok_or_else(|| HookError::InvalidInstallPath(target.to_path_buf()))?;
    fs::create_dir_all(parent).map_err(HookError::Io)?;
    let mut temp = NamedTempFile::new_in(parent).map_err(HookError::Io)?;
    temp.write_all(contents).map_err(HookError::Io)?;
    temp.as_file_mut().flush().map_err(HookError::Io)?;
    temp.as_file()
        .set_permissions(fs::Permissions::from_mode(mode))
        .map_err(HookError::Io)?;
    temp.persist(target)
        .map_err(|error| HookError::Io(error.error))?;
    Ok(())
}

#[cfg(test)]
fn atomic_symlink(source: &Path, target: &Path) -> HookResult<()> {
    let parent = target
        .parent()
        .ok_or_else(|| HookError::InvalidInstallPath(target.to_path_buf()))?;
    fs::create_dir_all(parent).map_err(HookError::Io)?;
    let temporary = NamedTempFile::new_in(parent).map_err(HookError::Io)?;
    let temporary_path = temporary.path().to_path_buf();
    drop(temporary);
    symlink(source, &temporary_path).map_err(HookError::Io)?;
    match fs::rename(&temporary_path, target) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = fs::remove_file(&temporary_path);
            Err(HookError::Io(error))
        }
    }
}

fn remove_file_if_present(path: &Path) -> HookResult<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(HookError::Io(error)),
    }
}

fn path_exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

fn discovery_directory(path: &Path) -> HookResult<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir().map_err(HookError::Io)?.join(path)
    };
    if absolute.is_dir() {
        Ok(absolute)
    } else if absolute.exists() {
        absolute
            .parent()
            .map(Path::to_path_buf)
            .ok_or(HookError::NotRepository(absolute))
    } else {
        Err(HookError::NotRepository(absolute))
    }
}

fn canonicalize_existing(path: &Path) -> HookResult<PathBuf> {
    fs::canonicalize(path).map_err(|source| HookError::Canonicalize {
        path: path.to_path_buf(),
        source,
    })
}

fn reject_configured_hooks_path(directory: &Path) -> HookResult<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(["config", "--get", "core.hooksPath"])
        .output()
        .map_err(|source| HookError::GitStart { source })?;
    match output.status.code() {
        Some(0) => {
            let configured = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            Err(HookError::ConfiguredHooksPath(if configured.is_empty() {
                "<empty>".to_owned()
            } else {
                configured
            }))
        }
        Some(1) => Ok(()),
        _ => Err(git_failure("git config --get core.hooksPath", output)),
    }
}

fn git_text(directory: &Path, args: &[&str]) -> HookResult<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(args)
        .output()
        .map_err(|source| HookError::GitStart { source })?;
    output_text(&format!("git {}", args.join(" ")), output)
}

fn discover_work_tree(invocation_dir: &Path, git_dir: &Path) -> HookResult<PathBuf> {
    if let Ok(path) = git_text(invocation_dir, &["rev-parse", "--show-toplevel"]) {
        return canonicalize_existing(Path::new(&path));
    }

    let mut candidates = Vec::new();
    if git_dir.file_name() == Some(OsStr::new(".git")) {
        if let Some(parent) = git_dir.parent() {
            candidates.push(parent.to_path_buf());
        }
    }

    if let Ok(pointer) = fs::read_to_string(git_dir.join("gitdir")) {
        let pointer = PathBuf::from(pointer.trim());
        let pointer = if pointer.is_absolute() {
            pointer
        } else {
            git_dir.join(pointer)
        };
        if let Some(parent) = pointer.parent() {
            candidates.push(parent.to_path_buf());
        }
    }

    let configured = Command::new("git")
        .arg(format!("--git-dir={}", git_dir.display()))
        .args(["config", "--get", "core.worktree"])
        .output()
        .map_err(|source| HookError::GitStart { source })?;
    if configured.status.success() {
        let configured = String::from_utf8(configured.stdout)
            .map_err(|_| HookError::GitNonUtf8("git config --get core.worktree".to_owned()))?;
        let configured = PathBuf::from(configured.trim());
        candidates.push(if configured.is_absolute() {
            configured
        } else {
            git_dir.join(configured)
        });
    }

    for candidate in candidates {
        let Ok(candidate) = fs::canonicalize(candidate) else {
            continue;
        };
        let Ok(discovered) = git_text(
            &candidate,
            &["rev-parse", "--path-format=absolute", "--git-dir"],
        ) else {
            continue;
        };
        if fs::canonicalize(discovered).is_ok_and(|path| path == git_dir) {
            return Ok(candidate);
        }
    }

    Err(HookError::WorkTreeNotFound(git_dir.to_path_buf()))
}

fn output_text(operation: &str, output: Output) -> HookResult<String> {
    if !output.status.success() {
        return Err(git_failure(operation, output));
    }
    String::from_utf8(output.stdout)
        .map(|value| value.trim().to_owned())
        .map_err(|_| HookError::GitNonUtf8(operation.to_owned()))
}

fn ensure_git_success(operation: &str, output: Output) -> HookResult<()> {
    if output.status.success() {
        Ok(())
    } else {
        Err(git_failure(operation, output))
    }
}

fn git_failure(operation: &str, output: Output) -> HookError {
    HookError::GitFailed {
        operation: operation.to_owned(),
        status: output.status.code(),
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
    }
}

#[derive(Debug, Error)]
pub enum HookError {
    #[error("could not run git: {source}")]
    GitStart {
        #[source]
        source: io::Error,
    },

    #[error("{operation} failed (status {status:?}): {stderr}")]
    GitFailed {
        operation: String,
        status: Option<i32>,
        stderr: String,
    },

    #[error("{0} returned non-UTF-8 output")]
    GitNonUtf8(String),

    #[error("not a Git repository: {0}")]
    NotRepository(PathBuf),

    #[error("could not locate the worktree belonging to Git directory {0}")]
    WorkTreeNotFound(PathBuf),

    #[error("could not canonicalize {path}: {source}")]
    Canonicalize {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("core.hooksPath is configured as {0:?}; Pipeline only installs into Git's standard hooks directory")]
    ConfiguredHooksPath(String),

    #[error("unsupported Git hook {0:?}")]
    UnsupportedHook(String),

    #[error("--managed cannot be combined with explicit hook names")]
    ManagedWithExplicitHooks,

    #[error("refusing to replace or remove unmanaged hook {0}")]
    UnmanagedHook(PathBuf),

    #[error("cannot link {command} from {path}: the same executable is not available from PATH")]
    LinkSourceNotInPath { command: String, path: PathBuf },

    #[error(
        "cannot replace {0} with a symlink to itself; run Pipeline from another PATH location"
    )]
    LinkTargetIsSource(PathBuf),

    #[error("could not use executable source {path}: {source}")]
    ExecutableSource {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("file is not executable: {0}")]
    NotExecutable(PathBuf),

    #[error("invalid installation path: {0}")]
    InvalidInstallPath(PathBuf),

    #[error("hook installation transaction failed: {0}")]
    InstallTransaction(String),

    #[error("path {path} is outside repository worktree {root}")]
    OutsideRepository { path: PathBuf, root: PathBuf },

    #[error("revision does not resolve to a commit: {0:?}")]
    InvalidRevision(String),

    #[error("invalid Git reference update input")]
    InvalidRefInput,

    #[error(transparent)]
    Io(#[from] io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;

    fn fake_repository(root: &Path) -> Repository {
        fs::create_dir_all(root.join("hooks")).unwrap();
        Repository {
            invocation_dir: root.to_path_buf(),
            git_dir: root.to_path_buf(),
            common_dir: root.to_path_buf(),
            work_tree: None,
            bare: true,
        }
    }

    fn executable(path: &Path, body: &str) {
        fs::write(path, body).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn selection_defaults_to_all_and_managed_is_an_intersection_on_remove() {
        let temp = tempfile::tempdir().unwrap();
        let repository = fake_repository(temp.path());
        atomic_write(
            &repository.hooks_dir().join("pre-commit"),
            format!("#!/bin/sh\n{MANAGED_HOOK_MARKER}\n").as_bytes(),
            0o755,
        )
        .unwrap();
        let declared = BTreeSet::from(["pre-commit".to_owned(), "pre-push".to_owned()]);

        assert_eq!(
            HookSelection::from_cli(&[], false)
                .unwrap()
                .for_add(&declared)
                .unwrap(),
            all_supported_hooks()
        );
        assert_eq!(
            HookSelection::Managed
                .for_remove(&repository, &declared)
                .unwrap(),
            BTreeSet::from(["pre-commit".to_owned()])
        );
    }

    #[test]
    fn rejects_managed_plus_names_and_unknown_names() {
        assert!(matches!(
            HookSelection::from_cli(&["pre-commit".to_owned()], true),
            Err(HookError::ManagedWithExplicitHooks)
        ));
        assert!(matches!(
            HookSelection::from_cli(&["made-up".to_owned()], false),
            Err(HookError::UnsupportedHook(_))
        ));
    }

    #[test]
    fn wrappers_are_owned_executable_and_preserve_dispatch_arguments() {
        let temp = tempfile::tempdir().unwrap();
        let repository = fake_repository(temp.path());
        install_hook_wrapper(&repository, "commit-msg").unwrap();
        let path = repository.hooks_dir().join("commit-msg");
        assert!(is_managed_hook(&path).unwrap());
        assert_ne!(fs::metadata(&path).unwrap().permissions().mode() & 0o111, 0);
        let text = fs::read_to_string(path).unwrap();
        assert!(text.contains("__hook commit-msg \"$@\""));
    }

    #[test]
    fn unmanaged_collision_is_checked_before_any_wrapper_changes() {
        let temp = tempfile::tempdir().unwrap();
        let repository = fake_repository(temp.path());
        let unmanaged = repository.hooks_dir().join("pre-push");
        fs::write(&unmanaged, "#!/bin/sh\nexit 0\n").unwrap();
        let hooks = BTreeSet::from(["pre-commit".to_owned(), "pre-push".to_owned()]);

        assert!(matches!(
            validate_hook_destinations(&repository, &hooks),
            Err(HookError::UnmanagedHook(path)) if path == unmanaged
        ));
        assert!(!repository.hooks_dir().join("pre-commit").exists());
    }

    #[test]
    fn commit_rechecks_hook_ownership() {
        let temp = tempfile::tempdir().unwrap();
        let repository = fake_repository(temp.path());
        let target = repository.hooks_dir().join("pre-commit");
        executable(
            &target,
            &format!("#!/bin/sh\n{MANAGED_HOOK_MARKER}\nexit 0\n"),
        );
        let staged = stage_hook_wrapper(&repository, "pre-commit").unwrap();

        executable(&target, "#!/bin/sh\necho unmanaged\n");
        assert!(matches!(
            commit_staged(vec![staged]),
            Err(HookError::InstallTransaction(message)) if message.contains("unmanaged hook")
        ));
        assert_eq!(
            fs::read_to_string(target).unwrap(),
            "#!/bin/sh\necho unmanaged\n"
        );
    }

    #[test]
    fn transaction_restores_earlier_targets_when_a_later_commit_fails() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first");
        fs::write(&first, "old\n").unwrap();
        let staged_first = stage_bytes(&first, b"new\n", 0o755).unwrap();

        let second_parent = temp.path().join("second-parent");
        fs::create_dir(&second_parent).unwrap();
        let second = second_parent.join("second");
        let staged_second = stage_bytes(&second, b"second\n", 0o755).unwrap();
        fs::rename(&second_parent, temp.path().join("moved-parent")).unwrap();

        assert!(matches!(
            commit_staged(vec![staged_first, staged_second]),
            Err(HookError::InstallTransaction(_))
        ));
        assert_eq!(fs::read_to_string(first).unwrap(), "old\n");
    }

    #[test]
    fn invalid_second_binary_source_is_checked_before_any_installation() {
        let temp = tempfile::tempdir().unwrap();
        let repository = fake_repository(temp.path());
        let pipeline = temp.path().join("source-pipeline");
        executable(&pipeline, "#!/bin/sh\n");
        let sources = InstallSources {
            pipeline,
            just: temp.path().join("missing-just"),
        };
        let hooks = BTreeSet::from(["pre-commit".to_owned()]);

        assert!(install_hooks(
            &repository,
            &hooks,
            &sources,
            InstallModes::new(InstallMode::Copy, None, None),
        )
        .is_err());
        assert!(!repository.installed_pipeline().exists());
        assert!(!repository.hooks_dir().join("pre-commit").exists());
    }

    #[test]
    fn explicit_copy_replaces_an_existing_link() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let target = temp.path().join("target");
        executable(&source, "#!/bin/sh\necho source\n");

        install_executable(&source, &target, InstallMode::Link).unwrap();
        assert!(fs::symlink_metadata(&target)
            .unwrap()
            .file_type()
            .is_symlink());

        install_executable(&source, &target, InstallMode::Copy).unwrap();
        assert!(!fs::symlink_metadata(&target)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            fs::read_to_string(target).unwrap(),
            "#!/bin/sh\necho source\n"
        );
    }

    #[test]
    fn explicit_modes_replace_hardlinks_and_noncanonical_symlinks() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        executable(&source, "#!/bin/sh\necho source\n");

        let copy_target = temp.path().join("copy-target");
        fs::hard_link(&source, &copy_target).unwrap();
        let staged = stage_executable(&source, &copy_target, InstallMode::Copy)
            .unwrap()
            .unwrap();
        commit_staged(vec![staged]).unwrap();
        assert_ne!(
            fs::metadata(&source).unwrap().ino(),
            fs::metadata(&copy_target).unwrap().ino()
        );

        let link_target = temp.path().join("link-target");
        symlink("source", &link_target).unwrap();
        let staged = stage_executable(&source, &link_target, InstallMode::Link)
            .unwrap()
            .unwrap();
        commit_staged(vec![staged]).unwrap();
        assert_eq!(fs::read_link(link_target).unwrap(), source);
    }

    #[test]
    fn an_installed_executable_is_never_linked_to_itself() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("pipeline");
        executable(&target, "#!/bin/sh\n");
        assert_eq!(
            resolve_target_install_mode("pipeline", &target, &target, InstallMode::Auto).unwrap(),
            InstallMode::Copy
        );
        assert!(matches!(
            resolve_target_install_mode("pipeline", &target, &target, InstallMode::Link),
            Err(HookError::LinkTargetIsSource(path)) if path == target
        ));
    }

    #[test]
    fn removing_last_wrapper_removes_binaries_but_keeps_policy() {
        let temp = tempfile::tempdir().unwrap();
        let repository = fake_repository(temp.path());
        fs::create_dir_all(repository.pipeline_dir()).unwrap();
        executable(&repository.installed_pipeline(), "#!/bin/sh\n");
        executable(&repository.installed_just(), "#!/bin/sh\n");
        fs::write(repository.policy_path(), "version: 1\n").unwrap();
        install_hook_wrapper(&repository, "pre-commit").unwrap();

        let report = remove_hooks(&repository, &BTreeSet::from(["pre-commit".to_owned()])).unwrap();
        assert!(report.removed_binaries);
        assert!(!repository.installed_pipeline().exists());
        assert!(!repository.installed_just().exists());
        assert!(repository.policy_path().exists());
    }

    #[test]
    fn parses_and_deduplicates_incoming_commits() {
        let updates =
            parse_ref_updates(b"0000 1111 refs/heads/main\n2222 0000 refs/heads/old\n").unwrap();
        assert_eq!(updates.len(), 2);
        assert!(is_zero_oid(&updates[0].old_oid));
        assert!(is_zero_oid(&updates[1].new_oid));
    }

    #[test]
    fn discovers_normal_and_bare_repositories_and_materializes_a_commit() {
        let temp = tempfile::tempdir().unwrap();
        let work = temp.path().join("work");
        let bare = temp.path().join("bare.git");
        fs::create_dir(&work).unwrap();
        let init = Command::new("git")
            .args(["init", "-q"])
            .arg(&work)
            .status()
            .unwrap();
        assert!(init.success());
        fs::write(work.join("tracked"), "hello\n").unwrap();
        let status = Command::new("git")
            .arg("-C")
            .arg(&work)
            .args(["add", "tracked"])
            .status()
            .unwrap();
        assert!(status.success());
        let status = Command::new("git")
            .arg("-C")
            .arg(&work)
            .args([
                "-c",
                "user.name=Pipeline Test",
                "-c",
                "user.email=pipeline@example.invalid",
                "commit",
                "-qm",
                "initial",
            ])
            .status()
            .unwrap();
        assert!(status.success());

        let normal = Repository::discover(&work).unwrap();
        assert!(!normal.bare);
        assert!(normal.is_tracked(work.join("tracked")).unwrap());
        let from_dot_git = Repository::discover(work.join(".git")).unwrap();
        assert_eq!(from_dot_git.work_tree.as_deref(), Some(work.as_path()));

        let status = Command::new("git")
            .args(["clone", "-q", "--bare"])
            .arg(&work)
            .arg(&bare)
            .stdout(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        let bare = Repository::discover(&bare).unwrap();
        assert!(bare.bare);
        let tree = bare.materialize_commit("HEAD").unwrap();
        assert_eq!(
            fs::read_to_string(tree.path().join("tracked")).unwrap(),
            "hello\n"
        );
    }
}
