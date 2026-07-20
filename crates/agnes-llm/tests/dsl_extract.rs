use agnes_llm::extract_dsl;

#[test]
fn peels_fenced_agnes_block() {
    let raw = "Sure! Here you go:\n\n```agnes\n(pipe (tool read-file :path \"README.md\"))\n```\n\nHope that helps.";
    let out = extract_dsl(raw);
    assert_eq!(out, "(pipe (tool read-file :path \"README.md\"))");
}

#[test]
fn peels_fenced_block_without_lang_tag() {
    let raw = "```\n(tool llm :prompt \"hi\")\n```";
    let out = extract_dsl(raw);
    assert_eq!(out, "(tool llm :prompt \"hi\")");
}

#[test]
fn passes_through_when_no_fence() {
    let raw = "(tool llm :prompt \"hi\")";
    assert_eq!(extract_dsl(raw), raw);
}

#[test]
fn picks_first_agnes_fence_when_multiple() {
    let raw = "```agnes\nA\n```\n```agnes\nB\n```";
    assert_eq!(extract_dsl(raw), "A");
}
