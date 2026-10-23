use std::path::Path;
use std::process::Command;

#[test]
fn thais_tui_does_not_depend_on_core() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root");
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let output = Command::new(cargo)
        .args(["tree", "-p", "thais-tui", "-e", "normal"])
        .current_dir(&root)
        .output()
        .expect("cargo tree");

    assert!(
        output.status.success(),
        "cargo tree failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let tree = String::from_utf8_lossy(&output.stdout);
    assert!(
        !tree.lines().any(|line| line.contains("thais-core")),
        "thais-tui must not depend on thais-core:\n{tree}"
    );
}
