use agnes_session::{EventSink, NodeKindTag, SessionEvent};
use std::io::Write;
use std::sync::Arc;
use std::time::Instant;

/// Renders SessionEvents to stderr with a start-time-relative timestamp.
pub struct StderrEventSink {
    start: Instant,
    printed_plan_header: bool,
    printed_trace_header: bool,
}

impl Default for StderrEventSink {
    fn default() -> Self {
        Self {
            start: Instant::now(),
            printed_plan_header: false,
            printed_trace_header: false,
        }
    }
}

impl StderrEventSink {
    pub fn new() -> Self {
        Self::default()
    }

    fn t(&self) -> String {
        let ms = self.start.elapsed().as_millis();
        format!("[+{}.{:03}s]", ms / 1000, ms % 1000)
    }
}

#[async_trait::async_trait]
impl EventSink for StderrEventSink {
    async fn emit(&mut self, ev: SessionEvent) {
        let e = &mut std::io::stderr().lock();
        match ev {
            SessionEvent::PlannerStart => {
                let _ = writeln!(e, "\n━━━ Planning ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
                self.start = Instant::now();
                self.printed_plan_header = false;
                self.printed_trace_header = false;
            }
            SessionEvent::PlannerRetry { attempt, error } => {
                let _ = writeln!(e, "  retry #{attempt}: {error}");
            }
            SessionEvent::DslProduced { source } => {
                let _ = writeln!(e, "━━━ Generated DSL ━━━━━━━━━━━━━━━━━━━━━━━━");
                let _ = writeln!(e, "{source}");
            }
            SessionEvent::PlanReady { tree } => {
                let _ = writeln!(e, "━━━ Plan ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
                let _ = crate::plan_view::render_plan(&tree, e);
                self.printed_plan_header = true;
            }
            SessionEvent::NodeStart { id: _, kind, args } => {
                if !self.printed_trace_header {
                    let _ = writeln!(e, "━━━ Trace ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
                    self.printed_trace_header = true;
                }
                let label = match kind {
                    NodeKindTag::Tool { name } => format!("tool {name}"),
                    NodeKindTag::Llm => "llm".into(),
                };
                let a = if args.is_empty() {
                    String::new()
                } else {
                    format!("  {}", args[0].1)
                };
                let _ = writeln!(e, "{} ▶ {label}{a}", self.t());
            }
            SessionEvent::NodeEnd {
                id: _,
                ok,
                preview,
                elapsed_ms: _,
            } => {
                let glyph = if ok { "✔" } else { "✘" };
                let _ = writeln!(e, "{} {glyph} {preview}", self.t());
            }
            SessionEvent::TurnResult {
                value_preview: _,
                value_type,
            } => {
                let _ = writeln!(e, "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
                let _ = writeln!(e, "(result: {value_type})");
            }
            SessionEvent::TurnFailed { error } => {
                let _ = writeln!(e, "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
                let _ = writeln!(e, "✘ turn failed: {error}");
            }
            SessionEvent::WriteSummary { entries } => {
                let t = self.t();
                let _ = writeln!(e, "writes:");
                // Right-pad the path column so the byte counts line up.
                let width = entries.iter().map(|(p, _)| p.len()).max().unwrap_or(0);
                for (path, bytes) in entries {
                    let _ = writeln!(
                        e,
                        "  {t} would write {:width$}  {bytes} bytes",
                        format!("\"{path}\""),
                        width = width + 2,
                    );
                }
            }
            SessionEvent::IterationStart { iter } => {
                let _ = writeln!(e, "\n─── iteration {iter} ───────────────────────────────");
                self.start = Instant::now();
                self.printed_plan_header = false;
                self.printed_trace_header = false;
            }
            SessionEvent::ObservationEmitted {
                iter,
                text,
                is_error,
            } => {
                let t = self.t();
                let tag = if is_error {
                    "✗ error"
                } else {
                    "↓ observed"
                };
                let char_count = text.chars().count();
                let preview: String = text.chars().take(120).collect();
                let ellipsis = if char_count > 120 { "…" } else { "" };
                let _ = writeln!(
                    e,
                    "{t} {tag} (iter {iter}, {char_count} chars): {preview}{ellipsis}"
                );
            }
            SessionEvent::ShellConfirm { command, responder } => {
                println!();
                println!("\x1b[1m[agnes] Confirm shell execution:\x1b[0m");
                println!("  Command: {}", command);
                print!("  OK to run? [Y/n] ");
                std::io::stdout().flush().unwrap();

                let mut input = String::new();
                std::io::stdin().read_line(&mut input).unwrap();
                let input = input.trim().to_lowercase();

                let approved = input.is_empty() || input == "y" || input == "yes";
                if let Some(tx) = Arc::into_inner(responder) {
                    let _ = tx.send(approved);
                }
            }
            _ => {
                // Future SessionEvent variants render nothing by default.
            }
        }
    }
}
