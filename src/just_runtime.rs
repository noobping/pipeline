//! Resolution of the Just executable used by Pipeline.
//!
//! A compatible installed/PATH executable is preferred. If none is available,
//! Pipeline can copy `/just` out of its configured official container image and
//! execute that binary directly on the host.

use std::env;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::{self, Permissions};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use semver::Version;
use tempfile::{Builder as TempBuilder, TempDir};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

/// Runtime values embedded from Cargo package metadata by the caller.
#[derive(Clone, Debug)]
pub struct RuntimeConfig {
    image: String,
    minimum_version: Version,
    installed_just: Option<PathBuf>,
    search_path: Option<OsString>,
    container_fallback: bool,
    temporary_root: Option<PathBuf>,
    cache_root: Option<PathBuf>,
}

impl RuntimeConfig {
    pub fn new(image: impl Into<String>, minimum_version: &str) -> Result<Self, RuntimeError> {
        let image = image.into();
        if image.trim().is_empty() {
            return Err(RuntimeError::Configuration(
                "Just container image cannot be empty".to_owned(),
            ));
        }
        let minimum_version = Version::parse(minimum_version).map_err(|error| {
            RuntimeError::Configuration(format!(
                "invalid minimum Just version `{minimum_version}`: {error}"
            ))
        })?;
        Ok(Self {
            image,
            minimum_version,
            installed_just: None,
            search_path: env::var_os("PATH"),
            container_fallback: true,
            temporary_root: Some(PathBuf::from("/tmp")),
            cache_root: default_cache_root(),
        })
    }

    pub fn image(&self) -> &str {
        &self.image
    }

    pub fn minimum_version(&self) -> &Version {
        &self.minimum_version
    }

    pub fn installed_just(&self) -> Option<&Path> {
        self.installed_just.as_deref()
    }

    pub fn with_installed_just(mut self, path: Option<PathBuf>) -> Self {
        self.installed_just = path;
        self
    }

    /// Override the PATH used for Just and container-engine lookup. `None`
    /// disables PATH lookup, which is useful for policy and deterministic tests.
    pub fn with_search_path(mut self, path: Option<OsString>) -> Self {
        self.search_path = path;
        self
    }

    pub fn with_container_fallback(mut self, enabled: bool) -> Self {
        self.container_fallback = enabled;
        self
    }

    /// Override `/tmp`; `None` skips directly to the one-run cache directory.
    pub fn with_temporary_root(mut self, path: Option<PathBuf>) -> Self {
        self.temporary_root = path;
        self
    }

