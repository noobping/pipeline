#![cfg(unix)]

pub mod cli;
pub mod definition;
pub mod error;
pub mod hooks;
pub mod just_runtime;
pub mod policy;

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, ExitStatus, Stdio};

use clap::Parser;
use definition::{
    DefinitionError, DefinitionKind, JobLimit, PreparedRun, Project, RunOptions, StdinSource,
};
use error::{PipelineError, Result};
use hooks::{
    hook_environment, incoming_commits, install_hooks, parse_ref_updates, preflight_install,
    remove_hooks, HookError, HookSelection, InstallMode, InstallModes, InstallSources, RefUpdate,
    Repository,
};
use just_runtime::{JustRuntime, RuntimeConfig};
use policy::{IncomingPolicy, Policy};
use tempfile::NamedTempFile;

const JUST_IMAGE: &str = env!("PIPELINE_JUST_IMAGE");
const JUST_MIN_VERSION: &str = env!("PIPELINE_JUST_MIN_VERSION");
const MAX_REF_INPUT: u64 = 16 * 1024 * 1024;

pub fn entrypoint(argv: Vec<OsString>) -> i32 {
    let cli = match cli::Cli::try_parse_from(cli::rewrite_argv(argv)) {
        Ok(cli) => cli,
        Err(error) => {
            let code = error.exit_code();
            let _ = error.print();
            return code;
        }
    };

    match dispatch(cli) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("pipeline: {error}");
            1
        }
    }
}

fn dispatch(cli: cli::Cli) -> Result<i32> {
    match cli.command {
        cli::Command::Run(arguments) => run_direct(arguments),
        cli::Command::Add(arguments) => add_hooks(arguments),
        cli::Command::Remove(arguments) => remove_installed_hooks(arguments),
        cli::Command::Hook(arguments) => run_hook(arguments),
    }
}

fn run_direct(arguments: cli::RunArgs) -> Result<i32> {
    let project = Project::discover(std::env::current_dir()?)?;
    let runtime = resolve_just(None, true)?;
    let options = RunOptions {
        arguments: arguments.arguments,
        job_limit: match (arguments.jobs, arguments.cap_jobs) {
            (Some(jobs), None) => JobLimit::PerProcess(jobs),
            (None, Some(jobs)) => JobLimit::Capped(jobs),
            (None, None) => JobLimit::Default,
            (Some(_), Some(_)) => unreachable!("clap rejects conflicting job limits"),
        },
        no_deps: arguments.no_deps,
        ..RunOptions::default()
    };
    Ok(exit_code(project.run(runtime.executable(), &options)?))
}

fn add_hooks(arguments: cli::AddArgs) -> Result<i32> {
    let repository = Repository::discover(std::env::current_dir()?)?;
    let selection = HookSelection::from_cli(&arguments.hooks, arguments.managed)?;
    let declared = if matches!(selection, HookSelection::Managed) {
        declared_hooks(&repository)?
    } else {
        BTreeSet::new()
    };
    let selected = selection.for_add(&declared)?;
    if selected.is_empty() {
        println!("No hooks selected.");
        return Ok(0);
    }
    let modes = install_modes(&arguments);
    let pipeline_source = std::env::current_exe()?;
    preflight_install(&repository, &selected, &pipeline_source, modes.pipeline)?;

    let policy = Policy::load(&repository.common_dir)?;
    // An explicit link must resolve to the user's PATH executable. Do not pull
    // an image only to discover afterwards that the extracted binary cannot be
    // linked under the requested semantics.
    let runtime = resolve_just(
        None,
        policy.runtime.container_fallback && modes.just != InstallMode::Link,
    )?;
    let sources = InstallSources {
        pipeline: pipeline_source,
        just: runtime.executable().to_path_buf(),
    };
    let report = install_hooks(&repository, &selected, &sources, modes)?;
    println!(
        "Added {} Pipeline-managed Git hook{}.",
        report.hooks.len(),
        if report.hooks.len() == 1 { "" } else { "s" }
    );
    Ok(0)
}

fn remove_installed_hooks(arguments: cli::RemoveArgs) -> Result<i32> {
    let repository = Repository::discover(std::env::current_dir()?)?;
    let selection = HookSelection::from_cli(&arguments.hooks, arguments.managed)?;
    let declared = if matches!(selection, HookSelection::Managed) {
        declared_hooks(&repository)?
    } else {
        BTreeSet::new()
    };
    let selected = selection.for_remove(&repository, &declared)?;
    let report = remove_hooks(&repository, &selected)?;
    println!(
        "Removed {} Pipeline-managed Git hook{}.",
        report.hooks.len(),
        if report.hooks.len() == 1 { "" } else { "s" }
    );
    Ok(0)
}

