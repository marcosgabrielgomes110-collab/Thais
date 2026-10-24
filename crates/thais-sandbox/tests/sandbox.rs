#![cfg(target_os = "linux")]

use std::process::Command;

#[test]
fn landlock_allows_workspace_writes() {
    let workspace = tempfile::tempdir().expect("workspace");
    let output = Command::new(env!("CARGO_BIN_EXE_thais-sandbox"))
        .arg("--workspace")
        .arg(workspace.path())
        .arg("--cwd")
        .arg(workspace.path())
        .arg("--")
        .arg("printf inside > allowed.txt")
        .output()
        .expect("sandbox");
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("allowed.txt")).expect("file"),
        "inside"
    );
}

#[test]
fn landlock_blocks_writes_outside_the_workspace() {
    let workspace = tempfile::tempdir().expect("workspace");
    let outside = tempfile::tempdir().expect("outside");
    let target = outside.path().join("escaped.txt");
    let command = format!("printf outside > {}", target.display());
    let output = Command::new(env!("CARGO_BIN_EXE_thais-sandbox"))
        .arg("--workspace")
        .arg(workspace.path())
        .arg("--cwd")
        .arg(workspace.path())
        .arg("--")
        .arg(command)
        .output()
        .expect("sandbox");
    assert!(!output.status.success());
    assert!(!target.exists());
}
