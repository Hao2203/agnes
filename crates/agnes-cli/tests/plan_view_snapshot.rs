use agnes_session::PlanTree;

fn sample() -> PlanTree {
    PlanTree {
        kind: "pipe".into(),
        label: "pipe".into(),
        provides: Some("String".into()),
        children: vec![
            PlanTree {
                kind: "par".into(),
                label: "par".into(),
                provides: Some("Unit".into()),
                children: vec![PlanTree {
                    kind: "let".into(),
                    label: "let ja".into(),
                    provides: Some("String".into()),
                    children: vec![PlanTree {
                        kind: "pipe".into(),
                        label: "pipe".into(),
                        provides: Some("String".into()),
                        children: vec![
                            PlanTree {
                                kind: "tool".into(),
                                label: "tool read-file".into(),
                                provides: Some("String".into()),
                                children: vec![],
                            },
                            PlanTree {
                                kind: "tool".into(),
                                label: "tool translate".into(),
                                provides: Some("String".into()),
                                children: vec![],
                            },
                        ],
                    }],
                }],
            },
            PlanTree {
                kind: "tool".into(),
                label: "tool join-lines".into(),
                provides: Some("String".into()),
                children: vec![],
            },
        ],
    }
}

#[test]
fn render_plan_uses_indent_tree_glyphs() {
    let mut buf = Vec::new();
    agnes_cli::plan_view::render_plan(&sample(), &mut buf).unwrap();
    let out = String::from_utf8(buf).unwrap();
    // A handful of anchors — exact rendering is exercised by insta if enabled,
    // but this smoke check keeps things stable enough for TDD.
    assert!(out.contains("pipe"));
    assert!(out.contains("├── par"));
    assert!(out.contains("│   └── let ja"));
    assert!(out.contains("└── tool join-lines"));
    assert!(out.contains("→ String"));
}