fn install_modes(arguments: &cli::AddArgs) -> InstallModes {
    let global = if arguments.link {
        InstallMode::Link
    } else if arguments.copy {
        InstallMode::Copy
    } else {
        InstallMode::Auto
    };
    let pipeline = match (arguments.link_pipeline, arguments.copy_pipeline) {
        (true, false) => Some(InstallMode::Link),
        (false, true) => Some(InstallMode::Copy),
        (false, false) => None,
        (true, true) => unreachable!("clap rejects conflicting Pipeline modes"),
    };
    let just = match (arguments.link_just, arguments.copy_just) {
        (true, false) => Some(InstallMode::Link),
        (false, true) => Some(InstallMode::Copy),
        (false, false) => None,
        (true, true) => unreachable!("clap rejects conflicting Just modes"),
    };
    InstallModes::new(global, pipeline, just)
}

fn declared_hooks(repository: &Repository) -> Result<BTreeSet<String>> {
    let project = if let Some(work_tree) = &repository.work_tree {
        let Some(project) = discover_optional(work_tree, work_tree)? else {
            return Ok(BTreeSet::new());
        };
        let declared = project.managed_hooks();
        if project.kind() == DefinitionKind::Yaml && declared.is_empty() {
            verify_tracked_paths(
                repository,
                [project.definition_path_with_boundary(work_tree)?],
            )?;
        } else {
            verify_tracked_control(repository, &project, work_tree)?;
        }
        project
    } else {
        let tree = match repository.materialize_commit("HEAD") {
            Ok(tree) => tree,
            Err(HookError::InvalidRevision(_)) => return Ok(BTreeSet::new()),
            Err(error) => return Err(error.into()),
        };
        let Some(project) = discover_optional(tree.path(), tree.path())? else {
            return Ok(BTreeSet::new());
        };
        let declared = project.managed_hooks();
        if project.kind() == DefinitionKind::Yaml && declared.is_empty() {
            project.definition_path_with_boundary(tree.path())?;
        } else {
            project.control_paths_with_boundary(tree.path())?;
        }
        project
    };
    Ok(project.managed_hooks().into_iter().collect())
}

fn run_hook(arguments: cli::HookArgs) -> Result<i32> {
    let directory = std::env::current_dir()?;
    let repository = Repository::discover(directory)?;
    let policy = Policy::load(&repository.common_dir)?;
    let mut base_environment = hook_environment(&arguments.hook, &arguments.arguments)?;

    if is_receive_hook(&arguments.hook)
        || (arguments.hook == "reference-transaction" && repository.bare)
    {
        return run_receive_hook(
            &repository,
            &policy,
            &arguments.hook,
            &arguments.arguments,
            base_environment,
        );
    }
    if arguments.hook == "proc-receive" {
        return run_protocol_hook(&repository, &policy, &arguments.hook, base_environment);
    }
    if arguments.hook == "fsmonitor-watchman" {
        return run_fsmonitor_hook(&repository, &policy, &arguments.arguments, base_environment);
    }

    if !policy.hook_enabled(&arguments.hook) {
        return Ok(0);
    }
    if let Some(work_tree) = &repository.work_tree {
        let Some(project) = discover_optional(work_tree, work_tree)? else {
            return missing_trigger(&policy, &arguments.hook);
        };
        verify_tracked_hook_control(&repository, &project, work_tree, &arguments.hook)?;
        add_git_environment(&mut base_environment, &repository, work_tree, None);
        return run_project_hook(
            &repository,
            &policy,
            project,
            work_tree.clone(),
            &arguments.hook,
            base_environment,
            StdinSource::Inherit,
        );
    }

    run_bare_hook(&repository, &policy, &arguments.hook, base_environment)
}

