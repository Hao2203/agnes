use agnes_types::TypeExpr;
use std::collections::HashMap;

/// Type environment threaded through expression checking.
#[derive(Debug, Default, Clone)]
pub struct Env {
    inner: HashMap<String, TypeExpr>,
}

impl Env {
    pub fn get(&self, name: &str) -> Option<&TypeExpr> {
        self.inner.get(name)
    }
    pub fn set(&mut self, name: String, ty: TypeExpr) {
        self.inner.insert(name, ty);
    }
}
