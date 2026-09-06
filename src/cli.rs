use std::ffi::OsString;
use std::num::NonZeroUsize;

use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "pipeline")]
#[command(version)]
#[command(about = "A small Just-powered pipeline runner")]
#[command(
    override_usage = "pipeline [--jobs N | --cap-jobs N] [--no-deps] [--] [TARGET...]\n       pipeline <add|remove> [OPTIONS] [HOOK...]"
)]
#[command(
    after_help = "Run options:\n  --jobs N      Limit both pipeline jobs and nested Just recipes\n  --cap-jobs N  Limit pipeline jobs and run each nested Just process with one job\n  --no-deps     Skip Just recipe dependencies (YAML graph dependencies still run)\n\nAliases: `install` = `add`, `uninstall` = `remove`."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    #[command(hide = true)]
    Run(RunArgs),
    #[command(alias = "install", about = "Add Pipeline-managed Git hooks")]
    Add(AddArgs),
    #[command(alias = "uninstall", about = "Remove Pipeline-managed Git hooks")]
    Remove(RemoveArgs),
    #[command(name = "__hook", hide = true)]
    Hook(HookArgs),
}

#[derive(Clone, Debug, Args, Default)]
pub struct RunArgs {
    #[arg(long, value_name = "N", conflicts_with = "cap_jobs")]
    pub jobs: Option<NonZeroUsize>,

    #[arg(long, value_name = "N", conflicts_with = "jobs")]
    pub cap_jobs: Option<NonZeroUsize>,

    #[arg(long)]
    pub no_deps: bool,

    #[arg(
        value_name = "TARGET",
        trailing_var_arg = true,
        allow_hyphen_values = true
    )]
    pub arguments: Vec<OsString>,
}

#[derive(Clone, Debug, Args, Default)]
pub struct AddArgs {
    /// Link both Pipeline and Just from PATH.
    #[arg(long, conflicts_with = "copy")]
    pub link: bool,

    /// Copy both Pipeline and Just into the Git common directory.
    #[arg(long, conflicts_with = "link")]
    pub copy: bool,

    /// Link Pipeline from PATH, overriding the global mode.
    #[arg(long, conflicts_with = "copy_pipeline")]
    pub link_pipeline: bool,

    /// Copy Pipeline, overriding the global mode.
    #[arg(long, conflicts_with = "link_pipeline")]
    pub copy_pipeline: bool,

    /// Link Just from PATH, overriding the global mode.
    #[arg(long, conflicts_with = "copy_just")]
    pub link_just: bool,

    /// Copy Just, overriding the global mode.
    #[arg(long, conflicts_with = "link_just")]
    pub copy_just: bool,

    /// Add only hooks declared by the current Pipeline definition.
    #[arg(long)]
    pub managed: bool,

    /// Supported hooks to add; omit to add all of them.
    #[arg(value_name = "HOOK")]
    pub hooks: Vec<String>,
}

#[derive(Clone, Debug, Args, Default)]
pub struct RemoveArgs {
    /// Remove only installed hooks declared by the current definition.
    #[arg(long)]
    pub managed: bool,

    /// Supported hooks to remove; omit to remove all Pipeline-owned hooks.
    #[arg(value_name = "HOOK")]
    pub hooks: Vec<String>,
}

#[derive(Clone, Debug, Args)]
pub struct HookArgs {
    pub hook: String,

    #[arg(
        value_name = "ARG",
        trailing_var_arg = true,
        allow_hyphen_values = true
    )]
    pub arguments: Vec<OsString>,
}

/// Rewrite Just-like invocations into the hidden `run` subcommand while
/// retaining a small set of explicit management commands.
pub fn rewrite_argv(mut argv: Vec<OsString>) -> Vec<OsString> {
    let Some(first) = argv.get(1).map(OsString::as_os_str) else {
        argv.insert(1, OsString::from("run"));
        return argv;
    };

    if matches!(
        first.to_str(),
        Some("add" | "install" | "remove" | "uninstall" | "__hook" | "help")
    ) || matches!(first.to_str(), Some("-h" | "--help" | "-V" | "--version"))
    {
        return argv;
    }

    argv.insert(1, OsString::from("run"));
    argv
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_targets_to_run() {
        let argv = rewrite_argv(vec!["pipeline".into(), "check".into()]);
        assert_eq!(argv, ["pipeline", "run", "check"]);
    }

    #[test]
    fn separator_forces_management_name_to_be_a_target() {
        let argv = rewrite_argv(vec!["pipeline".into(), "--".into(), "add".into()]);
        assert_eq!(argv, ["pipeline", "run", "--", "add"]);
    }

    #[test]
    fn leaves_management_commands_alone() {
        let argv = rewrite_argv(vec!["pipeline".into(), "install".into()]);
        assert_eq!(argv, ["pipeline", "install"]);
    }

    #[test]
    fn run_is_an_ordinary_target() {
        let argv = rewrite_argv(vec!["pipeline".into(), "run".into()]);
        assert_eq!(argv, ["pipeline", "run", "run"]);
    }
}
