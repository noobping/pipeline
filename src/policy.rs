//! Administrator policy for installed Git hooks.
//!
//! Policy is deliberately small and has only two layers: a machine-wide file
//! and a repository-local file.  A repository may tighten machine policy, but
//! it cannot loosen it.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

pub const SYSTEM_POLICY_PATH: &str = "/etc/pipeline.yml";
pub const REPOSITORY_POLICY_RELATIVE_PATH: &str = "pipeline/config.yml";
pub const POLICY_VERSION: u32 = 1;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Policy {
    pub version: u32,
    pub hooks: HooksPolicy,
    pub runtime: RuntimePolicy,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HooksPolicy {
    pub enabled: bool,
    pub disabled: BTreeSet<String>,
    pub incoming: IncomingPolicy,
    pub trusted_ref: Option<String>,
    pub parallel_refs: bool,
    pub missing_trigger: MissingTriggerPolicy,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimePolicy {
    pub container_fallback: bool,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Ord, PartialEq, PartialOrd)]
#[serde(rename_all = "kebab-case")]
pub enum IncomingPolicy {
    #[default]
    Project,
    Trusted,
    Skip,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Ord, PartialEq, PartialOrd)]
#[serde(rename_all = "kebab-case")]
pub enum MissingTriggerPolicy {
    #[default]
    Allow,
    Deny,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            version: POLICY_VERSION,
            hooks: HooksPolicy::default(),
            runtime: RuntimePolicy::default(),
        }
    }
}

impl Default for HooksPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            disabled: BTreeSet::new(),
            incoming: IncomingPolicy::Project,
            trusted_ref: None,
            parallel_refs: true,
            missing_trigger: MissingTriggerPolicy::Allow,
        }
    }
}

impl Default for RuntimePolicy {
    fn default() -> Self {
        Self {
            container_fallback: true,
        }
    }
}

impl Policy {
    /// Load `/etc/pipeline.yml` and `$GIT_COMMON_DIR/pipeline/config.yml`.
    /// Missing files contribute the permissive defaults.
    pub fn load(git_common_dir: impl AsRef<Path>) -> Result<Self, PolicyError> {
        Self::load_from_paths(
            Path::new(SYSTEM_POLICY_PATH),
            git_common_dir
                .as_ref()
                .join(REPOSITORY_POLICY_RELATIVE_PATH),
        )
    }

    /// Load policy from explicit paths.  This is also useful to embedders and
    /// keeps tests independent from the host's `/etc`.
    pub fn load_from_paths(
        system_path: impl AsRef<Path>,
        repository_path: impl AsRef<Path>,
    ) -> Result<Self, PolicyError> {
        let system = PolicyLayer::read_optional(system_path.as_ref())?;
        let repository = PolicyLayer::read_optional(repository_path.as_ref())?;
        Self::from_layers(system, repository)
    }

    /// Parse and merge two policy documents. `None` means the layer is absent.
    pub fn from_yaml_layers(
        system: Option<&str>,
        repository: Option<&str>,
    ) -> Result<Self, PolicyError> {
        let system = system
            .map(|text| PolicyLayer::parse(text, Path::new("<system policy>")))
            .transpose()?;
        let repository = repository
            .map(|text| PolicyLayer::parse(text, Path::new("<repository policy>")))
            .transpose()?;
        Self::from_layers(system, repository)
    }

    fn from_layers(
        system: Option<PolicyLayer>,
        repository: Option<PolicyLayer>,
    ) -> Result<Self, PolicyError> {
        let system = system.unwrap_or_default();
        let repository = repository.unwrap_or_default();

        let system_hooks = system.hooks.unwrap_or_default();
        let repository_hooks = repository.hooks.unwrap_or_default();
        let system_runtime = system.runtime.unwrap_or_default();
        let repository_runtime = repository.runtime.unwrap_or_default();

        let incoming = std::cmp::max(
            system_hooks.incoming.unwrap_or_default(),
            repository_hooks.incoming.unwrap_or_default(),
        );
        let system_trusted_ref = validate_trusted_ref(system_hooks.trusted_ref)?;
        let repository_trusted_ref = validate_trusted_ref(repository_hooks.trusted_ref)?;
        let trusted_ref = system_trusted_ref.or(repository_trusted_ref);

        if incoming == IncomingPolicy::Trusted && trusted_ref.is_none() {
            return Err(PolicyError::TrustedRefRequired);
        }

        let mut disabled = BTreeSet::new();
        disabled.extend(system_hooks.disabled.unwrap_or_default());
        disabled.extend(repository_hooks.disabled.unwrap_or_default());
        if let Some(name) = disabled
            .iter()
            .find(|name| !crate::hooks::is_supported_hook(name))
        {
            return Err(PolicyError::InvalidDisabledHook(name.clone()));
        }

        Ok(Self {
            version: POLICY_VERSION,
            hooks: HooksPolicy {
                enabled: system_hooks.enabled.unwrap_or(true)
                    && repository_hooks.enabled.unwrap_or(true),
                disabled,
                incoming,
                trusted_ref,
                parallel_refs: system_hooks.parallel_refs.unwrap_or(true)
                    && repository_hooks.parallel_refs.unwrap_or(true),
                missing_trigger: std::cmp::max(
                    system_hooks.missing_trigger.unwrap_or_default(),
                    repository_hooks.missing_trigger.unwrap_or_default(),
                ),
            },
            runtime: RuntimePolicy {
                container_fallback: system_runtime.container_fallback.unwrap_or(true)
                    && repository_runtime.container_fallback.unwrap_or(true),
            },
        })
    }

