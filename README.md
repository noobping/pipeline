![License](https://img.shields.io/badge/license-MIT-blue.svg)
[![Release](https://github.com/noobping/pipeline/actions/workflows/release.yml/badge.svg)](https://github.com/noobping/pipeline/actions/workflows/release.yml)

# Pipeline

Pipeline is a small task runner built on [Just](https://just.systems). Use a
normal `Pipelinefile`, or connect parallel jobs and dependencies in
`pipeline.yml`. The same tasks can also run as Git hooks in working or bare
repositories.

## Quick start

A `Pipelinefile` is an ordinary Justfile:

```just
set shell := ["bash", "-cu"]

default: check

[parallel]
check: format lint test

format:
    cargo fmt --check

lint:
    cargo clippy --all-targets -- -D warnings

test:
    cargo test

build: check
    cargo build --release
```

Run it with:

```sh
pipeline check
pipeline build
```

Pipeline searches the current directory and its parents. Within one directory,
definition precedence is `Pipelinefile`, `pipeline.yml`, then `.pipeline.yml`.

## YAML pipelines

YAML connects named pipelines and jobs to recipes in a nearby `justfile`,
`Justfile`, or `.justfile`:

```yaml
version: 1

pipelines:
  check:
    jobs:
      lint:
        just: lint
      test:
        just: test

  build:
    needs: [check]
    jobs:
      package:
        just: build
```

Independent jobs and pipeline dependencies run in parallel. A job waits for its
`needs`, and a pipeline waits for its prerequisite pipelines. Aggregate
pipelines containing only `needs` are valid.

Unknown fields, missing dependencies, duplicate dependencies, and cycles are
reported before anything runs. YAML job targets may use Just module paths such
as `docs::check`, but cannot contain arguments.

## Running

```text
pipeline [--jobs N | --cap-jobs N] [--no-deps] [--] [TARGET...]
```

With no target, YAML runs its first declared pipeline and a `Pipelinefile` runs
Just's default recipe. Multiple YAML targets start concurrently.

- `--jobs N` gives the scheduler and every nested Just process `N` jobs.
- `--cap-jobs N` gives the scheduler `N` jobs and nested Just processes one job,
  providing a predictable cap for YAML jobs.
- `--no-deps` skips native Just recipe dependencies, but not YAML dependencies.
- Use `--` before a target named `add`, `remove`, `install`, `uninstall`, or
  `help`.

Recipe output, stdin, signals, failures, and exit codes come directly from Just.

## Install and Just

Build or install Pipeline from source:

```sh
cargo build --locked --release
cargo install --path . --locked
```

Pipeline uses a compatible `just` from `PATH`. If none is available, it uses
Podman or Docker to copy Just from the configured official image into a private
temporary directory, then runs recipes on the host.

The image and minimum version are update knobs in `Cargo.toml`:

```toml
[package.metadata.pipeline]
just-image = "ghcr.io/casey/just:latest"
just-min-version = "1.56.0"
```

After the initial Cargo build, Pipeline builds itself:

```sh
target/release/pipeline build
```

## Container and GitHub Action

The Linux amd64/arm64 image bundles Pipeline, Just, Bash, Git, and CA
certificates. Arguments after the image name go directly to Pipeline:

```sh
podman run --rm --userns=keep-id \
  --env HOME=/tmp \
  --volume "$PWD:/work:Z" \
  --workdir /work \
  ghcr.io/noobping/pipeline:continuous check
```

Docker works with equivalent volume, user, and working-directory options. The
image has no container socket or project-specific toolchain; recipes needing
Podman, Buildah, Rust, or other tools need a suitable host or custom image.

The GitHub Action builds the same Dockerfile and runs Pipeline after checkout.
`arguments` contains one command-line argument per line, so values containing
spaces are not evaluated by a shell:

```yaml
- uses: actions/checkout@v4
- uses: noobping/pipeline@continuous
  with:
    arguments: |
      --cap-jobs
      4
      check
```

Use `working-directory: services/api` for a definition below the repository
root. From an ephemeral container, install hooks with `pipeline add --copy`;
linked binaries disappear with the container.

## Git hooks

Pipeline supports normal repositories, `.git` directories, linked worktrees,
submodules, and bare repositories:

```sh
pipeline add                         # every supported hook
pipeline add pre-commit pre-push     # selected hooks
pipeline add --managed               # YAML hooks; all for a Pipelinefile

pipeline remove                      # every Pipeline-owned hook
pipeline remove pre-commit           # selected hooks
pipeline remove --managed            # YAML hooks; all for a Pipelinefile
```

`install` and `uninstall` are aliases for `add` and `remove`. Pipeline refuses
to overwrite unmanaged hooks or use an external `core.hooksPath`.

A `Pipelinefile` exposes one universal `hook` recipe and receives the hook name
in `PIPELINE_HOOK`. YAML can select pipelines or individual jobs:

```yaml
on:
  pre-commit: [check]
  pre-push: [check::lint]
```

With no mode option, Pipeline links compatible executables found in `PATH` and
copies image-extracted executables. Override that with `--link` or `--copy`, or
per executable with `--link-pipeline`, `--copy-pipeline`, `--link-just`, and
`--copy-just`.

Hook definitions must be tracked by Git. Receive hooks in bare repositories run
against incoming commits, optionally in parallel. The default policy,
`incoming: project`, therefore lets pushers provide executable hook definitions.
For a shared server, keep definitions on a trusted ref in `/etc/pipeline.yml` or
`$GIT_COMMON_DIR/pipeline/config.yml`:

```yaml
version: 1
hooks:
  incoming: trusted
  trusted-ref: HEAD
```

Use `incoming: skip` to disable incoming-code execution entirely. Trusted
definitions control the recipes but are not a sandbox for the source those
recipes inspect or build.
