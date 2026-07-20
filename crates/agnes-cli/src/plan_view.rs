use agnes_session::PlanTree;
use std::io::{self, Write};

pub fn render_plan(tree: &PlanTree, out: &mut impl Write) -> io::Result<()> {
    render(tree, "", true, out, true)
}

fn render(
    node: &PlanTree,
    prefix: &str,
    is_last: bool,
    out: &mut impl Write,
    is_root: bool,
) -> io::Result<()> {
    if is_root {
        writeln!(out, "{}{}", node.label, provides_suffix(node))?;
    } else {
        let connector = if is_last { "└── " } else { "├── " };
        writeln!(
            out,
            "{prefix}{connector}{}{}",
            node.label,
            provides_suffix(node)
        )?;
    }
    let child_prefix = if is_root {
        String::new()
    } else if is_last {
        format!("{prefix}    ")
    } else {
        format!("{prefix}│   ")
    };
    let n = node.children.len();
    for (i, ch) in node.children.iter().enumerate() {
        render(ch, &child_prefix, i + 1 == n, out, false)?;
    }
    Ok(())
}

fn provides_suffix(node: &PlanTree) -> String {
    match &node.provides {
        Some(t)
            if node.kind == "tool"
                || node.kind == "llm"
                || node.kind == "let"
                || node.kind == "pipe"
                || node.kind == "par" =>
        {
            format!("  → {t}")
        }
        _ => String::new(),
    }
}
