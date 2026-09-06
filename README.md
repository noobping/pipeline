![License](https://img.shields.io/badge/license-MIT-blue.svg)
[![Release](https://github.com/noobping/pipeline/actions/workflows/release.yml/badge.svg)](https://github.com/noobping/pipeline/actions/workflows/release.yml)

# Pipeline

Pipeline is a small pipeline runner built on [Just](https://just.systems). It adds
pipeline and job dependencies, automatic parallel execution, and native Git-hook
installation while leaving recipes, output, signals, and failures to Just.

There is no daemon or service. Pipeline does not interpret GitHub Actions or
Gitea Actions workflow files; its GitHub Action simply runs a Pipeline definition
inside the published container.

## Requirements

Pipeline uses `just` 1.56 or newer from `PATH` when available. If it cannot find a
compatible executable, it tries Podman and then Docker, copies `/just` from the
official Just image into a secure one-run directory under `/tmp`, and executes it
on the host. Recipes do not run inside that container. If `/tmp` cannot execute
files, Pipeline uses a one-run user-cache directory instead.

The image and minimum version are intentionally easy to update in `Cargo.toml`:

```toml
[package.metadata.pipeline]
just-image = "ghcr.io/casey/just:latest"
just-min-version = "1.56.0"
```

Build and install from source with:

```sh
cargo build --release
cargo install --path . --locked
```

## Container

The published image contains both Pipeline and the configured official Just
binary. It does not need a container socket to execute recipes:

```sh
docker run --rm \
  --user "$(id -u):$(id -g)" \
  --env HOME=/tmp \
  --volume "$PWD:/work" \
  --workdir /work \
  ghcr.io/noobping/pipeline:continuous check
```

Arguments after the image name are passed directly to Pipeline. The image is
published for Linux amd64 and arm64. Its intentionally small runtime contains
Bash, Git, CA certificates, Pipeline, and Just; recipes remain responsible for
any additional tools they use.

When installing Git hooks from an ephemeral container, use `pipeline add --copy`.
The default automatic mode would otherwise link the installed hooks to binaries
inside that container. Files created by a container also use its effective user,
which is why the example maps the current user's numeric identity.

## GitHub Action

Checkout the repository, then invoke the action. `arguments` uses one line for
each command-line argument, so values containing spaces are preserved without
shell evaluation:

```yaml
- uses: actions/checkout@v4
- uses: noobping/pipeline@continuous
  with:
    arguments: |
      --cap-jobs
      4
      check
```

Definitions below the repository root can use the separate directory input:

```yaml
- uses: noobping/pipeline@continuous
  with:
    working-directory: services/api
    arguments: test
```

The Docker action runs as root, as required by GitHub, and marks only the mounted
workspace and selected working directory as safe for Git. It includes the same
small tool set as the published image; use ordinary runner steps or a purpose-built
image for recipes that require a larger host toolchain such as Podman or Buildah.

## Pipeline definitions

Starting in the current directory, Pipeline searches each directory upward. The
first directory containing a definition wins, with this precedence inside it:

1. `Pipelinefile`
2. `pipeline.yml`
3. `.pipeline.yml`

The names are case-sensitive. A `Pipelinefile` is a normal Justfile:

```just
set shell := ["bash", "-cu"]

[parallel]
check: fmt lint test

fmt:
    cargo fmt --check

lint:
    cargo clippy --all-targets -- -D warnings

test:
    cargo test

build: check
    cargo build --release
```

YAML is an alternative definition that connects pipelines to recipes from an
ordinary `justfile`, `Justfile`, or `.justfile` in the same directory or a parent:

```yaml
version: 1

pipelines:
  check:
    jobs:
      format:
        just: format
      lint:
        just: lint
      test:
        needs: [format]
        just: test

  build:
    needs: [check]
    jobs:
      build:
        just: build
```

Independent jobs run concurrently. A job waits for its `needs`; a pipeline waits
for its prerequisite pipelines to finish. A pipeline containing only `needs` is
a valid aggregate. Job targets may use Just module paths such as `docs::check`,
but YAML job targets do not contain arguments.

`version` is optional and defaults to 1. Unknown fields, missing dependencies,
self-dependencies, duplicate dependencies, and cycles are errors.

## Running pipelines

```text
pipeline [--jobs N | --cap-jobs N] [--no-deps] [--] [TARGET...]
```

With a `Pipelinefile`, targets and their arguments are passed to Just. With YAML,
each target names a pipeline; several selected pipelines start concurrently. No
target uses the first declared pipeline or the default Just recipe.

- `--jobs N` gives both the generated scheduler and every nested Just process N
  jobs. Nested parallel recipes can therefore exceed N total processes.
- `--cap-jobs N` gives the scheduler N jobs and nested Just processes one job,
  providing a predictable global cap for YAML jobs.
- `--no-deps` skips native Just recipe dependencies, but never skips YAML pipeline
  or job dependencies.
- Use `--` when a target is named `add`, `remove`, `install`, `uninstall`, or
  `help`.

Pipeline preserves Just's stdout, stderr, stdin, signal behavior, and exit status.
This includes Just's native behavior where a failed shared dependency can be
attempted again when reached through more than one parallel branch.

## Git hooks

Pipeline installs into normal repositories, bare repositories, submodules, and
linked worktrees using Git's common directory:

```sh
pipeline add                         # add every supported hook
pipeline add pre-commit pre-push     # add selected hooks
pipeline add --managed               # add only hooks declared by YAML

pipeline remove                      # remove every Pipeline-owned hook
pipeline remove pre-commit           # remove selected hooks
pipeline remove --managed            # remove only currently declared hooks
```

`install` aliases `add`, and `uninstall` aliases `remove`. Explicit hook names
cannot be combined with `--managed`. An empty managed set is a successful no-op.
A Pipelinefile has one universal `hook` recipe, so its managed set is every
supported hook. Pipeline uses a fixed set based on Git's
[documented hook names](https://git-scm.com/docs/githooks); unsupported names are
errors.

Hooks and private executables are placed below `$GIT_COMMON_DIR`:

```text
hooks/<hook>
pipeline/pipeline
pipeline/just
```

Pipeline refuses to overwrite unmanaged hooks and rejects repositories using an
external `core.hooksPath`. Additions are validated and staged before the whole
set is committed; a failed commit rolls earlier replacements back. The last hook
removal also removes Pipeline's installed executables. External symlink targets
are never removed.

### Link or copy

```text
--link              link both Pipeline and Just
--copy              copy both Pipeline and Just
--link-pipeline     override the Pipeline mode
--copy-pipeline
--link-just         override the Just mode
--copy-just
```

Without an option, a compatible Just found in `PATH` is linked into the Git
installation. An image-extracted Just is copied. Pipeline itself is linked only
when the running executable is the same executable found as `pipeline` in `PATH`;
otherwise it is copied. Explicit link mode requires a suitable executable in
`PATH`. Per-executable options override the global mode.

### YAML triggers

YAML can declare hook triggers at the document, pipeline, or job level:

```yaml
version: 1

on:
  pre-commit: [check, check::lint]

pipelines:
  check:
    on: [pre-push]
    jobs:
      format:
        just: format
      lint:
        on: [commit-msg]
        needs: [format]
        just: lint
```

Triggers from all levels are combined and deduplicated. A pipeline trigger runs
the complete pipeline. A `pipeline::job` trigger runs that job, its transitive job
dependencies, and prerequisite pipelines without running unrelated siblings.

Hook recipes inherit Git's environment and stdin. Pipeline additionally exports
`PIPELINE_HOOK`, `PIPELINE_HOOK_ARGC`, and `PIPELINE_HOOK_ARG_0...`. Receive-hook
runs also expose commit and ref context through `PIPELINE_COMMIT`,
`PIPELINE_REF_COUNT`, and `PIPELINE_REF_0...`.

Normal hooks use the current worktree. Bare receive hooks securely materialize
each unique incoming commit in a temporary worktree and run commits concurrently;
other bare hooks use a temporary tree for `HEAD` (or the configured trusted ref).
Definitions used by hooks must be tracked and cannot be discovered outside the
repository. Submodules and Git LFS objects are not hydrated automatically.

`proc-receive` and `fsmonitor-watchman` have Git-defined stdout protocols. A
declared recipe for either hook owns that protocol and should keep incidental
recipe output off stdout. When no trigger is declared, Pipeline transparently
falls through `proc-receive` commands and conservatively tells fsmonitor that all
paths changed. `push-to-checkout` performs Git's required worktree update after
its declared pipeline succeeds.

## Administrator policy

Optional policy is read from `/etc/pipeline.yml` and then
`$GIT_COMMON_DIR/pipeline/config.yml`:

```yaml
version: 1

hooks:
  enabled: true
  disabled: []
  incoming: project       # project | trusted | skip
  trusted-ref: HEAD
  parallel-refs: true
  missing-trigger: allow  # allow | deny

runtime:
  container-fallback: true
```

The two layers merge toward the more restrictive setting. `trusted` loads control
files from `trusted-ref` while recipes run against the incoming source tree;
`skip` disables incoming execution. Trusted control files are not a sandbox for
commands that build untrusted source.

Allowing pushes to a bare repository whose policy uses `incoming: project` also
allows pushers to execute project-controlled code on that host. Use `trusted` or
`skip` when that trust model is inappropriate.

## License

[MIT](LICENSE)
