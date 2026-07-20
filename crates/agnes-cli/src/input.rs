//! REPL-side input helpers. Kept intentionally tiny; the real parser
//! lives in `agnes-parser`. We only need enough smarts to know when the
//! multi-line entry is complete.

/// Simple paren balancer that skips runs inside "..." string literals.
/// Handles `\"` escapes. Not a full parser — it just tells the REPL when
/// to submit the buffer to `agnes-parser`.
pub fn is_balanced(s: &str) -> bool {
    let mut depth: i32 = 0;
    let mut in_str = false;
    let mut esc = false;
    for ch in s.chars() {
        if in_str {
            if esc {
                esc = false;
                continue;
            }
            match ch {
                '\\' => esc = true,
                '"' => in_str = false,
                _ => {}
            }
            continue;
        }
        match ch {
            '"' => in_str = true,
            '(' => depth += 1,
            ')' => depth -= 1,
            _ => {}
        }
        if depth < 0 {
            return false;
        }
    }
    depth == 0 && !in_str
}
