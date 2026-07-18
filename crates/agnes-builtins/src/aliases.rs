use agnes_types::{TypeExpr, TypeName};

pub fn text_like() -> TypeExpr {
    TypeExpr::Union(
        [
            TypeName("PlainText".into()),
            TypeName("Markdown".into()),
            TypeName("HTML".into()),
        ]
        .into_iter()
        .collect(),
    )
}

pub fn visual_doc() -> TypeExpr {
    TypeExpr::Union(
        [TypeName("PDF".into()), TypeName("Image".into())]
            .into_iter()
            .collect(),
    )
}