fn run_bare_hook(
    repository: &Repository,
    policy: &Policy,
    hook: &str,
    mut environment: Vec<(OsString, OsString)>,
) -> Result<i32> {
    let reference = if policy.hooks.incoming == IncomingPolicy::Trusted {
        policy
            .hooks
            .trusted_ref
            .as_deref()
            .expect("trusted policy was validated with a reference")
    } else {
        "HEAD"
    };
    let tree = match repository.materialize_commit(reference) {
        Ok(tree) => tree,
        Err(HookError::InvalidRevision(_))
            if policy.hooks.incoming != IncomingPolicy::Trusted
                && policy.missing_trigger_is_success() =>
        {
            return Ok(0);
        }
        Err(error) => return Err(error.into()),
    };
    let Some(project) = discover_optional(tree.path(), tree.path())? else {
        return missing_trigger(policy, hook);
    };
    validate_hook_control(&project, tree.path(), hook)?;
    add_git_environment(
        &mut environment,
        repository,
        tree.path(),
        Some(tree.index_path()),
    );
    run_project_hook(
        repository,
        policy,
        project,
        tree.path().to_path_buf(),
        hook,
        environment,
        StdinSource::Inherit,
    )
}

/// An absent fsmonitor hook means Git must scan the worktree itself. Since
/// Pipeline installs wrappers for all supported hooks by default, its no-op
/// path must explicitly report every path as changed instead of emitting an
/// empty (and therefore unsafe) answer.
fn run_fsmonitor_hook(
    repository: &Repository,
    policy: &Policy,
    arguments: &[OsString],
    mut environment: Vec<(OsString, OsString)>,
) -> Result<i32> {
    if !policy.hook_enabled("fsmonitor-watchman") {
        return fsmonitor_fallback(arguments);
    }
    let Some(work_tree) = &repository.work_tree else {
        return fsmonitor_fallback(arguments);
    };
    let Some(project) = discover_optional(work_tree, work_tree)? else {
        return if policy.missing_trigger_is_success() {
            fsmonitor_fallback(arguments)
        } else {
            missing_trigger(policy, "fsmonitor-watchman")
        };
    };

    verify_tracked_hook_control(repository, &project, work_tree, "fsmonitor-watchman")?;
    add_git_environment(&mut environment, repository, work_tree, None);
    let has_trigger = match project.kind() {
        DefinitionKind::Yaml => !project.selection_for_hook("fsmonitor-watchman")?.is_empty(),
        DefinitionKind::Pipelinefile => {
            let runtime = resolve_just(
                Some(repository.installed_just()),
                policy.runtime.container_fallback,
            )?;
            pipelinefile_has_hook(runtime.executable(), &project, work_tree, &environment)?
        }
    };
    if !has_trigger {
        return if policy.missing_trigger_is_success() {
            fsmonitor_fallback(arguments)
        } else {
            missing_trigger(policy, "fsmonitor-watchman")
        };
    }

    run_project_hook(
        repository,
        policy,
        project,
        work_tree.clone(),
        "fsmonitor-watchman",
        environment,
        StdinSource::Inherit,
    )
}

fn fsmonitor_fallback(arguments: &[OsString]) -> Result<i32> {
    let mut output = std::io::stdout().lock();
    fsmonitor_fallback_io(arguments, &mut output)
}

fn fsmonitor_fallback_io(arguments: &[OsString], output: &mut impl Write) -> Result<i32> {
    match arguments.first().and_then(|argument| argument.to_str()) {
        Some("1") => output.write_all(b"/\0")?,
        Some("2") => {
            let Some(token) = arguments.get(1) else {
                // Git treats a non-zero hook status as a request to perform a
                // full scan, which is safer than inventing a v2 clock token.
                return Ok(1);
            };
            output.write_all(token.as_os_str().as_bytes())?;
            output.write_all(b"\0/\0")?;
        }
        _ => return Ok(1),
    }
    output.flush()?;
    Ok(0)
}

