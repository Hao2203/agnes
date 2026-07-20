use agnes_cli::cli::{Args, Command};
use clap::Parser;

#[test]
fn max_turns_defaults_to_none() {
    let a = Args::try_parse_from(["agnes", "chat"]).unwrap();
    assert!(matches!(a.cmd, Some(Command::Chat)));
    assert!(a.max_turns.is_none());
}

#[test]
fn max_turns_from_flag() {
    let a = Args::try_parse_from(["agnes", "--max-turns", "42", "chat"]).unwrap();
    assert_eq!(a.max_turns, Some(42));
}

#[test]
fn max_turns_rejects_non_numeric() {
    let e = Args::try_parse_from(["agnes", "--max-turns", "abc", "chat"]);
    assert!(e.is_err());
}
