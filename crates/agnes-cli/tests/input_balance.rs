use agnes_cli::input::is_balanced;

#[test]
fn one_liner_is_balanced() {
    assert!(is_balanced("(tool foo)"));
}
#[test]
fn multiline_open_not_balanced() {
    assert!(!is_balanced("(pipe\n  (tool"));
}
#[test]
fn parens_inside_string_ignored() {
    assert!(is_balanced(r#"(tool x "(a)b")"#));
}
#[test]
fn escaped_quote_in_string() {
    assert!(is_balanced(r#"(tool x "a\"b")"#));
}