fn run_receive_hook(
    repository: &Repository,
    policy: &Policy,
    hook: &str,
    arguments: &[OsString],
    base_environment: Vec<(OsString, OsString)>,
) -> Result<i32> {
    if !policy.hook_enabled(hook) || policy.hooks.incoming == IncomingPolicy::Skip {
        return receive_success(repository, hook, arguments);
    }
    let incoming = IncomingContext::read(repository, hook, arguments)?;
    if incoming.commits.is_empty() {
        // Deletes and non-commit object updates intentionally do not run tasks.
        return receive_success(repository, hook, arguments);
    }

    let mut common_environment = base_environment;
    common_environment.push((
        "PIPELINE_REF_COUNT".into(),
        incoming.refs.len().to_string().into(),
    ));
    for (index, reference) in incoming.refs.iter().enumerate() {
        common_environment.push((format!("PIPELINE_REF_{index}").into(), reference.into()));
    }

    let trusted = if policy.hooks.incoming == IncomingPolicy::Trusted {
        let reference = policy
            .hooks
            .trusted_ref
            .as_deref()
            .expect("trusted policy was validated with a reference");
        let tree = repository.materialize_commit(reference)?;
        let project = discover_optional(tree.path(), tree.path())?;
        if let Some(project) = &project {
            validate_hook_control(project, tree.path(), hook)?;
        }
        Some((tree, project))
    } else {
        None
    };

    let mut candidates = Vec::new();
    let mut incoming_trees = Vec::new();
    for commit in &incoming.commits {
        let tree = repository.materialize_commit(commit)?;
        let project = match &trusted {
            Some((_, project)) => project.clone(),
            None => {
                let project = discover_optional(tree.path(), tree.path())?;
                if let Some(project) = &project {
                    validate_hook_control(project, tree.path(), hook)?;
                }
                project
            }
        };
        if let Some(project) = project {
            let mut environment = common_environment.clone();
            environment.push(("PIPELINE_COMMIT".into(), commit.into()));
            add_git_environment(
                &mut environment,
                repository,
                tree.path(),
                Some(tree.index_path()),
            );
            candidates.push(HookCandidate {
                project,
                working_directory: tree.path().to_path_buf(),
                environment,
            });
        } else if !policy.missing_trigger_is_success() {
            return Err(PipelineError::Message(format!(
                "no Pipeline definition found for {hook} at commit {commit}"
            )));
        }
        incoming_trees.push(tree);
    }

    let _keep_trees_alive = (&trusted, &incoming_trees);
    let result = run_candidates(
        repository,
        policy,
        hook,
        candidates,
        incoming.stdin.source(),
        policy.hooks.parallel_refs,
    )?;
    if result == 0 {
        receive_success(repository, hook, arguments)
    } else {
        Ok(result)
    }
}

fn receive_success(repository: &Repository, hook: &str, arguments: &[OsString]) -> Result<i32> {
    if hook != "push-to-checkout" {
        return Ok(0);
    }
    let Some(work_tree) = &repository.work_tree else {
        return Err(PipelineError::Message(
            "push-to-checkout requires a non-bare worktree".to_owned(),
        ));
    };
    let Some(commit) = arguments.first() else {
        return Err(PipelineError::Message(
            "the push-to-checkout hook requires a new object ID".to_owned(),
        ));
    };
    let status = ProcessCommand::new("git")
        .arg(format!("--git-dir={}", repository.git_dir.display()))
        .arg(format!("--work-tree={}", work_tree.display()))
        .args(["read-tree", "-u", "-m", "HEAD"])
        .arg(commit)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()?;
    Ok(exit_code(status))
}

/// `proc-receive` is interactive. Pipeline leaves its protocol on inherited
/// stdio and uses the current/trusted tree instead of consuming it as ref input.
fn run_protocol_hook(
    repository: &Repository,
    policy: &Policy,
    hook: &str,
    mut environment: Vec<(OsString, OsString)>,
) -> Result<i32> {
    if !policy.hook_enabled(hook) || policy.hooks.incoming == IncomingPolicy::Skip {
        return proc_receive_fallthrough();
    }
    let reference = if policy.hooks.incoming == IncomingPolicy::Trusted {
        policy
            .hooks
            .trusted_ref
            .as_deref()
            .expect("trusted policy was validated with a reference")
    } else {
        "HEAD"
    };
    let tree = match repository.materialize_commit(reference) {
        Ok(tree) => tree,
        Err(HookError::InvalidRevision(_))
            if policy.hooks.incoming == IncomingPolicy::Project
                && policy.missing_trigger_is_success() =>
        {
            return proc_receive_fallthrough();
        }
        Err(error) => return Err(error.into()),
    };
    let Some(project) = discover_optional(tree.path(), tree.path())? else {
        return if policy.missing_trigger_is_success() {
            proc_receive_fallthrough()
        } else {
            missing_trigger(policy, hook)
        };
    };
    validate_hook_control(&project, tree.path(), hook)?;
    add_git_environment(
        &mut environment,
        repository,
        tree.path(),
        Some(tree.index_path()),
    );

    let has_trigger = match project.kind() {
        DefinitionKind::Yaml => !project.selection_for_hook(hook)?.is_empty(),
        DefinitionKind::Pipelinefile => {
            let runtime = resolve_just(
                Some(repository.installed_just()),
                policy.runtime.container_fallback,
            )?;
            pipelinefile_has_hook(runtime.executable(), &project, tree.path(), &environment)?
        }
    };
    if !has_trigger {
        return if policy.missing_trigger_is_success() {
            proc_receive_fallthrough()
        } else {
            missing_trigger(policy, hook)
        };
    }
    run_project_hook(
        repository,
        policy,
        project,
        tree.path().to_path_buf(),
        hook,
        environment,
        StdinSource::Inherit,
    )
}