    pub fn hook_enabled(&self, hook: &str) -> bool {
        self.hooks.enabled && !self.hooks.disabled.contains(hook)
    }

    pub fn missing_trigger_is_success(&self) -> bool {
        self.hooks.missing_trigger == MissingTriggerPolicy::Allow
    }
}

fn validate_trusted_ref(value: Option<String>) -> Result<Option<String>, PolicyError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let trimmed = value.trim();
    if trimmed.is_empty() {
        Err(PolicyError::InvalidTrustedRef)
    } else {
        Ok(Some(trimmed.to_owned()))
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct PolicyLayer {
    version: Option<u32>,
    hooks: Option<HooksLayer>,
    runtime: Option<RuntimeLayer>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
struct HooksLayer {
    enabled: Option<bool>,
    disabled: Option<Vec<String>>,
    incoming: Option<IncomingPolicy>,
    trusted_ref: Option<String>,
    parallel_refs: Option<bool>,
    missing_trigger: Option<MissingTriggerPolicy>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
struct RuntimeLayer {
    container_fallback: Option<bool>,
}

impl PolicyLayer {
    fn read_optional(path: &Path) -> Result<Option<Self>, PolicyError> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(PolicyError::Read {
                    path: path.to_path_buf(),
                    source,
                })
            }
        };
        Self::parse(&text, path).map(Some)
    }

    fn parse(text: &str, path: &Path) -> Result<Self, PolicyError> {
        let layer: Self = yaml_serde::from_str(text).map_err(|error| PolicyError::Parse {
            path: path.to_path_buf(),
            message: error.to_string(),
        })?;
        if let Some(version) = layer.version {
            if version != POLICY_VERSION {
                return Err(PolicyError::UnsupportedVersion {
                    path: path.to_path_buf(),
                    version,
                });
            }
        }
        Ok(layer)
    }
}

#[derive(Debug, Error)]
pub enum PolicyError {
    #[error("could not read policy {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("invalid policy {path}: {message}")]
    Parse { path: PathBuf, message: String },

    #[error("unsupported policy version {version} in {path}; expected version 1")]
    UnsupportedVersion { path: PathBuf, version: u32 },

    #[error("hooks.incoming is `trusted`, but hooks.trusted-ref is not configured")]
    TrustedRefRequired,

    #[error("hooks.trusted-ref must not be empty")]
    InvalidTrustedRef,

    #[error("hooks.disabled contains an unsupported hook name: {0:?}")]
    InvalidDisabledHook(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_permissive() {
        let policy = Policy::from_yaml_layers(None, None).unwrap();
        assert_eq!(policy, Policy::default());
        assert!(policy.hook_enabled("pre-commit"));
        assert!(policy.missing_trigger_is_success());
    }

    #[test]
    fn layers_only_become_more_restrictive() {
        let system = r#"
hooks:
  enabled: true
  disabled: [pre-push]
  incoming: trusted
  trusted-ref: refs/heads/pipeline
  parallel-refs: false
runtime:
  container-fallback: true
"#;
        let repository = r#"
version: 1
hooks:
  enabled: false
  disabled: [pre-commit]
  incoming: skip
  trusted-ref: refs/heads/ignored
  missing-trigger: deny
runtime:
  container-fallback: false
"#;

        let policy = Policy::from_yaml_layers(Some(system), Some(repository)).unwrap();
        assert!(!policy.hooks.enabled);
        assert_eq!(
            policy.hooks.disabled,
            BTreeSet::from(["pre-commit".to_owned(), "pre-push".to_owned()])
        );
        assert_eq!(policy.hooks.incoming, IncomingPolicy::Skip);
        assert_eq!(
            policy.hooks.trusted_ref.as_deref(),
            Some("refs/heads/pipeline")
        );
        assert!(!policy.hooks.parallel_refs);
        assert_eq!(policy.hooks.missing_trigger, MissingTriggerPolicy::Deny);
        assert!(!policy.runtime.container_fallback);
    }

    #[test]
    fn repository_can_supply_ref_when_system_only_selects_trusted_mode() {
        let policy = Policy::from_yaml_layers(
            Some("hooks:\n  incoming: trusted\n"),
            Some("hooks:\n  trusted-ref: v1\n"),
        )
        .unwrap();
        assert_eq!(policy.hooks.trusted_ref.as_deref(), Some("v1"));
    }

    #[test]
    fn trusted_mode_requires_a_revision() {
        assert!(matches!(
            Policy::from_yaml_layers(Some("hooks:\n  incoming: trusted\n"), None),
            Err(PolicyError::TrustedRefRequired)
        ));
    }

    #[test]
    fn rejects_unknown_fields_and_versions() {
        assert!(matches!(
            Policy::from_yaml_layers(Some("surprise: true\n"), None),
            Err(PolicyError::Parse { .. })
        ));
        assert!(matches!(
            Policy::from_yaml_layers(Some("version: 2\n"), None),
            Err(PolicyError::UnsupportedVersion { version: 2, .. })
        ));
        assert!(matches!(
            Policy::from_yaml_layers(Some("hooks:\n  disabled: [not-a-hook]\n"), None),
            Err(PolicyError::InvalidDisabledHook(_))
        ));
    }

    #[test]
    fn loads_optional_files() {
        let temp = tempfile::tempdir().unwrap();
        let system = temp.path().join("system.yml");
        let repository = temp.path().join("repo.yml");
        fs::write(&repository, "hooks:\n  disabled: [update]\n").unwrap();

        let policy = Policy::load_from_paths(system, repository).unwrap();
        assert!(!policy.hook_enabled("update"));
        assert!(policy.hook_enabled("pre-commit"));
    }
}
