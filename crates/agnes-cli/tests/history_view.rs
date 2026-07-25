use agnes_cli::history_view::render_history;
use agnes_llm::{Iteration, Observation, Turn, TurnOutcome};
use agnes_types::TypeName;

fn iter(dsl: &str, obs: Vec<Observation>) -> Iteration {
    Iteration {
        assistant_dsl: dsl.into(),
        observations: obs,
        executed_dsl: None,
    }
}

fn obs(text: &str, is_error: bool, type_name: Option<&str>) -> Observation {
    Observation {
        text: text.into(),
        is_error,
        type_name: type_name.map(|s| TypeName(s.into())),
    }
}

#[test]
fn empty_history_prints_nothing() {
    let mut out = Vec::new();
    render_history(&[], &mut out).unwrap();
    assert_eq!(String::from_utf8(out).unwrap(), "");
}

#[test]
fn single_turn_single_iteration_finished_prints_expected() {
    let turns = vec![Turn {
        user_nl: "read notes".into(),
        iterations: vec![iter("(pipe \"notes.md\" (tool read-file) finish)", vec![])],
        outcome: TurnOutcome::Finished {
            result: "notes contents".into(),
        },
    }];
    let mut out = Vec::new();
    render_history(&turns, &mut out).unwrap();
    let s = String::from_utf8(out).unwrap();
    assert!(s.contains("--- turn 0 ---"));
    assert!(s.contains("user: read notes"));
    assert!(s.contains("iter 0: (pipe"));
    assert!(s.contains("outcome: Finished: notes contents"));
}

#[test]
fn multi_iteration_turn_shows_observations_between_dsls() {
    let turns = vec![Turn {
        user_nl: "translate this".into(),
        iterations: vec![
            iter(
                "(pipe (tool read-file \"x\") observe)",
                vec![obs("hello world", false, Some("String"))],
            ),
            iter("(pipe \"text\" (tool translate \"ja\") finish)", vec![]),
        ],
        outcome: TurnOutcome::Finished {
            result: "こんにちは".into(),
        },
    }];
    let mut out = Vec::new();
    render_history(&turns, &mut out).unwrap();
    let s = String::from_utf8(out).unwrap();
    // Two iterations, one observation.
    assert!(s.contains("iter 0:"));
    assert!(s.contains("iter 1:"));
    assert!(s.contains("obs[0] (String): hello world"));
    assert!(s.contains("outcome: Finished: こんにちは"));
}

#[test]
fn error_observations_are_flagged() {
    let turns = vec![Turn {
        user_nl: "boom".into(),
        iterations: vec![
            iter(
                "(pipe (tool bogus) observe)",
                vec![obs("parse: unknown tool bogus", true, None)],
            ),
            iter("(pipe \"ok\" finish)", vec![]),
        ],
        outcome: TurnOutcome::Finished {
            result: "ok".into(),
        },
    }];
    let mut out = Vec::new();
    render_history(&turns, &mut out).unwrap();
    let s = String::from_utf8(out).unwrap();
    assert!(s.contains("obs[0] (error): parse: unknown tool bogus"));
}

#[test]
fn turn_limit_exceeded_outcome_is_labelled() {
    let turns = vec![Turn {
        user_nl: "spinny".into(),
        iterations: vec![iter("(pipe x observe)", vec![obs("x", false, None)])],
        outcome: TurnOutcome::TurnLimitExceeded,
    }];
    let mut out = Vec::new();
    render_history(&turns, &mut out).unwrap();
    let s = String::from_utf8(out).unwrap();
    assert!(s.contains("outcome: TurnLimitExceeded"));
}

#[test]
fn multiple_observations_per_iteration_are_indexed() {
    let turns = vec![Turn {
        user_nl: "multi obs".into(),
        iterations: vec![iter(
            "(pipe (tool_observe foo) (fmap observe))",
            vec![
                obs("tool result", false, Some("Int")),
                obs("fmap result", false, Some("String")),
            ],
        )],
        outcome: TurnOutcome::TurnLimitExceeded,
    }];
    let mut out = Vec::new();
    render_history(&turns, &mut out).unwrap();
    let s = String::from_utf8(out).unwrap();
    assert!(s.contains("obs[0] (Int): tool result"));
    assert!(s.contains("obs[1] (String): fmap result"));
}