/// Behave like an absent `proc-receive` hook by negotiating protocol v1 and
/// returning every command to receive-pack with `option fall-through`.
fn proc_receive_fallthrough() -> Result<i32> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut input = stdin.lock();
    let mut output = stdout.lock();
    proc_receive_fallthrough_io(&mut input, &mut output)?;
    Ok(0)
}

fn proc_receive_fallthrough_io(input: &mut impl Read, output: &mut impl Write) -> Result<()> {
    let mut read_bytes = 0_u64;

    let mut offered_atomic = false;
    loop {
        match read_packet(input, &mut read_bytes)? {
            Packet::Flush => break,
            Packet::Data(data) => {
                offered_atomic |=
                    data.split(|byte| *byte == b'\0')
                        .nth(1)
                        .is_some_and(|features| {
                            features
                                .split(|byte| byte.is_ascii_whitespace())
                                .filter(|feature| !feature.is_empty())
                                .any(|feature| feature == b"atomic")
                        });
            }
        }
    }
    let response: &[u8] = if offered_atomic {
        b"version=1\0atomic\n"
    } else {
        b"version=1\0\n"
    };
    write_packet(output, response)?;
    write_flush(output)?;
    output.flush()?;

    let mut references = Vec::new();
    loop {
        match read_packet(input, &mut read_bytes)? {
            Packet::Flush => break,
            Packet::Data(data) => {
                let line = data.strip_suffix(b"\n").unwrap_or(&data);
                let reference = line
                    .splitn(3, |byte| *byte == b' ')
                    .nth(2)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| {
                        PipelineError::Message(
                            "invalid command in proc-receive protocol".to_owned(),
                        )
                    })?;
                references.push(reference.to_vec());
            }
        }
    }

    for reference in references {
        let mut ok = b"ok ".to_vec();
        ok.extend_from_slice(&reference);
        ok.push(b'\n');
        write_packet(output, &ok)?;
        write_packet(output, b"option fall-through\n")?;
    }
    write_flush(output)?;
    output.flush()?;
    Ok(())
}

enum Packet {
    Flush,
    Data(Vec<u8>),
}

fn read_packet(input: &mut impl Read, total: &mut u64) -> Result<Packet> {
    let mut header = [0_u8; 4];
    input.read_exact(&mut header)?;
    let header = std::str::from_utf8(&header)
        .ok()
        .and_then(|value| usize::from_str_radix(value, 16).ok())
        .ok_or_else(|| PipelineError::Message("invalid proc-receive packet header".to_owned()))?;
    if header == 0 {
        return Ok(Packet::Flush);
    }
    if header < 4 {
        return Err(PipelineError::Message(
            "unsupported proc-receive control packet".to_owned(),
        ));
    }
    let payload_length = header - 4;
    *total += payload_length as u64;
    if *total > MAX_REF_INPUT {
        return Err(PipelineError::Message(format!(
            "proc-receive input exceeds {} MiB",
            MAX_REF_INPUT / 1024 / 1024
        )));
    }
    let mut data = vec![0; payload_length];
    input.read_exact(&mut data)?;
    Ok(Packet::Data(data))
}

fn write_packet(output: &mut impl Write, payload: &[u8]) -> Result<()> {
    let length = payload.len() + 4;
    if length > 0xffff {
        return Err(PipelineError::Message(
            "proc-receive packet is too large".to_owned(),
        ));
    }
    write!(output, "{length:04x}")?;
    output.write_all(payload)?;
    Ok(())
}

fn write_flush(output: &mut impl Write) -> Result<()> {
    output.write_all(b"0000")?;
    Ok(())
}

fn run_project_hook(
    repository: &Repository,
    policy: &Policy,
    project: Project,
    working_directory: PathBuf,
    hook: &str,
    environment: Vec<(OsString, OsString)>,
    stdin: StdinSource,
) -> Result<i32> {
    run_candidates(
        repository,
        policy,
        hook,
        vec![HookCandidate {
            project,
            working_directory,
            environment,
        }],
        stdin,
        false,
    )
}

#[derive(Clone)]
struct HookCandidate {
    project: Project,
    working_directory: PathBuf,
    environment: Vec<(OsString, OsString)>,
}

