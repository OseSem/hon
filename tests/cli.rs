use std::process::{Command, Output};

fn hon(args: &[&str]) -> (tempfile::TempDir, Output) {
    let home = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_hon"))
        .args(args)
        .env("HON_HOME", home.path())
        .output()
        .unwrap();
    (home, output)
}

#[test]
fn help_lists_commands() {
    let (_home, out) = hon(&["--help"]);
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    let commands: Vec<&str> = stdout
        .lines()
        .skip_while(|l| !l.starts_with("Commands:"))
        .skip(1)
        .take_while(|l| !l.is_empty())
        .filter_map(|l| l.split_whitespace().next())
        .collect();
    for cmd in ["search", "download", "read"] {
        assert!(commands.contains(&cmd), "{cmd} missing from:\n{stdout}");
    }
    assert!(!commands.contains(&"man"), "{stdout}");
}

#[test]
fn bad_source_flag_lists_valid() {
    let (_home, out) = hon(&["download", "x", "1", "--source", "foo"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("mangadex"), "{stderr}");
    assert!(stderr.contains("mangakatana"), "{stderr}");
}

#[test]
fn interactive_and_unbuilt_modes_fail_clearly() {
    for (args, msg) in [
        (
            &[][..],
            "error: hon needs an interactive terminal (try `hon search`)",
        ),
        (&["read", "x"][..], "error: read isn't built yet"),
    ] {
        let (_home, out) = hon(args);
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        let stderr = String::from_utf8(out.stderr).unwrap();
        assert!(stderr.contains(msg), "{args:?}: {stderr}");
    }
}

#[test]
fn bad_config_reports_cause() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("config.toml"), "colour = \"red\"\n").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_hon"))
        .args(["search", "x"])
        .env("HON_HOME", home.path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.starts_with("error: "), "{stderr}");
    assert!(stderr.contains("colour"), "{stderr}");
}

#[test]
fn man_page_is_current() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/man/hon.1");
    let committed = std::fs::read(path).unwrap_or_default();
    assert!(
        committed == hon::cli::man_page(),
        "man/hon.1 is out of date; run `cargo run -- man > man/hon.1`"
    );
}
