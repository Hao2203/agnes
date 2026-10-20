//! A collection guaranteed to hold at least one value.

use crate::EmptyError;
use std::fmt;
use std::sync::Arc;

/// A non-empty, immutable collection. Cloning is cheap — it only bumps a
/// reference count — and read access is available through `AsRef<[T]>`.
#[derive(Clone, PartialEq, Eq)]
pub struct NonEmpty<T> {
    inner: Arc<[T]>,
}

impl<T> NonEmpty<T> {
    /// Wraps a single value; infallible.
    pub fn new(first: T) -> Self {
        Self {
            inner: Arc::from([first]),
        }
    }

    /// Collects an iterator, rejecting an empty one. Fallible counterpart of
    /// the standard `FromIterator` trait, whose signature cannot fail.
    ///
    /// Collected straight into `Arc<[T]>`, which uses std's exact-size fast
    /// path: iterators with a known length land in a single allocation.
    /// When a `Vec` already exists, prefer [`NonEmpty::from_vec`].
    #[allow(clippy::should_implement_trait)]
    pub fn from_iter<I>(iter: I) -> Result<Self, EmptyError>
    where
        I: IntoIterator<Item = T>,
    {
        let inner: Arc<[T]> = iter.into_iter().collect();
        if inner.is_empty() {
            return Err(EmptyError);
        }
        Ok(Self { inner })
    }

    /// Wraps an existing vector, rejecting the empty one. No re-collection:
    /// the elements are moved into the shared allocation as-is.
    pub fn from_vec(items: Vec<T>) -> Result<Self, EmptyError> {
        if items.is_empty() {
            return Err(EmptyError);
        }
        Ok(Self {
            inner: Arc::from(items),
        })
    }

    pub fn first(&self) -> &T {
        &self.inner[0]
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Always false — part of the slice-like API, kept for lint symmetry.
    pub fn is_empty(&self) -> bool {
        false
    }

    pub fn iter(&self) -> std::slice::Iter<'_, T> {
        self.inner.iter()
    }

    /// Deep-copies the contents into a vector.
    pub fn to_vec(&self) -> Vec<T>
    where
        T: Clone,
    {
        self.inner.to_vec()
    }
}

impl<T> AsRef<[T]> for NonEmpty<T> {
    fn as_ref(&self) -> &[T] {
        &self.inner
    }
}

impl<T: fmt::Debug> fmt::Debug for NonEmpty<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.inner.iter()).finish()
    }
}

impl<T> From<T> for NonEmpty<T> {
    fn from(first: T) -> Self {
        Self::new(first)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_iter_rejects_empty() {
        assert!(NonEmpty::from_iter(Vec::<u8>::new()).is_err());
        assert!(NonEmpty::from_iter(std::iter::empty::<u8>()).is_err());
    }

    #[test]
    fn from_vec_rejects_empty_and_moves_buffer() {
        assert!(NonEmpty::from_vec(Vec::<u8>::new()).is_err());
        let two = NonEmpty::from_vec(vec![1, 2]).unwrap();
        assert_eq!(two.as_ref(), &[1, 2]);
    }

    #[test]
    fn from_iter_accepts_iterators() {
        let two = NonEmpty::from_iter([1, 2]).unwrap();
        assert_eq!(two.len(), 2);
        assert_eq!(two.as_ref(), &[1, 2]);

        let mapped = NonEmpty::from_iter((1..=3).map(|n| n * 2)).unwrap();
        assert_eq!(mapped.to_vec(), vec![2, 4, 6]);
    }

    #[test]
    fn single_value_is_infallible() {
        let one: NonEmpty<u8> = NonEmpty::new(7);
        assert_eq!(one.first(), &7);
        assert_eq!(one.as_ref(), &[7]);
        assert!(!one.is_empty());
    }

    #[test]
    fn clone_is_shared_not_deep() {
        let original = NonEmpty::from_iter(["a".to_owned(), "b".to_owned()]).unwrap();
        let clone = original.clone();
        // Both views point at the same allocation.
        assert_eq!(original.as_ref().as_ptr(), clone.as_ref().as_ptr());
    }
}