fn run_candidates(
    repository: &Repository,
    policy: &Policy,
    hook: &str,
    candidates: Vec<HookCandidate>,
    stdin: StdinSource,
    parallel: bool,
) -> Result<i32> {
    let mut selected = Vec::new();
    for candidate in candidates {
        match candidate.project.kind() {
            DefinitionKind::Pipelinefile => selected.push((candidate, None)),
            DefinitionKind::Yaml => {
                let selection = candidate.project.selection_for_hook(hook)?;
                if selection.is_empty() {
                    if !policy.missing_trigger_is_success() {
                        return missing_trigger(policy, hook);
                    }
                } else {
                    selected.push((candidate, Some(selection)));
                }
            }
        }
    }
    if selected.is_empty() {
        return missing_trigger(policy, hook);
    }

    let runtime = resolve_just(
        Some(repository.installed_just()),
        policy.runtime.container_fallback,
    )?;
    let mut prepared = Vec::new();
    for (candidate, selection) in selected {
        if selection.is_none()
            && !pipelinefile_has_hook(
                runtime.executable(),
                &candidate.project,
                &candidate.working_directory,
                &candidate.environment,
            )?
        {
            if policy.missing_trigger_is_success() {
                continue;
            }
            return missing_trigger(policy, hook);
        }
        let options = RunOptions {
            arguments: if selection.is_none() {
                vec![OsString::from("hook")]
            } else {
                Vec::new()
            },
            environment: candidate.environment,
            stdin: stdin.clone(),
            working_directory: Some(candidate.working_directory),
            ..RunOptions::default()
        };
        let run = match selection {
            Some(selection) => {
                candidate
                    .project
                    .prepare_selection(&selection, runtime.executable(), &options)?
            }
            None => candidate.project.prepare(runtime.executable(), &options)?,
        };
        prepared.push(run);
    }
    if prepared.is_empty() {
        return missing_trigger(policy, hook);
    }
    execute_prepared(&prepared, parallel)
}

