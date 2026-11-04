use std::any::TypeId;
use std::fmt;

/// The namespace an access belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccessKind {
    Component,
    Resource,
    Message,
}

impl AccessKind {
    fn label(self) -> &'static str {
        match self {
            Self::Component => "component",
            Self::Resource => "resource",
            Self::Message => "message",
        }
    }
}

/// Whether a parameter reads or writes the accessed value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccessMode {
    Read,
    Write,
}

impl AccessMode {
    fn label(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
        }
    }
}

/// One declared access of a system parameter.
#[derive(Clone, Copy)]
pub struct Access {
    kind: AccessKind,
    id: TypeId,
    name: &'static str,
    mode: AccessMode,
}

impl Access {
    pub(crate) fn of<T: 'static>(kind: AccessKind, mode: AccessMode) -> Self {
        Self {
            kind,
            id: TypeId::of::<T>(),
            name: std::any::type_name::<T>(),
            mode,
        }
    }

    pub(crate) fn component<T: 'static>(mode: AccessMode) -> Self {
        Self::of::<T>(AccessKind::Component, mode)
    }

    pub(crate) fn resource<T: 'static>(mode: AccessMode) -> Self {
        Self::of::<T>(AccessKind::Resource, mode)
    }

    pub(crate) fn message<T: 'static>(mode: AccessMode) -> Self {
        Self::of::<T>(AccessKind::Message, mode)
    }

    /// The accessed namespace.
    pub fn kind(self) -> AccessKind {
        self.kind
    }

    /// The access mode.
    pub fn mode(self) -> AccessMode {
        self.mode
    }

    /// The fully qualified type name of the accessed value.
    pub fn name(self) -> &'static str {
        self.name
    }

    fn conflicts_with(self, other: Self) -> bool {
        self.kind == other.kind
            && self.id == other.id
            && (self.mode == AccessMode::Write || other.mode == AccessMode::Write)
    }
}

impl fmt::Debug for Access {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

impl fmt::Display for Access {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} `{}` ({})",
            self.kind.label(),
            self.name,
            self.mode.label()
        )
    }
}

/// The set of values a system parameter touches.
#[derive(Clone, Debug, Default)]
pub struct AccessSet {
    entries: Vec<Access>,
}

impl AccessSet {
    /// Creates an empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the set declares no accesses.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub(crate) fn push(&mut self, access: Access) {
        self.entries.push(access);
    }

    pub(crate) fn extend(&mut self, other: Self) {
        self.entries.extend(other.entries);
    }

    pub(crate) fn merge(&mut self, other: &Self) {
        self.entries.extend_from_slice(&other.entries);
    }

    /// Returns the first pair of conflicting accesses, if any.
    ///
    /// Two accesses conflict when they target the same value in the same
    /// namespace and at least one of them is a write.
    pub fn conflicts(&self) -> Option<(Access, Access)> {
        for (index, first) in self.entries.iter().enumerate() {
            for second in &self.entries[index + 1..] {
                if first.conflicts_with(*second) {
                    return Some((*first, *second));
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{Access, AccessMode, AccessSet};

    #[test]
    fn reads_do_not_conflict() {
        let mut set = AccessSet::new();
        set.push(Access::component::<u32>(AccessMode::Read));
        set.push(Access::component::<u32>(AccessMode::Read));
        assert!(set.conflicts().is_none());
    }

    #[test]
    fn read_and_write_conflict() {
        let mut set = AccessSet::new();
        set.push(Access::component::<u32>(AccessMode::Read));
        set.push(Access::component::<u32>(AccessMode::Write));
        assert!(set.conflicts().is_some());
    }

    #[test]
    fn different_namespaces_do_not_conflict() {
        let mut set = AccessSet::new();
        set.push(Access::component::<u32>(AccessMode::Write));
        set.push(Access::resource::<u32>(AccessMode::Write));
        assert!(set.conflicts().is_none());
    }
}