    /// Override the fallback cache root; `None` disables that fallback.
    pub fn with_cache_root(mut self, path: Option<PathBuf>) -> Self {
        self.cache_root = path;
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RuntimeSource {
    Installed(PathBuf),
    Path(PathBuf),
    PodmanImage(String),
    DockerImage(String),
}

/// A verified Just executable. The private temporary directory keeps an image
/// extraction alive for exactly as long as the resolved runtime is used.
#[derive(Debug)]
pub struct JustRuntime {
    executable: PathBuf,
    version: Version,
    source: RuntimeSource,
    _temporary: Option<TempDir>,
}

impl JustRuntime {
    pub fn resolve(config: &RuntimeConfig) -> Result<Self, RuntimeError> {
        let mut attempts = Vec::new();

        if let Some(path) = &config.installed_just {
            match verified_executable(path, &config.minimum_version, config.search_path.as_deref())
            {
                Ok((executable, version)) => {
                    return Ok(Self {
                        source: RuntimeSource::Installed(executable.clone()),
                        executable,
                        version,
                        _temporary: None,
                    });
                }
                Err(error) => attempts.push(RuntimeAttempt::new(
                    format!("installed Just ({})", path.display()),
                    error,
                )),
            }
        }

        match find_in_path("just", config.search_path.as_deref()) {
            Some(path) => match verified_executable(
                &path,
                &config.minimum_version,
                config.search_path.as_deref(),
            ) {
                Ok((executable, version)) => {
                    return Ok(Self {
                        source: RuntimeSource::Path(executable.clone()),
                        executable,
                        version,
                        _temporary: None,
                    });
                }
                Err(error) => attempts.push(RuntimeAttempt::new(
                    format!("Just from PATH ({})", path.display()),
                    error,
                )),
            },
            None => attempts.push(RuntimeAttempt::new("Just from PATH", "not found")),
        }

        if config.container_fallback {
            for (engine_name, source) in [
                ("podman", RuntimeSourceKind::Podman),
                ("docker", RuntimeSourceKind::Docker),
            ] {
                let Some(engine) = find_in_path(engine_name, config.search_path.as_deref()) else {
                    attempts.push(RuntimeAttempt::new(engine_name, "not found in PATH"));
                    continue;
                };
                match extract_from_image(config, &engine, source) {
                    Ok(runtime) => return Ok(runtime),
                    Err(error) => attempts.push(RuntimeAttempt::new(
                        format!("{engine_name} image `{}`", config.image),
                        error,
                    )),
                }
            }
        } else {
            attempts.push(RuntimeAttempt::new(
                "container fallback",
                "disabled by configuration",
            ));
        }

        Err(RuntimeError::Unavailable {
            minimum_version: config.minimum_version.clone(),
            attempts,
        })
    }

    pub fn executable(&self) -> &Path {
        &self.executable
    }

    pub fn version(&self) -> &Version {
        &self.version
    }

    pub fn source(&self) -> &RuntimeSource {
        &self.source
    }

    pub fn command(&self) -> Command {
        Command::new(&self.executable)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeAttempt {
    pub source: String,
    pub error: String,
}

impl RuntimeAttempt {
    fn new(source: impl Into<String>, error: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            error: error.into(),
        }
    }
}

#[derive(Debug)]
pub enum RuntimeError {
    Configuration(String),
    Unavailable {
        minimum_version: Version,
        attempts: Vec<RuntimeAttempt>,
    },
}

impl RuntimeError {
    pub fn attempts(&self) -> &[RuntimeAttempt] {
        match self {
            Self::Configuration(_) => &[],
            Self::Unavailable { attempts, .. } => attempts,
        }
    }
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Configuration(message) => formatter.write_str(message),
            Self::Unavailable {
                minimum_version,
                attempts,
            } => {
                write!(
                    formatter,
                    "could not resolve Just {minimum_version} or newer"
                )?;
                for attempt in attempts {
                    write!(formatter, "\n  {}: {}", attempt.source, attempt.error)?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for RuntimeError {}

#[derive(Clone, Copy)]
enum RuntimeSourceKind {
    Podman,
    Docker,
}

impl RuntimeSourceKind {
    fn finish(self, image: String) -> RuntimeSource {
        match self {
            Self::Podman => RuntimeSource::PodmanImage(image),
            Self::Docker => RuntimeSource::DockerImage(image),
        }
    }
}

fn verified_executable(
    path: &Path,
    minimum: &Version,
    search_path: Option<&OsStr>,
) -> std::result::Result<(PathBuf, Version), String> {
    let output = run_output(path, &[OsStr::new("--version")], search_path)
        .map_err(|error| format!("cannot execute: {error}"))?;
    if !output.status.success() {
        return Err(format_output_failure("`just --version` failed", &output));
    }
    let version = parse_just_version(&output)
        .ok_or_else(|| "unrecognized output from `just --version`".to_owned())?;
    if &version < minimum {
        return Err(format!(
            "version {version} is too old; {minimum} or newer is required"
        ));
    }
    let executable = fs::canonicalize(path)
        .map_err(|error| format!("cannot resolve executable path: {error}"))?;
    Ok((executable, version))
}

fn parse_just_version(output: &Output) -> Option<Version> {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    stdout.lines().chain(stderr.lines()).find_map(|line| {
        let mut words = line.split_whitespace();
        if !words.next()?.eq_ignore_ascii_case("just") {
            return None;
        }
        Version::parse(words.next()?.trim_start_matches('v')).ok()
    })
}

fn find_in_path(program: &str, search_path: Option<&OsStr>) -> Option<PathBuf> {
    let search_path = search_path?;
    env::split_paths(search_path)
        .map(|directory| {
            if directory.as_os_str().is_empty() {
                PathBuf::from(program)
            } else {
                directory.join(program)
            }
        })
        .find(|candidate| {
            fs::metadata(candidate)
                .map(|metadata| {
                    metadata.is_file() && {
                        #[cfg(unix)]
                        {
                            metadata.permissions().mode() & 0o111 != 0
                        }
                        #[cfg(not(unix))]
                        {
                            true
                        }
                    }
                })
                .unwrap_or(false)
        })
}

fn extract_from_image(
    config: &RuntimeConfig,
    engine: &Path,
    source_kind: RuntimeSourceKind,
) -> std::result::Result<JustRuntime, String> {
    ensure_image(config, engine)?;

    let (directory, used_temporary_root) = create_stage(config)?;
    let extracted = directory.path().join("just");
    copy_from_container(config, engine, &extracted)?;
    make_executable(&extracted).map_err(|error| {
        format!(
            "cannot make extracted Just executable at {}: {error}",
            extracted.display()
        )
    })?;

    match verified_executable(
        &extracted,
        &config.minimum_version,
        config.search_path.as_deref(),
    ) {
        Ok((executable, version)) => Ok(JustRuntime {
            executable,
            version,
            source: source_kind.finish(config.image.clone()),
            _temporary: Some(directory),
        }),
        Err(first_error) if used_temporary_root && looks_like_noexec(&first_error) => {
            let cache = create_cache_stage(config)?;
            let cached_executable = cache.path().join("just");
            fs::copy(&extracted, &cached_executable).map_err(|error| {
                format!(
                    "cannot copy extracted Just to cache {}: {error}",
                    cached_executable.display()
                )
            })?;
            make_executable(&cached_executable).map_err(|error| {
                format!(
                    "cannot make cached Just executable at {}: {error}",
                    cached_executable.display()
                )
            })?;
            let (executable, version) = verified_executable(
                &cached_executable,
                &config.minimum_version,
                config.search_path.as_deref(),
            )
            .map_err(|cache_error| {
                format!(
                    "temporary Just was not executable ({first_error}); cache fallback failed: {cache_error}"
                )
            })?;
            Ok(JustRuntime {
                executable,
                version,
                source: source_kind.finish(config.image.clone()),
                _temporary: Some(cache),
            })
        }
        Err(error) => Err(error),
    }
}

fn ensure_image(config: &RuntimeConfig, engine: &Path) -> std::result::Result<(), String> {
    let inspect = run_output(
        engine,
        &[
            OsStr::new("image"),
            OsStr::new("inspect"),
            OsStr::new(&config.image),
        ],
        config.search_path.as_deref(),
    )
    .map_err(|error| format!("cannot inspect image: {error}"))?;
    if inspect.status.success() {
        return Ok(());
    }
    if !image_is_absent(&inspect) {
        return Err(format_output_failure("cannot inspect image", &inspect));
    }

    let pull = run_output(
        engine,
        &[OsStr::new("pull"), OsStr::new(&config.image)],
        config.search_path.as_deref(),
    )
    .map_err(|error| format!("cannot pull image: {error}"))?;
    if pull.status.success() {
        Ok(())
    } else {
        Err(format_output_failure("cannot pull image", &pull))
    }
}

fn image_is_absent(output: &Output) -> bool {
    let stderr = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
    let stdout = String::from_utf8_lossy(&output.stdout).to_ascii_lowercase();
    let diagnostic = format!("{stderr}\n{stdout}");
    // Docker and Podman both use status 1 for a missing local image. Their
    // wording has varied, so recognize the established absence diagnostics;
    // an empty status-1 response is also accepted for compatible wrappers.
    let recognized = diagnostic.contains("no such image")
        || diagnostic.contains("no such object")
        || diagnostic.contains("image not known")
        || diagnostic.contains("image not found");
    (output.status.code() == Some(1) && (diagnostic.trim().is_empty() || recognized))
        || (output.status.code() == Some(125) && recognized)
}

fn copy_from_container(
    config: &RuntimeConfig,
    engine: &Path,
    destination: &Path,
) -> std::result::Result<(), String> {
    static CONTAINER_SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let name = format!(
        "pipeline-just-{}-{}",
        std::process::id(),
        CONTAINER_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );
    let create = run_output(
        engine,
        &[
            OsStr::new("create"),
            OsStr::new("--name"),
            OsStr::new(&name),
            OsStr::new(&config.image),
            OsStr::new("/just"),
            OsStr::new("--version"),
        ],
        config.search_path.as_deref(),
    )
    .map_err(|error| format!("cannot create extraction container: {error}"))?;
    if !create.status.success() {
        return Err(format_output_failure(
            "cannot create extraction container",
            &create,
        ));
    }
    let id = String::from_utf8_lossy(&create.stdout)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_owned)
        .unwrap_or(name);
    let guard = ContainerGuard {
        engine: engine.to_path_buf(),
        id: id.clone(),
        search_path: config.search_path.clone(),
        active: true,
    };

    let source = format!("{id}:/just");
    let copy = run_output(
        engine,
        &[
            OsStr::new("cp"),
            OsStr::new(&source),
            destination.as_os_str(),
        ],
        config.search_path.as_deref(),
    )
    .map_err(|error| format!("cannot copy /just from container: {error}"))?;
    if !copy.status.success() {
        return Err(format_output_failure(
            "cannot copy /just from container",
            &copy,
        ));
    }

    guard.remove()
}

struct ContainerGuard {
    engine: PathBuf,
    id: String,
    search_path: Option<OsString>,
    active: bool,
}

impl ContainerGuard {
    fn remove(mut self) -> std::result::Result<(), String> {
        let output = run_output(
            &self.engine,
            &[OsStr::new("rm"), OsStr::new("-f"), OsStr::new(&self.id)],
            self.search_path.as_deref(),
        )
        .map_err(|error| format!("cannot remove extraction container: {error}"))?;
        if output.status.success() {
            self.active = false;
            Ok(())
        } else {
            Err(format_output_failure(
                "cannot remove extraction container",
                &output,
            ))
        }
    }
}

impl Drop for ContainerGuard {
    fn drop(&mut self) {
        if self.active {
            let _ = run_output(
                &self.engine,
                &[OsStr::new("rm"), OsStr::new("-f"), OsStr::new(&self.id)],
                self.search_path.as_deref(),
            );
        }
    }
}

fn create_stage(config: &RuntimeConfig) -> std::result::Result<(TempDir, bool), String> {
    if let Some(root) = &config.temporary_root {
        match TempBuilder::new().prefix("pipeline-just-").tempdir_in(root) {
            Ok(directory) => return Ok((directory, true)),
            Err(error) if config.cache_root.is_some() => {
                let _ = error;
            }
            Err(error) => {
                return Err(format!(
                    "cannot create temporary directory in {}: {error}",
                    root.display()
                ));
            }
        }
    }
    create_cache_stage(config).map(|directory| (directory, false))
}

fn create_cache_stage(config: &RuntimeConfig) -> std::result::Result<TempDir, String> {
    let root = config
        .cache_root
        .as_ref()
        .ok_or_else(|| "no user cache directory is available".to_owned())?;
    fs::create_dir_all(root)
        .map_err(|error| format!("cannot create cache directory {}: {error}", root.display()))?;
    #[cfg(unix)]
    fs::set_permissions(root, Permissions::from_mode(0o700))
        .map_err(|error| format!("cannot secure cache directory {}: {error}", root.display()))?;
    TempBuilder::new()
        .prefix("pipeline-just-")
        .tempdir_in(root)
        .map_err(|error| {
            format!(
                "cannot create temporary cache directory in {}: {error}",
                root.display()
            )
        })
}

fn make_executable(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        fs::set_permissions(path, Permissions::from_mode(0o700))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

fn looks_like_noexec(error: &str) -> bool {
    error.contains("Permission denied") || error.contains("permission denied")
}

fn run_output(
    program: &Path,
    arguments: &[&OsStr],
    search_path: Option<&OsStr>,
) -> io::Result<Output> {
    for attempt in 0..4 {
        let mut command = Command::new(program);
        command
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        match search_path {
            Some(path) => {
                command.env("PATH", path);
            }
            None => {
                command.env_remove("PATH");
            }
        }
        match command.output() {
            // A just-written executable can very briefly report ETXTBSY on
            // some filesystems. Rebuilding the command and retrying also makes
            // image extraction resilient to that harmless race.
            Err(error) if error.raw_os_error() == Some(26) && attempt < 3 => {
                std::thread::sleep(Duration::from_millis(2));
            }
            result => return result,
        }
    }
    unreachable!("the final command attempt always returns")
}

fn format_output_failure(context: &str, output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let detail = if !stderr.trim().is_empty() {
        stderr.trim()
    } else if !stdout.trim().is_empty() {
        stdout.trim()
    } else {
        "no diagnostic output"
    };
    format!("{context} ({}): {detail}", output.status)
}

fn default_cache_root() -> Option<PathBuf> {
    env::var_os("XDG_CACHE_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .map(|root| root.join("pipeline"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[cfg(unix)]
    fn executable(path: &Path, contents: &str) {
        fs::write(path, contents).unwrap();
        fs::set_permissions(path, Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn resolves_compatible_just_from_path() {
        let root = tempfile::tempdir().unwrap();
        let just = root.path().join("just");
        executable(&just, "#!/bin/sh\necho 'just 1.57.0'\n");
        let config = RuntimeConfig::new("example/just:tag", "1.56.0")
            .unwrap()
            .with_search_path(Some(root.path().as_os_str().to_owned()))
            .with_container_fallback(false);
        let runtime = JustRuntime::resolve(&config).unwrap();
        assert_eq!(runtime.executable(), fs::canonicalize(just).unwrap());
        assert_eq!(runtime.version(), &Version::new(1, 57, 0));
        assert!(matches!(runtime.source(), RuntimeSource::Path(_)));
    }

    #[test]
    #[cfg(unix)]
    fn rejects_old_installed_and_path_versions_with_diagnostics() {
        let root = tempfile::tempdir().unwrap();
        let just = root.path().join("just");
        executable(&just, "#!/bin/sh\necho 'just 1.55.0'\n");
        let config = RuntimeConfig::new("example/just:tag", "1.56.0")
            .unwrap()
            .with_installed_just(Some(just.clone()))
            .with_search_path(Some(root.path().as_os_str().to_owned()))
            .with_container_fallback(false);
        let error = JustRuntime::resolve(&config).unwrap_err();
        assert!(error.to_string().contains("too old"));
        assert_eq!(error.attempts().len(), 3);
    }

    #[test]
    #[cfg(unix)]
    fn rejects_non_just_executables_with_semver_output() {
        let root = tempfile::tempdir().unwrap();
        let just = root.path().join("just");
        executable(&just, "#!/bin/sh\necho 'not-just 99.0.0'\n");
        let config = RuntimeConfig::new("example/just:tag", "1.56.0")
            .unwrap()
            .with_search_path(Some(root.path().as_os_str().to_owned()))
            .with_container_fallback(false);

        let error = JustRuntime::resolve(&config).unwrap_err();
        assert!(error.to_string().contains("unrecognized output"), "{error}");
    }

    #[test]
    #[cfg(unix)]
    fn podman_pulls_extracts_verifies_and_removes_container() {
        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("bin");
        let tmp = root.path().join("tmp");
        let cache = root.path().join("cache");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(&tmp).unwrap();
        let payload = root.path().join("payload");
        executable(&payload, "#!/bin/sh\necho 'just 1.57.0'\n");
        let log = root.path().join("engine.log");
        let engine = bin.join("podman");
        executable(
            &engine,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\ncase \"$1\" in\n  image) echo 'image not known' >&2; exit 125 ;;\n  pull) exit 0 ;;\n  create) [ \"$#\" -eq 6 ] && [ \"$2\" = --name ] && [ \"$4\" = official/just:1.57 ] && [ \"$5\" = /just ] && [ \"$6\" = --version ] || exit 64; echo container-123; exit 0 ;;\n  cp) /usr/bin/cp '{}' \"$3\"; exit $? ;;\n  rm) exit 0 ;;\nesac\nexit 1\n",
                log.display(),
                payload.display()
            ),
        );

        let config = RuntimeConfig::new("official/just:1.57", "1.56.0")
            .unwrap()
            .with_search_path(Some(bin.as_os_str().to_owned()))
            .with_temporary_root(Some(tmp))
            .with_cache_root(Some(cache));
        let runtime = JustRuntime::resolve(&config).unwrap();
        assert!(runtime.executable().is_file());
        assert_eq!(
            runtime.source(),
            &RuntimeSource::PodmanImage("official/just:1.57".to_owned())
        );
        let calls = fs::read_to_string(log).unwrap();
        assert!(calls.contains("image inspect official/just:1.57"));
        assert!(calls.contains("pull official/just:1.57"));
        let create = calls
            .lines()
            .find(|line| line.starts_with("create "))
            .expect("container create call");
        assert!(create.starts_with("create --name pipeline-just-"));
        assert!(create.ends_with(" official/just:1.57 /just --version"));
        assert!(calls.contains("cp container-123:/just"));
        assert!(calls.contains("rm -f container-123"));
    }

    #[test]
    #[cfg(unix)]
    fn falls_back_to_docker_after_podman_failure_and_to_cache_without_tmp() {
        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("bin");
        let cache = root.path().join("cache");
        fs::create_dir_all(&bin).unwrap();
        executable(&bin.join("podman"), "#!/bin/sh\nexit 1\n");
        let payload = root.path().join("payload");
        executable(&payload, "#!/bin/sh\necho 'just 1.60.0'\n");
        executable(
            &bin.join("docker"),
            &format!(
                "#!/bin/sh\ncase \"$1\" in\n image) exit 0 ;;\n create) [ \"$#\" -eq 6 ] && [ \"$2\" = --name ] && [ \"$4\" = official/just:tag ] && [ \"$5\" = /just ] && [ \"$6\" = --version ] || exit 64; echo docker-id; exit 0 ;;\n cp) /usr/bin/cp '{}' \"$3\"; exit $? ;;\n rm) exit 0 ;;\nesac\nexit 1\n",
                payload.display()
            ),
        );
        let config = RuntimeConfig::new("official/just:tag", "1.56.0")
            .unwrap()
            .with_search_path(Some(bin.as_os_str().to_owned()))
            .with_temporary_root(None)
            .with_cache_root(Some(cache.clone()));
        let runtime = JustRuntime::resolve(&config).unwrap();
        assert!(matches!(runtime.source(), RuntimeSource::DockerImage(_)));
        assert!(runtime.executable().starts_with(cache));
    }

    #[test]
    #[cfg(unix)]
    fn inspect_errors_do_not_trigger_a_pull() {
        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let log = root.path().join("engine.log");
        executable(
            &bin.join("podman"),
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nif [ \"$1\" = image ]; then echo 'cannot connect to service' >&2; exit 125; fi\nexit 99\n",
                log.display()
            ),
        );
        let config = RuntimeConfig::new("official/just:tag", "1.56.0")
            .unwrap()
            .with_search_path(Some(bin.as_os_str().to_owned()))
            .with_container_fallback(true);

        let error = JustRuntime::resolve(&config).unwrap_err();
        assert!(error.to_string().contains("cannot inspect image"));
        assert!(log.exists(), "{error}");
        let calls = fs::read_to_string(log).unwrap();
        assert!(calls.contains("image inspect official/just:tag"));
        assert!(!calls.contains("pull official/just:tag"));
    }
}
