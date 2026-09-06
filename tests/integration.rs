#![cfg(unix)]

use std::fs;
use std::io::Write;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::{Command, Output, Stdio};

fn pipeline() -> &'static str {
    env!("CARGO_BIN_EXE_pipeline")
}

fn command_available(command: &str) -> bool {
    Command::new(command)
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

fn prerequisites() -> bool {
    command_available("git")
        && Command::new("just")
            .arg("--version")
            .output()
            .ok()
            .filter(|output| output.status.success())
            .is_some_and(|output| {
                String::from_utf8_lossy(&output.stdout)
                    .split_whitespace()
                    .nth(1)
                    .and_then(|version| semver::Version::parse(version).ok())
                    .is_some_and(|version| version >= semver::Version::new(1, 56, 0))
            })
}

fn from_path(name: &str) -> Option<std::path::PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|directory| directory.join(name))
            .find(|candidate| candidate.is_file())
    })
}

fn run(command: &mut Command) -> Output {
    let output = command.output().expect("start command");
    assert!(
        output.status.success(),
        "command failed with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn git(directory: &Path, arguments: &[&str]) -> Output {
    run(Command::new("git").arg("-C").arg(directory).args(arguments))
}

fn init_repository(path: &Path) {
    run(Command::new("git")
        .args(["-c", "init.defaultBranch=main", "init", "-q"])
        .arg(path));
}

fn commit_all(path: &Path, message: &str) {
    git(path, &["add", "."]);
    run(Command::new("git").arg("-C").arg(path).args([
        "-c",
        "user.name=Pipeline Test",
        "-c",
        "user.email=pipeline@example.invalid",
        "commit",
        "-qm",
        message,
    ]));
}

fn install(path: &Path, hook: &str) {
    run(Command::new(pipeline())
        .current_dir(path)
        .args(["add", "--copy", hook]));
}

fn packet(payload: &[u8], output: &mut Vec<u8>) {
    write!(output, "{:04x}", payload.len() + 4).unwrap();
    output.extend_from_slice(payload);
}

#[test]
fn explicit_link_uses_absolute_path_executables() {
    if !prerequisites() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let repository = temp.path().join("repository");
    let bin = temp.path().join("bin");
    fs::create_dir(&bin).unwrap();
    init_repository(&repository);
    symlink(pipeline(), bin.join("pipeline")).unwrap();
    let path = std::env::join_paths(
        std::iter::once(bin.clone())
            .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap()).collect::<Vec<_>>()),
    )
    .unwrap();

    run(Command::new(bin.join("pipeline"))
        .current_dir(&repository)
        .env("PATH", path)
        .args(["add", "--link", "pre-commit"]));

    let installed = repository.join(".git/pipeline");
    assert_eq!(
        fs::read_link(installed.join("pipeline")).unwrap(),
        fs::canonicalize(pipeline()).unwrap()
    );
    assert_eq!(
        fs::read_link(installed.join("just")).unwrap(),
        fs::canonicalize(from_path("just").unwrap()).unwrap()
    );
}

#[test]
fn yaml_jobs_execute_in_parallel() {
    if !prerequisites() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    fs::write(
        temp.path().join("pipeline.yml"),
        "version: 1\npipelines:\n  check:\n    jobs:\n      one:\n        just: one\n      two:\n        just: two\n",
    )
    .unwrap();
    fs::write(
        temp.path().join("justfile"),
        r#"one:
    @touch one.started; i=0; while [ ! -e two.started ] && [ "$i" -lt 200 ]; do sleep 0.01; i=$((i + 1)); done; test -e two.started

two:
    @touch two.started; i=0; while [ ! -e one.started ] && [ "$i" -lt 200 ]; do sleep 0.01; i=$((i + 1)); done; test -e one.started
"#,
    )
    .unwrap();

    run(Command::new(pipeline())
        .current_dir(temp.path())
        .arg("check"));
    assert!(temp.path().join("one.started").exists());
    assert!(temp.path().join("two.started").exists());
}

#[test]
fn bare_pre_receive_runs_against_the_incoming_commit() {
    if !prerequisites() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    let bare = temp.path().join("remote.git");
    init_repository(&source);
    fs::write(
        source.join("Pipelinefile"),
        r#"hook:
    @test "$PIPELINE_HOOK" = pre-receive
    @test -n "$PIPELINE_COMMIT"
    @test "$GIT_LFS_SKIP_SMUDGE" = 1
    @git cat-file -e "$PIPELINE_COMMIT^{commit}"
    @test "$(git rev-parse --show-toplevel)" = "$PWD"
"#,
    )
    .unwrap();
    fs::write(source.join("payload"), "incoming\n").unwrap();
    commit_all(&source, "incoming");

    run(Command::new("git")
        .args(["init", "-q", "--bare"])
        .arg(&bare));
    install(&bare, "pre-receive");
    run(Command::new("git")
        .arg("-C")
        .arg(&source)
        .args(["push", "-q"])
        .arg(&bare)
        .arg("HEAD:refs/heads/main"));

    let remote = git(&bare, &["rev-parse", "refs/heads/main"]);
    let local = git(&source, &["rev-parse", "HEAD"]);
    assert_eq!(remote.stdout, local.stdout);
}

#[test]
fn unborn_bare_proc_receive_falls_through() {
    if !prerequisites() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let bare = temp.path().join("remote.git");
    run(Command::new("git")
        .args(["init", "-q", "--bare"])
        .arg(&bare));
    install(&bare, "proc-receive");

    let mut input = Vec::new();
    packet(b"version=1\0atomic\n", &mut input);
    input.extend_from_slice(b"0000");
    packet(
        b"0000000000000000000000000000000000000000 1111111111111111111111111111111111111111 refs/for/main\n",
        &mut input,
    );
    input.extend_from_slice(b"0000");

    let mut child = Command::new(bare.join("hooks/proc-receive"))
        .current_dir(&bare)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(&input).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let mut expected = Vec::new();
    packet(b"version=1\0atomic\n", &mut expected);
    expected.extend_from_slice(b"0000");
    packet(b"ok refs/for/main\n", &mut expected);
    packet(b"option fall-through\n", &mut expected);
    expected.extend_from_slice(b"0000");
    assert_eq!(output.stdout, expected);
}

#[test]
fn push_to_checkout_preserves_update_instead_semantics() {
    if !prerequisites() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    let remote = temp.path().join("remote");
    init_repository(&source);
    fs::write(
        source.join("Pipelinefile"),
        "hook:\n    @test \"$PIPELINE_HOOK\" = push-to-checkout\n",
    )
    .unwrap();
    fs::write(source.join("payload"), "old\n").unwrap();
    commit_all(&source, "initial");

    run(Command::new("git")
        .args(["clone", "-q"])
        .arg(&source)
        .arg(&remote));
    git(
        &remote,
        &["config", "receive.denyCurrentBranch", "updateInstead"],
    );
    install(&remote, "push-to-checkout");

    fs::write(source.join("payload"), "new\n").unwrap();
    commit_all(&source, "update");
    run(Command::new("git")
        .arg("-C")
        .arg(&source)
        .args(["push", "-q"])
        .arg(&remote)
        .arg("HEAD:main"));

    assert_eq!(fs::read_to_string(remote.join("payload")).unwrap(), "new\n");
    assert!(git(&remote, &["status", "--porcelain"]).stdout.is_empty());
}
