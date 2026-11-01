//! The command's help: the whole of it, and each command's own part, asked by `--help` or `-h`.

use std::process::{Command, Output};

/// The commands the help names.
const COMMANDS: [&str; 14] = [
    "write",
    "exec",
    "replica",
    "read",
    "out",
    "blocks",
    "census",
    "recut",
    "catalog",
    "functions",
    "types",
    "operators",
    "opclasses",
    "hold",
];

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_warren-snail"))
        .args(args)
        .output()
        .unwrap()
}

fn printed(out: &Output) -> String {
    String::from_utf8(out.stdout.clone()).unwrap()
}

#[test]
fn the_whole_help_names_every_command() {
    let out = run(&["--help"]);
    assert!(out.status.success());
    let help = printed(&out);
    for c in COMMANDS {
        assert!(help.contains(&format!("warren-snail {c} ")), "{c}: {help}");
        assert!(help.contains(&format!("\n  {c} ")), "{c}: {help}");
    }
}

#[test]
fn each_command_prints_its_own_usage_and_no_other() {
    for c in COMMANDS {
        for asked in ["--help", "-h"] {
            let out = run(&[c, asked]);
            assert!(out.status.success(), "{c} {asked}: {out:?}");
            assert!(out.stderr.is_empty(), "{c} {asked}: {out:?}");
            let help = printed(&out);
            assert!(
                help.starts_with(&format!("usage: warren-snail {c} ")),
                "{c} {asked}: {help}"
            );
            assert!(help.contains(&format!("\n  {c} ")), "{c} {asked}: {help}");
            for other in COMMANDS.iter().filter(|&&o| o != c) {
                assert!(
                    !help.contains(&format!("warren-snail {other} ")),
                    "{c} {asked} names {other}: {help}"
                );
                assert!(
                    !help.contains(&format!("\n  {other} ")),
                    "{c} {asked} says what {other} does: {help}"
                );
            }
        }
    }
}

#[test]
fn help_after_other_arguments_is_still_the_command_s_help() {
    let out = run(&["read", "no.snail", "question.sql", "--help"]);
    assert!(out.status.success(), "{out:?}");
    assert!(printed(&out).starts_with("usage: warren-snail read "));
    let out = run(&[
        "read",
        "no.snail",
        "question.sql",
        "--out",
        "a.parquet",
        "--text",
        "-h",
    ]);
    assert!(out.status.success(), "{out:?}");
    assert!(printed(&out).starts_with("usage: warren-snail read "));
}

#[test]
fn help_as_the_value_of_a_flag_is_no_help() {
    // `--param --help` gives $1 the text `--help`: the command runs, and refuses the file it
    // cannot open, rather than printing help.
    let out = run(&[
        "read",
        "no-such.snail",
        "question.sql",
        "--out",
        "a.parquet",
        "--param",
        "--help",
    ]);
    assert!(!out.status.success(), "{out:?}");
    assert!(out.stdout.is_empty(), "{out:?}");
    let refused = String::from_utf8(out.stderr).unwrap();
    assert!(!refused.contains("usage:"), "{refused}");
}
