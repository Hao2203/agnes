/// Peel an ```agnes``` (or bare ```) fenced block out of an LLM response.
/// Preference: first ```agnes``` block, else first ``` block, else the
/// full string trimmed. Never errors — the parser downstream will
/// produce a proper error if the extracted content isn't valid agnes.
pub fn extract_dsl(raw: &str) -> String {
    if let Some(block) = fenced(raw, "```agnes") {
        return block;
    }
    if let Some(block) = fenced(raw, "```") {
        return block;
    }
    raw.trim().to_string()
}

fn fenced(raw: &str, open: &str) -> Option<String> {
    let start = raw.find(open)?;
    let after_open = &raw[start + open.len()..];
    // Skip an optional trailing tag line on the opener: "agnes\n" or just "\n".
    let after_line = match after_open.find('\n') {
        Some(nl) => &after_open[nl + 1..],
        None => after_open,
    };
    let end = after_line.find("```")?;
    Some(after_line[..end].trim().to_string())
}
