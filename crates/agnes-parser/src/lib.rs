//! S-expression parser for agnes DSL.
//!
//! Wraps the `lexpr` crate; walks the resulting sexpr tree into
//! `agnes_ast` types. Keyword args (`:key value`) are treated as pairs.
//!
//! ## lexpr 0.2 adaptations
//!
//! - `lexpr` 0.2 does not accept a bare `|` as a symbol start (SYMBOL_EXTENDED
//!   in `lexpr/src/parse/mod.rs` excludes `|`), so the DSL's `(A | B)` union
//!   syntax fails at tokenization. We preprocess the source, replacing every
//!   `|` byte that is not inside a string literal with the sentinel symbol
//!   `__agnes_union_bar__`, then interpret that sentinel back to a union
//!   separator when building the `TypeExprAst`.
//! - `Value::as_vector()` does not exist in 0.2; we use `Value::to_vec()` which
//!   works for both `[...]` (with default `Brackets::List`, parsed as a list)
//!   and `(...)` forms.
//! - Keyword args (`:foo`) are enabled with `KeywordSyntax::ColonPrefix`. They
//!   surface as `Value::Keyword(name)` where `name` has the leading `:`
//!   already stripped.

pub mod error;
mod expr;
mod toplevel;

use agnes_ast::{Expr, Program, Span};
pub use error::ParseError;

/// The sentinel symbol we substitute for the union `|` operator before feeding
/// source to lexpr. Chosen so it cannot collide with a real identifier written
/// in agnes source (double underscores + `agnes` prefix).
pub(crate) const UNION_BAR_SENTINEL: &str = "__agnes_union_bar__";

/// Parse an entire .agnes source file.
///
/// A file is a sequence of top-level forms. A `(declare ...)` or
/// `(define ...)` produces a `TopLevel`; anything else at the top is
/// treated as the `main` expression (only the last such expression
/// wins, and a parse error is returned if more than one appears).
pub fn parse(source: &str) -> Result<Program, ParseError> {
    let prepared = preprocess_union_bars(source);
    let forms = read_forms(&prepared)?;
    let mut toplevels = Vec::new();
    let mut main: Option<Expr> = None;

    for form in forms {
        let span = Span::DUMMY;
        if is_toplevel(&form) {
            toplevels.push(toplevel::parse_toplevel(&form, span)?);
        } else {
            if main.is_some() {
                return Err(ParseError {
                    span,
                    message: "multiple main expressions at top level; wrap them in a single (pipe ...) or (par ...)".into(),
                });
            }
            main = Some(expr::parse_expr(&form, span)?);
        }
    }

    Ok(Program { toplevels, main })
}

/// Read all top-level forms from source. Uses `lexpr` for tokenization.
fn read_forms(source: &str) -> Result<Vec<lexpr::Value>, ParseError> {
    use lexpr::parse::{KeywordSyntax, Options, StringSyntax};
    let opts = Options::new()
        .with_string_syntax(StringSyntax::R6RS)
        .with_keyword_syntax(KeywordSyntax::ColonPrefix);
    let mut parser = lexpr::Parser::from_str_custom(source, opts);
    let mut out = Vec::new();
    for r in parser.value_iter() {
        match r {
            Ok(v) => out.push(v),
            Err(e) => {
                return Err(ParseError {
                    span: Span::DUMMY,
                    message: format!("{e}"),
                });
            }
        }
    }
    Ok(out)
}

/// Replace every `|` character outside of string literals with a
/// whitespace-padded sentinel symbol so that lexpr accepts the union operator
/// as a plain symbol.
///
/// String literals (`"..."`) preserve `|` untouched. Backslash escapes inside
/// strings are honored so `"a\|b"` (an escaped bar in an R6RS string) is left
/// alone as well.
///
/// Walks `char_indices()` and matches on `char` (not raw bytes) so multi-byte
/// UTF-8 sequences pass through intact.
fn preprocess_union_bars(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    let mut in_str = false;
    let mut escape = false;
    for (_, c) in source.char_indices() {
        if in_str {
            out.push(c);
            if escape {
                escape = false;
            } else if c == '\\' {
                escape = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_str = true;
                out.push('"');
            }
            '|' => {
                out.push(' ');
                out.push_str(UNION_BAR_SENTINEL);
                out.push(' ');
            }
            _ => out.push(c),
        }
    }
    out
}

fn is_toplevel(form: &lexpr::Value) -> bool {
    let cons = match form {
        lexpr::Value::Cons(c) => c,
        _ => return false,
    };
    let head = cons.car().as_symbol();
    matches!(head, Some("declare") | Some("define"))
}