fn pipelinefile_has_hook(
    just: &Path,
    project: &Project,
    working_directory: &Path,
    environment: &[(OsString, OsString)],
) -> Result<bool> {
    let output = ProcessCommand::new(just)
        .args([OsStr::new("--justfile"), project.path().as_os_str()])
        .args([
            OsStr::new("--working-directory"),
            working_directory.as_os_str(),
        ])
        .args([OsStr::new("--show"), OsStr::new("hook")])
        .envs(environment.iter().cloned())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()?;
    if output.status.success() {
        return Ok(true);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("does not contain recipe `hook`")
        || stderr.contains("unknown recipe `hook`")
        || stderr.contains("Unknown recipe `hook`")
    {
        Ok(false)
    } else {
        Err(PipelineError::Message(format!(
            "could not inspect {}: {}",
            project.path().display(),
            stderr.trim()
        )))
    }
}

fn execute_prepared(prepared: &[PreparedRun], parallel: bool) -> Result<i32> {
    if !parallel || prepared.len() < 2 {
        for run in prepared {
            let code = exit_code(run.status()?);
            if code != 0 {
                return Ok(code);
            }
        }
        return Ok(0);
    }

    let mut children = Vec::with_capacity(prepared.len());
    for run in prepared {
        match run.command()?.spawn() {
            Ok(child) => children.push(child),
            Err(error) => {
                for child in &mut children {
                    let _ = child.wait();
                }
                return Err(error.into());
            }
        }
    }
    let mut result = 0;
    for child in &mut children {
        let code = exit_code(child.wait()?);
        if result == 0 && code != 0 {
            result = code;
        }
    }
    Ok(result)
}

struct IncomingContext {
    commits: Vec<String>,
    refs: Vec<String>,
    stdin: ReplayInput,
}

impl IncomingContext {
    fn read(repository: &Repository, hook: &str, arguments: &[OsString]) -> Result<Self> {
        let mut stdin = ReplayInput::inherit();
        let updates = match hook {
            "pre-receive" | "post-receive" => {
                stdin = ReplayInput::read()?;
                parse_ref_updates(stdin.bytes())?
            }
            "reference-transaction" if repository.bare => {
                stdin = ReplayInput::read()?;
                parse_ref_updates(stdin.bytes())?
            }
            "update" => vec![update_from_arguments(arguments)?],
            "post-update" => updates_from_refs(repository, arguments),
            "push-to-checkout" => update_from_checkout(arguments)?,
            _ => Vec::new(),
        };
        let refs = updates
            .iter()
            .map(|update| update.reference.clone())
            .collect();
        let commits = incoming_commits(repository, &updates)?;
        Ok(Self {
            commits,
            refs,
            stdin,
        })
    }
}

enum ReplayInput {
    Inherit,
    File { file: NamedTempFile, bytes: Vec<u8> },
}

impl ReplayInput {
    fn inherit() -> Self {
        Self::Inherit
    }

    fn read() -> Result<Self> {
        let mut bytes = Vec::new();
        std::io::stdin()
            .lock()
            .take(MAX_REF_INPUT + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_REF_INPUT {
            return Err(PipelineError::Message(format!(
                "Git hook input exceeds {} MiB",
                MAX_REF_INPUT / 1024 / 1024
            )));
        }
        let mut file = tempfile::Builder::new()
            .prefix("pipeline-hook-input-")
            .tempfile()?;
        file.write_all(&bytes)?;
        file.flush()?;
        Ok(Self::File { file, bytes })
    }

    fn bytes(&self) -> &[u8] {
        match self {
            Self::Inherit => &[],
            Self::File { bytes, .. } => bytes,
        }
    }

    fn source(&self) -> StdinSource {
        match self {
            Self::Inherit => StdinSource::Inherit,
            Self::File { file, .. } => StdinSource::File(file.path().to_path_buf()),
        }
    }
}

fn update_from_arguments(arguments: &[OsString]) -> Result<RefUpdate> {
    if arguments.len() != 3 {
        return Err(PipelineError::Message(
            "the update hook requires REF OLD-OID NEW-OID arguments".to_owned(),
        ));
    }
    Ok(RefUpdate {
        reference: unicode_argument(&arguments[0], "reference")?,
        old_oid: unicode_argument(&arguments[1], "old object ID")?,
        new_oid: unicode_argument(&arguments[2], "new object ID")?,
    })
}

fn updates_from_refs(repository: &Repository, arguments: &[OsString]) -> Vec<RefUpdate> {
    arguments
        .iter()
        .filter_map(|argument| argument.to_str())
        .filter_map(|reference| {
            repository
                .resolve_commit(reference)
                .ok()
                .map(|new_oid| RefUpdate {
                    old_oid: String::new(),
                    new_oid,
                    reference: reference.to_owned(),
                })
        })
        .collect()
}

fn update_from_checkout(arguments: &[OsString]) -> Result<Vec<RefUpdate>> {
    let Some(new_oid) = arguments.first() else {
        return Err(PipelineError::Message(
            "the push-to-checkout hook requires a new object ID".to_owned(),
        ));
    };
    Ok(vec![RefUpdate {
        old_oid: String::new(),
        new_oid: unicode_argument(new_oid, "new object ID")?,
        reference: "HEAD".to_owned(),
    }])
}

fn unicode_argument(argument: &OsStr, description: &str) -> Result<String> {
    argument
        .to_str()
        .map(str::to_owned)
        .ok_or_else(|| PipelineError::Message(format!("Git supplied a non-UTF-8 {description}")))
}

fn is_receive_hook(hook: &str) -> bool {
    matches!(
        hook,
        "pre-receive" | "update" | "post-receive" | "post-update" | "push-to-checkout"
    )
}

fn discover_optional(start: &Path, boundary: &Path) -> Result<Option<Project>> {
    match Project::discover_with_boundary(start, boundary) {
        Ok(project) => Ok(Some(project)),
        Err(DefinitionError::NotFound { .. }) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn verify_tracked_control(
    repository: &Repository,
    project: &Project,
    boundary: &Path,
) -> Result<()> {
    verify_tracked_paths(repository, project.control_paths_with_boundary(boundary)?)
}

fn verify_tracked_hook_control(
    repository: &Repository,
    project: &Project,
    boundary: &Path,
    hook: &str,
) -> Result<()> {
    verify_tracked_paths(repository, hook_control_paths(project, boundary, hook)?)
}

fn verify_tracked_paths(
    repository: &Repository,
    paths: impl IntoIterator<Item = PathBuf>,
) -> Result<()> {
    for path in paths {
        if !repository.is_tracked(&path)? {
            return Err(PipelineError::Message(format!(
                "hook control file is not tracked by Git: {}",
                path.display()
            )));
        }
    }
    Ok(())
}

fn validate_hook_control(project: &Project, boundary: &Path, hook: &str) -> Result<()> {
    hook_control_paths(project, boundary, hook).map(|_| ())
}

fn hook_control_paths(project: &Project, boundary: &Path, hook: &str) -> Result<Vec<PathBuf>> {
    if project.kind() == DefinitionKind::Yaml && project.selection_for_hook(hook)?.is_empty() {
        Ok(vec![project.definition_path_with_boundary(boundary)?])
    } else {
        Ok(project.control_paths_with_boundary(boundary)?)
    }
}

fn add_git_environment(
    environment: &mut Vec<(OsString, OsString)>,
    repository: &Repository,
    work_tree: &Path,
    index: Option<&Path>,
) {
    environment.push(("GIT_DIR".into(), repository.git_dir.as_os_str().to_owned()));
    environment.push((
        "GIT_COMMON_DIR".into(),
        repository.common_dir.as_os_str().to_owned(),
    ));
    environment.push(("GIT_WORK_TREE".into(), work_tree.as_os_str().to_owned()));
    if let Some(index) = index {
        environment.push(("GIT_INDEX_FILE".into(), index.as_os_str().to_owned()));
        environment.push(("GIT_LFS_SKIP_SMUDGE".into(), "1".into()));
    }
}

fn missing_trigger(policy: &Policy, hook: &str) -> Result<i32> {
    if policy.missing_trigger_is_success() {
        Ok(0)
    } else {
        Err(PipelineError::Message(format!(
            "no Pipeline trigger is declared for Git hook `{hook}`"
        )))
    }
}

fn resolve_just(installed: Option<PathBuf>, container_fallback: bool) -> Result<JustRuntime> {
    let config = RuntimeConfig::new(JUST_IMAGE, JUST_MIN_VERSION)?
        .with_installed_just(installed)
        .with_container_fallback(container_fallback);
    Ok(JustRuntime::resolve(&config)?)
}

fn exit_code(status: ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;

    status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(payload: &[u8], output: &mut Vec<u8>) {
        write!(output, "{:04x}", payload.len() + 4).unwrap();
        output.extend_from_slice(payload);
    }

    #[test]
    fn proc_receive_missing_trigger_falls_through() {
        let mut input = Vec::new();
        packet(b"version=1\0atomic push-options\n", &mut input);
        input.extend_from_slice(b"0000");
        packet(
            b"0000000000000000000000000000000000000000 1111111111111111111111111111111111111111 refs/for/main\n",
            &mut input,
        );
        input.extend_from_slice(b"0000");

        let mut output = Vec::new();
        proc_receive_fallthrough_io(&mut input.as_slice(), &mut output).unwrap();

        let mut expected = Vec::new();
        packet(b"version=1\0atomic\n", &mut expected);
        expected.extend_from_slice(b"0000");
        packet(b"ok refs/for/main\n", &mut expected);
        packet(b"option fall-through\n", &mut expected);
        expected.extend_from_slice(b"0000");
        assert_eq!(output, expected);
    }

    #[test]
    fn fsmonitor_missing_trigger_forces_a_full_scan() {
        let mut output = Vec::new();
        assert_eq!(
            fsmonitor_fallback_io(&["1".into(), "old-clock".into()], &mut output).unwrap(),
            0
        );
        assert_eq!(output, b"/\0");

        output.clear();
        assert_eq!(
            fsmonitor_fallback_io(&["2".into(), "old-clock".into()], &mut output).unwrap(),
            0
        );
        assert_eq!(output, b"old-clock\0/\0");

        output.clear();
        assert_eq!(
            fsmonitor_fallback_io(&["2".into()], &mut output).unwrap(),
            1
        );
        assert!(output.is_empty());
    }

    #[test]
    fn yaml_without_a_hook_trigger_does_not_require_a_justfile() {
        let root = tempfile::tempdir().unwrap();
        let definition = root.path().join("pipeline.yml");
        std::fs::write(
            &definition,
            "version: 1\npipelines:\n  check:\n    jobs:\n      build:\n        just: build\n",
        )
        .unwrap();
        let project = Project::discover(root.path()).unwrap();
        assert_eq!(
            hook_control_paths(&project, root.path(), "pre-commit").unwrap(),
            vec![definition.clone()]
        );

        std::fs::write(
            &definition,
            "version: 1\non:\n  pre-commit: [check]\npipelines:\n  check:\n    jobs:\n      build:\n        just: build\n",
        )
        .unwrap();
        let project = Project::discover(root.path()).unwrap();
        assert!(hook_control_paths(&project, root.path(), "pre-commit").is_err());
    }
}
