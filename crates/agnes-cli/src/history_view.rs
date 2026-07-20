//! Render agnes-llm::Turn history for the `/history` slash command.

use agnes_llm::{Turn, TurnOutcome};
use std::io::Write;

pub fn render_history(turns: &[Turn], out: &mut dyn Write) -> std::io::Result<()> {
    for (i, t) in turns.iter().enumerate() {
        writeln!(out, "--- turn {i} ---")?;
        writeln!(out, "user: {}", t.user_nl)?;
        for (j, it) in t.iterations.iter().enumerate() {
            writeln!(out, "iter {j}: {}", it.assistant_dsl)?;
            if let Some(obs) = &it.observation {
                let label = if obs.is_error {
                    "error".to_string()
                } else {
                    obs.type_name
                        .as_ref()
                        .map(|n| n.0.clone())
                        .unwrap_or_else(|| "?".to_string())
                };
                writeln!(out, "  obs ({label}): {}", obs.text)?;
            }
        }
        match &t.outcome {
            TurnOutcome::Finished { result } => {
                writeln!(out, "outcome: Finished: {result}")?;
            }
            TurnOutcome::TurnLimitExceeded => {
                writeln!(out, "outcome: TurnLimitExceeded")?;
            }
        }
    }
    Ok(())
}
