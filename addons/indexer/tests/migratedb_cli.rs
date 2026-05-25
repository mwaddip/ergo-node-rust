use assert_cmd::Command;

#[test]
fn help_lists_all_flags() {
    let output = Command::cargo_bin("ergo-indexer-migratedb")
        .unwrap()
        .arg("--help")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    for flag in ["--in", "--out", "--update-config", "--resume", "-y"] {
        assert!(stdout.contains(flag), "missing {flag} in --help");
    }
}
