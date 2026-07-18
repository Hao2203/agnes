use agnes_types::TypeName;
use std::collections::HashMap;

/// Type environment threaded through expression checking.
#[derive(Debug, Default, Clone)]
pub struct Env {
    inner: HashMap<String, TypeName>,
}

impl Env {
    pub fn get(&self, name: &str) -> Option<&TypeName> {
        self.inner.get(name)
    }
    pub fn set(&mut self, name: String, ty: TypeName) {
        self.inner.insert(name, ty);
    }
}
