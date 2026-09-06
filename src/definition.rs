//! Pipeline definition discovery, validation, selection, and Justfile generation.
//!
//! Pipeline deliberately keeps execution policy small: a `Pipelinefile` is passed
//! straight to Just, while YAML is compiled to a temporary Just dependency graph.

use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{self, Write};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use tempfile::{Builder as TempBuilder, NamedTempFile};
use thiserror::Error;

pub use crate::hooks::SUPPORTED_HOOKS;

/// Definition names, in precedence order within a directory.
pub const DEFINITION_NAMES: &[&str] = &["Pipelinefile", "pipeline.yml", ".pipeline.yml"];

/// Just's conventional definition names, in lookup order.
pub const JUSTFILE_NAMES: &[&str] = &["justfile", "Justfile", ".justfile"];

#[derive(Debug, Error)]
pub enum DefinitionError {
    #[error("no Pipelinefile, pipeline.yml, or .pipeline.yml found from {start}")]
    NotFound { start: PathBuf },

    #[error("no justfile, Justfile, or .justfile found from {start}")]
    JustfileNotFound { start: PathBuf },

    #[error("definition path is not a regular file: {0}")]
    NotAFile(PathBuf),

    #[error("unsupported definition filename `{0}`")]
    UnsupportedFilename(String),

    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("invalid YAML in {path}: {source}")]
    Yaml {
        path: PathBuf,
        #[source]
        source: yaml_serde::Error,
    },

    #[error("invalid pipeline definition: {0}")]
    Invalid(String),

    #[error("definition target is not valid UTF-8: {0:?}")]
    NonUnicodeTarget(OsString),

    #[error("Just executable path is not valid UTF-8: {0:?}")]
    NonUnicodeExecutable(PathBuf),

    #[error("cannot execute `{program}`: {source}")]
    Execute {
        program: PathBuf,
        #[source]
        source: io::Error,
    },
}

pub type Result<T> = std::result::Result<T, DefinitionError>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DefinitionKind {
    Pipelinefile,
    Yaml,
}

#[derive(Clone, Debug)]
enum Definition {
    Pipelinefile,
    Yaml(PipelineConfig),
}

/// A discovered and, for YAML, fully validated project definition.
#[derive(Clone, Debug)]
pub struct Project {
    path: PathBuf,
    root: PathBuf,
    definition: Definition,
}

impl Project {
    /// Search `start` and its ancestors. The nearest directory wins, then
    /// [`DEFINITION_NAMES`] determines precedence within that directory.
    pub fn discover(start: impl AsRef<Path>) -> Result<Self> {
        Self::discover_inner(start.as_ref(), None)
    }

    /// Search upward without walking above `boundary` (which is included).
    pub fn discover_with_boundary(
        start: impl AsRef<Path>,
        boundary: impl AsRef<Path>,
    ) -> Result<Self> {
        Self::discover_inner(start.as_ref(), Some(boundary.as_ref()))
    }

    fn discover_inner(start: &Path, boundary: Option<&Path>) -> Result<Self> {
        let original = start.to_path_buf();
        let mut directory = absolute_existing(start)?;
        if directory.is_file() {
            directory = directory
                .parent()
                .expect("an absolute file path always has a parent")
                .to_path_buf();
        }

        let boundary = match boundary {
            Some(path) => {
                let path = absolute_existing(path)?;
                let path = if path.is_file() {
                    path.parent()
                        .expect("an absolute file path always has a parent")
                        .to_path_buf()
                } else {
                    path
                };
                if !directory.starts_with(&path) {
                    return Err(DefinitionError::Invalid(format!(
                        "search start {} is outside boundary {}",
                        directory.display(),
                        path.display()
                    )));
                }
                Some(path)
            }
            None => None,
        };

        loop {
            for name in DEFINITION_NAMES {
                let candidate = directory.join(name);
                if fs::symlink_metadata(&candidate).is_ok() {
                    return Self::open(candidate);
                }
            }

            if boundary.as_ref().is_some_and(|root| root == &directory) {
                break;
            }
            if !directory.pop() {
                break;
            }
        }

        Err(DefinitionError::NotFound { start: original })
    }

    /// Open one of the three supported definition filenames directly.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let requested = path.as_ref();
        let filename = requested
            .file_name()
            .and_then(OsStr::to_str)
            .ok_or_else(|| DefinitionError::UnsupportedFilename(requested.display().to_string()))?;
        let metadata = fs::metadata(requested).map_err(|source| DefinitionError::Io {
            path: requested.to_path_buf(),
            source,
        })?;
        if !metadata.is_file() {
            return Err(DefinitionError::NotAFile(requested.to_path_buf()));
        }

        // Canonicalize the parent, not the final entry. Keeping the lexical
        // definition path lets Git-hook validation verify that a symlink itself
        // is tracked instead of accidentally validating only its target.
        let requested_parent = requested
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let root = absolute_existing(requested_parent)?;
        let path = root.join(
            requested
                .file_name()
                .expect("the supported filename was read above"),
        );

        let definition = match filename {
            "Pipelinefile" => Definition::Pipelinefile,
            "pipeline.yml" | ".pipeline.yml" => {
                let contents = fs::read_to_string(&path).map_err(|source| DefinitionError::Io {
                    path: path.clone(),
                    source,
                })?;
                let config: PipelineConfig =
                    yaml_serde::from_str(&contents).map_err(|source| DefinitionError::Yaml {
                        path: path.clone(),
                        source,
                    })?;
                config.validate()?;
                Definition::Yaml(config)
            }
            other => return Err(DefinitionError::UnsupportedFilename(other.to_owned())),
        };

