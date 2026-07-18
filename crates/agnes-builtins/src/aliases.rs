use agnes_types::{TypeExpr, canonicalize_union};

pub fn text_like() -> TypeExpr {
    canonicalize_union([
        TypeExpr::named("PlainText"),
        TypeExpr::named("Markdown"),
        TypeExpr::named("HTML"),
    ])
}

pub fn visual_doc() -> TypeExpr {
    canonicalize_union([TypeExpr::named("PDF"), TypeExpr::named("Image")])
}