        Ok(Self {
            path,
            root,
            definition,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn kind(&self) -> DefinitionKind {
        match self.definition {
            Definition::Pipelinefile => DefinitionKind::Pipelinefile,
            Definition::Yaml(_) => DefinitionKind::Yaml,
        }
    }

    pub fn yaml(&self) -> Option<&PipelineConfig> {
        match &self.definition {
            Definition::Pipelinefile => None,
            Definition::Yaml(config) => Some(config),
        }
    }

    /// Resolve ordinary command-line arguments to a YAML selection.
    pub fn select(&self, arguments: &[OsString]) -> Result<Selection> {
        match &self.definition {
            Definition::Pipelinefile => Err(DefinitionError::Invalid(
                "Pipelinefile targets are passed directly to Just".to_owned(),
            )),
            Definition::Yaml(config) => config.select_pipelines(arguments),
        }
    }

    pub fn selection_for_hook(&self, hook: &str) -> Result<Selection> {
        match &self.definition {
            Definition::Pipelinefile => Err(DefinitionError::Invalid(
                "Pipelinefile hooks use the universal `hook` recipe".to_owned(),
            )),
            Definition::Yaml(config) => config.selection_for_hook(hook),
        }
    }

    pub fn managed_hooks(&self) -> Vec<String> {
        match &self.definition {
            Definition::Pipelinefile => SUPPORTED_HOOKS
                .iter()
                .map(|hook| (*hook).to_owned())
                .collect(),
            Definition::Yaml(config) => config.managed_hooks(),
        }
    }

    /// Return every directly loaded project control file. For YAML this also
    /// resolves the ordinary Justfile used by nested recipe invocations.
    pub fn control_paths(&self) -> Result<Vec<PathBuf>> {
        self.control_paths_inner(None)
    }

    /// As [`Project::control_paths`], but reject a control file outside
    /// `boundary`. Hook dispatch uses this before checking that paths are tracked.
    pub fn control_paths_with_boundary(&self, boundary: impl AsRef<Path>) -> Result<Vec<PathBuf>> {
        self.control_paths_inner(Some(boundary.as_ref()))
    }

    /// Return only the selected Pipeline definition after applying the same
    /// lexical and resolved-path boundary checks as [`Project::control_paths`].
    /// This lets a hook with no YAML trigger skip safely without requiring an
    /// unrelated Justfile to exist.
    pub fn definition_path_with_boundary(&self, boundary: impl AsRef<Path>) -> Result<PathBuf> {
        let boundary = absolute_existing(boundary.as_ref())?;
        self.validate_definition_boundary(Some(&boundary))?;
        Ok(self.path.clone())
    }

    fn control_paths_inner(&self, boundary: Option<&Path>) -> Result<Vec<PathBuf>> {
        let boundary = boundary.map(absolute_existing).transpose()?;
        self.validate_definition_boundary(boundary.as_deref())?;

        let mut paths = vec![self.path.clone()];
        if matches!(self.definition, Definition::Yaml(_)) {
            paths.push(discover_justfile(&self.root, boundary.as_deref())?);
        }
        Ok(paths)
    }

    fn validate_definition_boundary(&self, boundary: Option<&Path>) -> Result<()> {
        if boundary
            .as_ref()
            .is_some_and(|root| !self.path.starts_with(root))
        {
            return invalid(format!(
                "definition {} is outside control boundary {}",
                self.path.display(),
                boundary.as_ref().expect("checked above").display()
            ));
        }
        if let Some(boundary) = &boundary {
            let resolved = fs::canonicalize(&self.path).map_err(|source| DefinitionError::Io {
                path: self.path.clone(),
                source,
            })?;
            if !resolved.starts_with(boundary) {
                return invalid(format!(
                    "definition {} resolves outside control boundary {}",
                    self.path.display(),
                    boundary.display()
                ));
            }
        }
        Ok(())
    }

    /// Render a YAML selection without writing it to disk.
    pub fn render(
        &self,
        selection: &Selection,
        just_executable: &Path,
        job_limit: JobLimit,
        no_deps: bool,
    ) -> Result<RenderedPipeline> {
        match &self.definition {
            Definition::Pipelinefile => Err(DefinitionError::Invalid(
                "a Pipelinefile does not need rendering".to_owned(),
            )),
            Definition::Yaml(config) => config.render(
                selection,
                &self.root,
                just_executable,
                Some(&discover_justfile(&self.root, None)?),
                job_limit,
                no_deps,
            ),
        }
    }

    /// Prepare a direct Pipelinefile invocation or a generated YAML invocation.
    pub fn prepare(&self, just_executable: &Path, options: &RunOptions) -> Result<PreparedRun> {
        match &self.definition {
            Definition::Pipelinefile => {
                let working_directory = options
                    .working_directory
                    .clone()
                    .unwrap_or_else(|| self.root.clone());
                let mut arguments = vec![
                    OsString::from("--justfile"),
                    self.path.as_os_str().to_owned(),
                    OsString::from("--working-directory"),
                    working_directory.as_os_str().to_owned(),
                ];
                append_outer_options(&mut arguments, options.job_limit, options.no_deps);
                arguments.extend(options.arguments.iter().cloned());
                Ok(PreparedRun::new(
                    just_executable.to_path_buf(),
                    arguments,
                    working_directory,
                    options,
                    None,
                ))
            }
            Definition::Yaml(config) => {
                let selection = config.select_pipelines(&options.arguments)?;
                self.prepare_yaml(config, &selection, just_executable, options)
            }
        }
    }

    /// Prepare a YAML run from an explicit selection, used by Git-hook dispatch.
    pub fn prepare_selection(
        &self,
        selection: &Selection,
        just_executable: &Path,
        options: &RunOptions,
    ) -> Result<PreparedRun> {
        match &self.definition {
            Definition::Pipelinefile => Err(DefinitionError::Invalid(
                "explicit selections only apply to YAML definitions".to_owned(),
            )),
            Definition::Yaml(config) => {
                self.prepare_yaml(config, selection, just_executable, options)
            }
        }
    }

    fn prepare_yaml(
        &self,
        config: &PipelineConfig,
        selection: &Selection,
        just_executable: &Path,
        options: &RunOptions,
    ) -> Result<PreparedRun> {
        let working_directory = options
            .working_directory
            .clone()
            .unwrap_or_else(|| self.root.clone());
        let control_justfile = discover_justfile(&self.root, None)?;
        let rendered = config.render(
            selection,
            &working_directory,
            just_executable,
            Some(&control_justfile),
            options.job_limit,
            options.no_deps,
        )?;
        let mut file = TempBuilder::new()
            .prefix("pipeline-")
            .suffix(".just")
            .tempfile()
            .map_err(|source| DefinitionError::Io {
                path: std::env::temp_dir(),
                source,
            })?;
        file.write_all(rendered.contents.as_bytes())
            .and_then(|()| file.flush())
            .map_err(|source| DefinitionError::Io {
                path: file.path().to_path_buf(),
                source,
            })?;

        let mut arguments = vec![
            OsString::from("--justfile"),
            file.path().as_os_str().to_owned(),
            OsString::from("--working-directory"),
            rendered.working_directory.as_os_str().to_owned(),
        ];
        append_outer_options(&mut arguments, options.job_limit, false);
        arguments.push(OsString::from(rendered.entrypoint));

        Ok(PreparedRun::new(
            just_executable.to_path_buf(),
            arguments,
            rendered.working_directory,
            options,
            Some(file),
        ))
    }

    pub fn run(&self, just_executable: &Path, options: &RunOptions) -> Result<ExitStatus> {
        self.prepare(just_executable, options)?.status()
    }
}

fn absolute_existing(path: &Path) -> Result<PathBuf> {
    fs::canonicalize(path).map_err(|source| DefinitionError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn discover_justfile(start: &Path, boundary: Option<&Path>) -> Result<PathBuf> {
    let original = start.to_path_buf();
    let mut directory = absolute_existing(start)?;
    let boundary = boundary.map(absolute_existing).transpose()?;
    if boundary
        .as_ref()
        .is_some_and(|root| !directory.starts_with(root))
    {
        return invalid(format!(
            "Justfile search start {} is outside boundary {}",
            directory.display(),
            boundary.as_ref().expect("checked above").display()
        ));
    }

    loop {
        for name in JUSTFILE_NAMES {
            let candidate = directory.join(name);
            if fs::symlink_metadata(&candidate).is_ok() {
                let metadata = fs::metadata(&candidate).map_err(|source| DefinitionError::Io {
                    path: candidate.clone(),
                    source,
                })?;
                if !metadata.is_file() {
                    return Err(DefinitionError::NotAFile(candidate));
                }
                let resolved =
                    fs::canonicalize(&candidate).map_err(|source| DefinitionError::Io {
                        path: candidate.clone(),
                        source,
                    })?;
                if boundary
                    .as_ref()
                    .is_some_and(|root| !resolved.starts_with(root))
                {
                    return invalid(format!(
                        "Justfile {} resolves outside control boundary {}",
                        resolved.display(),
                        boundary.as_ref().expect("checked above").display()
                    ));
                }
                // Return the directory entry, not its canonical target, so Git
                // can verify that this exact control path is tracked.
                return Ok(candidate);
            }
        }
        if boundary.as_ref().is_some_and(|root| root == &directory) || !directory.pop() {
            break;
        }
    }
    Err(DefinitionError::JustfileNotFound { start: original })
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PipelineConfig {
    #[serde(default = "default_version")]
    pub version: u32,

    #[serde(default, rename = "on")]
    pub triggers: IndexMap<String, Vec<String>>,

    pub pipelines: IndexMap<String, PipelineSpec>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PipelineSpec {
    #[serde(default)]
    pub needs: Vec<String>,

    #[serde(default, rename = "on")]
    pub triggers: Vec<String>,

    #[serde(default)]
    pub jobs: IndexMap<String, JobSpec>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct JobSpec {
    pub just: String,

    #[serde(default)]
    pub needs: Vec<String>,

    #[serde(default, rename = "on")]
    pub triggers: Vec<String>,
}

const fn default_version() -> u32 {
    1
}

impl PipelineConfig {
    pub fn from_yaml(input: &str) -> Result<Self> {
        let config: Self = yaml_serde::from_str(input).map_err(|source| DefinitionError::Yaml {
            path: PathBuf::from("<memory>"),
            source,
        })?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        if self.version != 1 {
            return invalid(format!(
                "unsupported version {}; only version 1 is supported",
                self.version
            ));
        }
        if self.pipelines.is_empty() {
            return invalid("`pipelines` must contain at least one pipeline");
        }

        for hook in self.triggers.keys() {
            validate_hook(hook)?;
        }

        for (pipeline_name, pipeline) in &self.pipelines {
            validate_identifier(pipeline_name, "pipeline")?;
            if pipeline.jobs.is_empty() && pipeline.needs.is_empty() {
                return invalid(format!(
                    "pipeline `{pipeline_name}` must contain at least one job or dependency"
                ));
            }
            reject_duplicates(
                &pipeline.needs,
                &format!("pipeline `{pipeline_name}` dependencies"),
            )?;
            reject_duplicates(
                &pipeline.triggers,
                &format!("pipeline `{pipeline_name}` triggers"),
            )?;
            for hook in &pipeline.triggers {
                validate_hook(hook)?;
            }
            for needed in &pipeline.needs {
                if needed == pipeline_name {
                    return invalid(format!("pipeline `{pipeline_name}` depends on itself"));
                }
                if !self.pipelines.contains_key(needed) {
                    return invalid(format!(
                        "pipeline `{pipeline_name}` needs missing pipeline `{needed}`"
                    ));
                }
            }

            for (job_name, job) in &pipeline.jobs {
                validate_identifier(job_name, "job")?;
                validate_just_path(&job.just).map_err(|_| {
                    DefinitionError::Invalid(format!(
                        "job `{pipeline_name}::{job_name}` has invalid Just target `{}`",
                        job.just
                    ))
                })?;
                reject_duplicates(
                    &job.needs,
                    &format!("job `{pipeline_name}::{job_name}` dependencies"),
                )?;
                reject_duplicates(
                    &job.triggers,
                    &format!("job `{pipeline_name}::{job_name}` triggers"),
                )?;
                for hook in &job.triggers {
                    validate_hook(hook)?;
                }
                for needed in &job.needs {
                    if needed == job_name {
                        return invalid(format!(
                            "job `{pipeline_name}::{job_name}` depends on itself"
                        ));
                    }
                    if !pipeline.jobs.contains_key(needed) {
                        return invalid(format!(
                            "job `{pipeline_name}::{job_name}` needs missing sibling job `{needed}`"
                        ));
                    }
                }
            }
        }

        detect_cycle(
            self.pipelines.keys().map(String::as_str),
            |name| {
                self.pipelines[name]
                    .needs
                    .iter()
                    .map(String::as_str)
                    .collect()
            },
            "pipeline",
        )?;
        for (pipeline_name, pipeline) in &self.pipelines {
            detect_cycle(
                pipeline.jobs.keys().map(String::as_str),
                |name| {
                    pipeline.jobs[name]
                        .needs
                        .iter()
                        .map(String::as_str)
                        .collect()
                },
                &format!("job in pipeline `{pipeline_name}`"),
            )?;
        }

        for (hook, selectors) in &self.triggers {
            for selector in selectors {
                self.parse_selector(selector).map_err(|error| {
                    DefinitionError::Invalid(format!(
                        "hook `{hook}` has invalid selector `{selector}`: {error}"
                    ))
                })?;
            }
        }

        Ok(())
    }

    pub fn select_pipelines(&self, arguments: &[OsString]) -> Result<Selection> {
        let mut selection = Selection::default();
        if arguments.is_empty() {
            let first = self
                .pipelines
                .first()
                .expect("validated definitions contain a pipeline")
                .0;
            selection.push_pipeline(first);
            return Ok(selection);
        }

        for argument in arguments {
            let target = argument
                .to_str()
                .ok_or_else(|| DefinitionError::NonUnicodeTarget(argument.clone()))?;
            if !self.pipelines.contains_key(target) {
                return invalid(format!("unknown pipeline `{target}`"));
            }
            selection.push_pipeline(target);
        }
        Ok(selection)
    }

    pub fn selection_for_hook(&self, hook: &str) -> Result<Selection> {
        validate_hook(hook)?;
        let mut selection = Selection::default();

        if let Some(selectors) = self.triggers.get(hook) {
            for selector in selectors {
                match self.parse_selector(selector).expect("validated selector") {
                    SelectedTarget::Pipeline(name) => selection.push_pipeline(&name),
                    SelectedTarget::Job(job) => selection.push_job(&job.pipeline, &job.job),
                }
            }
        }

        for (pipeline_name, pipeline) in &self.pipelines {
            if pipeline.triggers.iter().any(|candidate| candidate == hook) {
                selection.push_pipeline(pipeline_name);
            }
            for (job_name, job) in &pipeline.jobs {
                if job.triggers.iter().any(|candidate| candidate == hook) {
                    selection.push_job(pipeline_name, job_name);
                }
            }
        }

        Ok(selection)
    }

    pub fn managed_hooks(&self) -> Vec<String> {
        let mut present = HashSet::new();
        present.extend(self.triggers.keys().map(String::as_str));
        for pipeline in self.pipelines.values() {
            present.extend(pipeline.triggers.iter().map(String::as_str));
            for job in pipeline.jobs.values() {
                present.extend(job.triggers.iter().map(String::as_str));
            }
        }
        SUPPORTED_HOOKS
            .iter()
            .filter(|hook| present.contains(**hook))
            .map(|hook| (*hook).to_owned())
            .collect()
    }

    fn parse_selector(&self, selector: &str) -> std::result::Result<SelectedTarget, String> {
        let parts: Vec<_> = selector.split("::").collect();
        match parts.as_slice() {
            [pipeline] => {
                if self.pipelines.contains_key(*pipeline) {
                    Ok(SelectedTarget::Pipeline((*pipeline).to_owned()))
                } else {
                    Err(format!("unknown pipeline `{pipeline}`"))
                }
            }
            [pipeline, job] => {
                let Some(spec) = self.pipelines.get(*pipeline) else {
                    return Err(format!("unknown pipeline `{pipeline}`"));
                };
                if !spec.jobs.contains_key(*job) {
                    return Err(format!("unknown job `{pipeline}::{job}`"));
                }
                Ok(SelectedTarget::Job(JobSelector {
                    pipeline: (*pipeline).to_owned(),
                    job: (*job).to_owned(),
                }))
            }
            _ => Err("expected `PIPELINE` or `PIPELINE::JOB`".to_owned()),
        }
    }

    pub fn render(
        &self,
        selection: &Selection,
        working_directory: &Path,
        just_executable: &Path,
        control_justfile: Option<&Path>,
        job_limit: JobLimit,
        no_deps: bool,
    ) -> Result<RenderedPipeline> {
        self.validate_selection(selection)?;
        let executable = just_executable
            .to_str()
            .ok_or_else(|| DefinitionError::NonUnicodeExecutable(just_executable.to_path_buf()))?;
        reject_generated_newline(executable, "Just executable path")?;
        let executable = shell_quote(executable);
        let mut output = String::from("# Generated by Pipeline. Do not edit.\n\n");

        for (pipeline_index, (_, pipeline)) in self.pipelines.iter().enumerate() {
            let gate = pipeline_gate(pipeline_index);
            let dependency_nodes: Vec<_> = pipeline
                .needs
                .iter()
                .map(|name| {
                    pipeline_done(
                        self.pipelines
                            .get_index_of(name)
                            .expect("validated pipeline dependency"),
                    )
                })
                .collect();
            render_node(&mut output, &gate, &dependency_nodes, None);

            for (job_index, (_, job)) in pipeline.jobs.iter().enumerate() {
                let name = pipeline_job(pipeline_index, job_index);
                let mut dependencies = Vec::with_capacity(job.needs.len() + 1);
                dependencies.push(gate.clone());
                dependencies.extend(job.needs.iter().map(|needed| {
                    pipeline_job(
                        pipeline_index,
                        pipeline
                            .jobs
                            .get_index_of(needed)
                            .expect("validated job dependency"),
                    )
                }));

                let mut command = format!("@{executable}");
                if let Some(path) = control_justfile {
                    let path = path.to_str().ok_or_else(|| {
                        DefinitionError::Invalid(format!(
                            "Justfile path is not valid UTF-8: {:?}",
                            path
                        ))
                    })?;
                    let cwd = working_directory.to_str().ok_or_else(|| {
                        DefinitionError::Invalid(format!(
                            "working directory is not valid UTF-8: {:?}",
                            working_directory
                        ))
                    })?;
                    reject_generated_newline(path, "Justfile path")?;
                    reject_generated_newline(cwd, "working directory")?;
                    command.push_str(" --justfile ");
                    command.push_str(&shell_quote(path));
                    command.push_str(" --working-directory ");
                    command.push_str(&shell_quote(cwd));
                }
                if let Some(jobs) = job_limit.nested_jobs() {
                    command.push_str(" --jobs ");
                    command.push_str(&jobs.get().to_string());
                }
                if no_deps {
                    command.push_str(" --no-deps");
                }
                command.push(' ');
                command.push_str(&shell_quote(&job.just));
                render_node(&mut output, &name, &dependencies, Some(&command));
            }

            let done = pipeline_done(pipeline_index);
            let dependencies = if pipeline.jobs.is_empty() {
                vec![gate]
            } else {
                pipeline
                    .jobs
                    .iter()
                    .enumerate()
                    .map(|(job_index, _)| pipeline_job(pipeline_index, job_index))
                    .collect()
            };
            render_node(&mut output, &done, &dependencies, None);
        }

        let roots = selection
            .targets
            .iter()
            .map(|target| match target {
                SelectedTarget::Pipeline(name) => pipeline_done(
                    self.pipelines
                        .get_index_of(name)
                        .expect("validated selected pipeline"),
                ),
                SelectedTarget::Job(job) => {
                    let pipeline_index = self
                        .pipelines
                        .get_index_of(&job.pipeline)
                        .expect("validated selected pipeline");
                    pipeline_job(
                        pipeline_index,
                        self.pipelines[&job.pipeline]
                            .jobs
                            .get_index_of(&job.job)
                            .expect("validated selected job"),
                    )
                }
            })
            .collect::<Vec<_>>();
        render_node(&mut output, "__pipeline_selected", &roots, None);

        Ok(RenderedPipeline {
            contents: output,
            entrypoint: "__pipeline_selected".to_owned(),
            working_directory: working_directory.to_path_buf(),
        })
    }

    fn validate_selection(&self, selection: &Selection) -> Result<()> {
        for target in &selection.targets {
            match target {
                SelectedTarget::Pipeline(name) => {
                    if !self.pipelines.contains_key(name) {
                        return invalid(format!("unknown selected pipeline `{name}`"));
                    }
                }
                SelectedTarget::Job(job) => {
                    let Some(pipeline) = self.pipelines.get(&job.pipeline) else {
                        return invalid(format!("unknown selected pipeline `{}`", job.pipeline));
                    };
                    if !pipeline.jobs.contains_key(&job.job) {
                        return invalid(format!(
                            "unknown selected job `{}::{}`",
                            job.pipeline, job.job
                        ));
                    }
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct JobSelector {
    pub pipeline: String,
    pub job: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SelectedTarget {
    Pipeline(String),
    Job(JobSelector),
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Selection {
    pub targets: Vec<SelectedTarget>,
}

impl Selection {
    pub fn is_empty(&self) -> bool {
        self.targets.is_empty()
    }

    pub fn pipeline(name: impl Into<String>) -> Self {
        Self {
            targets: vec![SelectedTarget::Pipeline(name.into())],
        }
    }

    pub fn job(pipeline: impl Into<String>, job: impl Into<String>) -> Self {
        Self {
            targets: vec![SelectedTarget::Job(JobSelector {
                pipeline: pipeline.into(),
                job: job.into(),
            })],
        }
    }

    fn push_pipeline(&mut self, pipeline: &str) {
        self.targets.retain(
            |target| !matches!(target, SelectedTarget::Job(job) if job.pipeline == pipeline),
        );
        if !self
            .targets
            .iter()
            .any(|target| matches!(target, SelectedTarget::Pipeline(name) if name == pipeline))
        {
            self.targets
                .push(SelectedTarget::Pipeline(pipeline.to_owned()));
        }
    }

    fn push_job(&mut self, pipeline: &str, job: &str) {
        if self
            .targets
            .iter()
            .any(|target| matches!(target, SelectedTarget::Pipeline(name) if name == pipeline))
        {
            return;
        }
        let selector = JobSelector {
            pipeline: pipeline.to_owned(),
            job: job.to_owned(),
        };
        if !self
            .targets
            .contains(&SelectedTarget::Job(selector.clone()))
        {
            self.targets.push(SelectedTarget::Job(selector));
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum JobLimit {
    #[default]
    Default,
    PerProcess(NonZeroUsize),
    Capped(NonZeroUsize),
}

impl JobLimit {
    pub fn outer_jobs(self) -> Option<NonZeroUsize> {
        match self {
            Self::Default => None,
            Self::PerProcess(jobs) | Self::Capped(jobs) => Some(jobs),
        }
    }

    pub fn nested_jobs(self) -> Option<NonZeroUsize> {
        match self {
            Self::Default => None,
            Self::PerProcess(jobs) => Some(jobs),
            Self::Capped(_) => NonZeroUsize::new(1),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub enum StdinSource {
    #[default]
    Inherit,
    File(PathBuf),
    Null,
}

#[derive(Clone, Debug, Default)]
pub struct RunOptions {
    pub arguments: Vec<OsString>,
    pub job_limit: JobLimit,
    pub no_deps: bool,
    pub environment: Vec<(OsString, OsString)>,
    pub stdin: StdinSource,
    /// Override the directory recipes execute in while retaining control files
    /// from the loaded [`Project`]. Used for trusted definitions over incoming trees.
    pub working_directory: Option<PathBuf>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RenderedPipeline {
    pub contents: String,
    pub entrypoint: String,
    pub working_directory: PathBuf,
}

/// An invocation owning its temporary generated Justfile, if any.
#[derive(Debug)]
pub struct PreparedRun {
    program: PathBuf,
    arguments: Vec<OsString>,
    working_directory: PathBuf,
    environment: Vec<(OsString, OsString)>,
    stdin: StdinSource,
    _generated_definition: Option<NamedTempFile>,
}

impl PreparedRun {
    fn new(
        program: PathBuf,
        arguments: Vec<OsString>,
        working_directory: PathBuf,
        options: &RunOptions,
        generated_definition: Option<NamedTempFile>,
    ) -> Self {
        Self {
            program,
            arguments,
            working_directory,
            environment: options.environment.clone(),
            stdin: options.stdin.clone(),
            _generated_definition: generated_definition,
        }
    }

    pub fn program(&self) -> &Path {
        &self.program
    }

    pub fn arguments(&self) -> &[OsString] {
        &self.arguments
    }

    pub fn working_directory(&self) -> &Path {
        &self.working_directory
    }

    pub fn command(&self) -> Result<Command> {
        let mut command = Command::new(&self.program);
        command
            .args(&self.arguments)
            .current_dir(&self.working_directory)
            .envs(self.environment.iter().cloned());
        match &self.stdin {
            StdinSource::Inherit => {
                command.stdin(Stdio::inherit());
            }
            StdinSource::File(path) => {
                let file = File::open(path).map_err(|source| DefinitionError::Io {
                    path: path.clone(),
                    source,
                })?;
                command.stdin(Stdio::from(file));
            }
            StdinSource::Null => {
                command.stdin(Stdio::null());
            }
        }
        command.stdout(Stdio::inherit()).stderr(Stdio::inherit());
        Ok(command)
    }

    pub fn status(&self) -> Result<ExitStatus> {
        self.command()?
            .status()
            .map_err(|source| DefinitionError::Execute {
                program: self.program.clone(),
                source,
            })
    }
}

fn append_outer_options(arguments: &mut Vec<OsString>, limit: JobLimit, no_deps: bool) {
    if let Some(jobs) = limit.outer_jobs() {
        arguments.push(OsString::from("--jobs"));
        arguments.push(OsString::from(jobs.get().to_string()));
    }
    if no_deps {
        arguments.push(OsString::from("--no-deps"));
    }
}

fn pipeline_gate(index: usize) -> String {
    format!("__pipeline_gate_{index}")
}

fn pipeline_done(index: usize) -> String {
    format!("__pipeline_done_{index}")
}

fn pipeline_job(pipeline: usize, job: usize) -> String {
    format!("__pipeline_job_{pipeline}_{job}")
}

fn render_node(output: &mut String, name: &str, dependencies: &[String], body: Option<&str>) {
    if dependencies.len() > 1 {
        output.push_str("[parallel]\n");
    }
    output.push_str(name);
    output.push(':');
    for dependency in dependencies {
        output.push(' ');
        output.push_str(dependency);
    }
    output.push('\n');
    if let Some(body) = body {
        output.push_str("    ");
        output.push_str(body);
        output.push('\n');
    }
    output.push('\n');
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn reject_generated_newline(value: &str, description: &str) -> Result<()> {
    if value.contains('\n') || value.contains('\r') {
        invalid(format!("{description} cannot contain a newline"))
    } else {
        Ok(())
    }
}

fn validate_identifier(value: &str, kind: &str) -> Result<()> {
    let mut characters = value.chars();
    let Some(first) = characters.next() else {
        return invalid(format!("{kind} name cannot be empty"));
    };
    if !(first.is_ascii_alphabetic() || first == '_')
        || !characters.all(|character| {
            character.is_ascii_alphanumeric() || character == '_' || character == '-'
        })
    {
        return invalid(format!(
            "invalid {kind} name `{value}`; expected [A-Za-z_][A-Za-z0-9_-]*"
        ));
    }
    Ok(())
}

fn validate_just_path(value: &str) -> Result<()> {
    if value.is_empty()
        || value
            .split("::")
            .any(|part| validate_identifier(part, "Just target").is_err())
    {
        return invalid("invalid Just target path");
    }
    Ok(())
}

fn validate_hook(hook: &str) -> Result<()> {
    if SUPPORTED_HOOKS.contains(&hook) {
        Ok(())
    } else {
        invalid(format!("unknown Git hook `{hook}`"))
    }
}

fn reject_duplicates(values: &[String], description: &str) -> Result<()> {
    let mut seen = HashSet::new();
    for value in values {
        if !seen.insert(value) {
            return invalid(format!("duplicate `{value}` in {description}"));
        }
    }
    Ok(())
}

fn detect_cycle<'a, I, F>(nodes: I, dependencies: F, kind: &str) -> Result<()>
where
    I: IntoIterator<Item = &'a str>,
    F: Fn(&str) -> Vec<&'a str>,
{
    fn visit<'a, F>(
        node: &'a str,
        dependencies: &F,
        states: &mut HashMap<&'a str, u8>,
        stack: &mut Vec<&'a str>,
        kind: &str,
    ) -> Result<()>
    where
        F: Fn(&str) -> Vec<&'a str>,
    {
        match states.get(node).copied() {
            Some(2) => return Ok(()),
            Some(1) => {
                let start = stack.iter().position(|entry| *entry == node).unwrap_or(0);
                let mut path = stack[start..].to_vec();
                path.push(node);
                return invalid(format!("{kind} dependency cycle: {}", path.join(" -> ")));
            }
            _ => {}
        }
        states.insert(node, 1);
        stack.push(node);
        for dependency in dependencies(node) {
            visit(dependency, dependencies, states, stack, kind)?;
        }
        stack.pop();
        states.insert(node, 2);
        Ok(())
    }

    let nodes: Vec<_> = nodes.into_iter().collect();
    let mut states = HashMap::new();
    let mut stack = Vec::new();
    for node in nodes {
        visit(node, &dependencies, &mut states, &mut stack, kind)?;
    }
    Ok(())
}

fn invalid<T>(message: impl Into<String>) -> Result<T> {
    Err(DefinitionError::Invalid(message.into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    fn parse(yaml: &str) -> PipelineConfig {
        PipelineConfig::from_yaml(yaml).unwrap()
    }

    #[test]
    fn discovery_uses_nearest_directory_then_filename_precedence() {
        let root = tempfile::tempdir().unwrap();
        let child = root.path().join("a/b");
        fs::create_dir_all(&child).unwrap();
        fs::write(
            root.path().join("pipeline.yml"),
            "pipelines:\n  root:\n    jobs:\n      x:\n        just: x\n",
        )
        .unwrap();
        fs::write(root.path().join("Pipelinefile"), "default:\n    true\n").unwrap();

        let project = Project::discover(&child).unwrap();
        assert_eq!(project.kind(), DefinitionKind::Pipelinefile);

        fs::write(
            child.join(".pipeline.yml"),
            "pipelines:\n  child:\n    jobs:\n      x:\n        just: x\n",
        )
        .unwrap();
        let project = Project::discover(&child).unwrap();
        assert_eq!(project.path(), child.join(".pipeline.yml"));
    }

    #[test]
    fn boundary_is_inclusive_and_not_crossed() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        let child = repo.join("nested");
        fs::create_dir_all(&child).unwrap();
        fs::write(root.path().join("Pipelinefile"), "x:\n    true\n").unwrap();
        assert!(matches!(
            Project::discover_with_boundary(&child, &repo),
            Err(DefinitionError::NotFound { .. })
        ));
    }

    #[test]
    #[cfg(unix)]
    fn control_paths_preserve_tracked_symlinks_and_reject_external_targets() {
        let root = tempfile::tempdir().unwrap();
        let repository = root.path().join("repository");
        let outside = root.path().join("outside");
        fs::create_dir_all(&repository).unwrap();
        fs::create_dir_all(&outside).unwrap();
        let yaml = "pipelines:\n  check:\n    jobs:\n      test:\n        just: test\n";
        fs::write(repository.join("definition-source.yml"), yaml).unwrap();
        fs::write(repository.join("just-source"), "test:\n    true\n").unwrap();
        symlink("definition-source.yml", repository.join("pipeline.yml")).unwrap();
        symlink("just-source", repository.join("justfile")).unwrap();

        let project = Project::open(repository.join("pipeline.yml")).unwrap();
        assert_eq!(project.path(), repository.join("pipeline.yml"));
        assert_eq!(
            project.control_paths_with_boundary(&repository).unwrap(),
            vec![repository.join("pipeline.yml"), repository.join("justfile")]
        );

        fs::remove_file(repository.join("pipeline.yml")).unwrap();
        fs::write(outside.join("definition.yml"), yaml).unwrap();
        symlink(
            outside.join("definition.yml"),
            repository.join("pipeline.yml"),
        )
        .unwrap();
        let project = Project::open(repository.join("pipeline.yml")).unwrap();
        assert!(project
            .control_paths_with_boundary(&repository)
            .unwrap_err()
            .to_string()
            .contains("outside control boundary"));
    }

    #[test]
    fn version_defaults_to_one_and_unknown_fields_are_rejected() {
        let config = parse("pipelines:\n  check:\n    jobs:\n      test:\n        just: test\n");
        assert_eq!(config.version, 1);
        assert!(PipelineConfig::from_yaml(
            "pipelines:\n  check:\n    jobs:\n      test:\n        just: test\n        command: nope\n"
        )
        .is_err());
        assert!(PipelineConfig::from_yaml(
            "version: 2\npipelines:\n  check:\n    jobs:\n      test:\n        just: test\n"
        )
        .is_err());
    }

    #[test]
    fn validates_references_names_and_cycles() {
        assert!(PipelineConfig::from_yaml(
            "pipelines:\n  a:\n    needs: [b]\n  b:\n    needs: [a]\n"
        )
        .unwrap_err()
        .to_string()
        .contains("cycle"));
        assert!(PipelineConfig::from_yaml(
            "pipelines:\n  ok:\n    jobs:\n      one:\n        needs: [missing]\n        just: test\n"
        )
        .unwrap_err()
        .to_string()
        .contains("missing sibling"));
        assert!(PipelineConfig::from_yaml(
            "pipelines:\n  bad name:\n    jobs:\n      one:\n        just: test\n"
        )
        .is_err());
        assert!(PipelineConfig::from_yaml(
            "pipelines:\n  ok:\n    jobs:\n      one:\n        just: 'test --flag'\n"
        )
        .is_err());
    }

    #[test]
    fn trigger_union_prefers_whole_pipeline_and_managed_hooks_are_canonical() {
        let config = parse(
            "on:\n  pre-push: [check::test]\n\npipelines:\n  check:\n    on: [pre-commit, pre-push]\n    jobs:\n      fmt:\n        on: [commit-msg]\n        just: fmt\n      test:\n        on: [pre-commit]\n        just: test\n",
        );
        assert_eq!(
            config.selection_for_hook("pre-commit").unwrap(),
            Selection::pipeline("check")
        );
        assert_eq!(
            config.selection_for_hook("commit-msg").unwrap(),
            Selection::job("check", "fmt")
        );
        assert_eq!(
            config.managed_hooks(),
            vec!["pre-commit", "commit-msg", "pre-push"]
        );
    }

    #[test]
    fn default_and_multiple_pipeline_selection_preserve_order() {
        let config = parse(
            "pipelines:\n  first:\n    jobs:\n      x:\n        just: x\n  second:\n    jobs:\n      y:\n        just: y\n",
        );
        assert_eq!(
            config.select_pipelines(&[]).unwrap(),
            Selection::pipeline("first")
        );
        assert_eq!(
            config
                .select_pipelines(&[OsString::from("second"), OsString::from("first")])
                .unwrap()
                .targets,
            vec![
                SelectedTarget::Pipeline("second".to_owned()),
                SelectedTarget::Pipeline("first".to_owned())
            ]
        );
    }

    #[test]
    fn render_has_parallel_barriers_and_nested_options() {
        let config = parse(
            "pipelines:\n  check:\n    jobs:\n      fmt:\n        just: tools::fmt\n      test:\n        needs: [fmt]\n        just: test\n  build:\n    needs: [check]\n    jobs:\n      build:\n        just: build\n",
        );
        let rendered = config
            .render(
                &Selection::pipeline("build"),
                Path::new("/work"),
                Path::new("/usr/bin/just"),
                Some(Path::new("/work/justfile")),
                JobLimit::Capped(NonZeroUsize::new(8).unwrap()),
                true,
            )
            .unwrap();
        assert!(rendered
            .contents
            .contains("__pipeline_gate_1: __pipeline_done_0"));
        assert!(rendered
            .contents
            .contains("__pipeline_job_0_1: __pipeline_gate_0 __pipeline_job_0_0"));
        assert!(rendered
            .contents
            .contains("--working-directory '/work' --jobs 1 --no-deps 'tools::fmt'"));
        assert!(rendered
            .contents
            .contains("__pipeline_selected: __pipeline_done_1"));
    }

    #[test]
    fn prepared_yaml_applies_outer_but_not_outer_no_deps() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("pipeline.yml");
        fs::write(
            &path,
            "pipelines:\n  check:\n    jobs:\n      test:\n        just: test\n",
        )
        .unwrap();
        fs::write(root.path().join("justfile"), "test:\n    true\n").unwrap();
        let project = Project::open(path).unwrap();
        let options = RunOptions {
            job_limit: JobLimit::PerProcess(NonZeroUsize::new(3).unwrap()),
            no_deps: true,
            ..RunOptions::default()
        };
        let prepared = project
            .prepare(Path::new("/usr/bin/just"), &options)
            .unwrap();
        let arguments: Vec<_> = prepared
            .arguments()
            .iter()
            .map(|value| value.to_string_lossy().into_owned())
            .collect();
        assert!(arguments.windows(2).any(|pair| pair == ["--jobs", "3"]));
        assert!(!arguments.iter().any(|value| value == "--no-deps"));
    }

    #[test]
    fn generated_graph_is_accepted_by_an_available_just() {
        let Some(just) = std::env::var_os("PATH").and_then(|path| {
            std::env::split_paths(&path)
                .map(|directory| directory.join("just"))
                .find(|candidate| candidate.is_file())
        }) else {
            return;
        };
        let root = tempfile::tempdir().unwrap();
        fs::write(
            root.path().join("justfile"),
            "format:\n    @true\n\ntest: format\n    @true\n\nbuild:\n    @true\n",
        )
        .unwrap();
        fs::write(
            root.path().join("pipeline.yml"),
            "pipelines:\n  check:\n    jobs:\n      format:\n        just: format\n      test:\n        needs: [format]\n        just: test\n  build:\n    needs: [check]\n    jobs:\n      build:\n        just: build\n",
        )
        .unwrap();
        let project = Project::open(root.path().join("pipeline.yml")).unwrap();
        let options = RunOptions {
            arguments: vec![OsString::from("build")],
            job_limit: JobLimit::Capped(NonZeroUsize::new(2).unwrap()),
            stdin: StdinSource::Null,
            ..RunOptions::default()
        };
        assert!(project.run(&just, &options).unwrap().success());
    }
}
