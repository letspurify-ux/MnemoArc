//! Copy-on-write retained data. Cloning a session shares immutable payloads;
//! only the container or value being edited is copied.
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    ops::{Deref, DerefMut},
    sync::{Arc, OnceLock},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(transparent)]
struct Payload<T> {
    value: T,
    #[serde(skip)]
    bytes: OnceLock<usize>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Shared<T>(Arc<Payload<T>>);

impl<T: PartialEq> PartialEq for Shared<T> {
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}
impl<T: Eq> Eq for Shared<T> {}

impl<T> From<T> for Shared<T> {
    fn from(value: T) -> Self {
        Self(Arc::new(Payload {
            value,
            bytes: OnceLock::new(),
        }))
    }
}

impl<T> From<Vec<T>> for Shared<VecDeque<T>> {
    fn from(value: Vec<T>) -> Self {
        VecDeque::from(value).into()
    }
}

impl<T: Default> Default for Shared<T> {
    fn default() -> Self {
        T::default().into()
    }
}

impl<A, T: FromIterator<A>> FromIterator<A> for Shared<T> {
    fn from_iter<I: IntoIterator<Item = A>>(items: I) -> Self {
        items.into_iter().collect::<T>().into()
    }
}

impl<T> Deref for Shared<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0.value
    }
}

impl<T: Clone> DerefMut for Shared<T> {
    fn deref_mut(&mut self) -> &mut T {
        let payload = Arc::make_mut(&mut self.0);
        payload.bytes.take();
        &mut payload.value
    }
}

impl<T: Serialize> Shared<T> {
    pub(crate) fn bytes(&self) -> usize {
        *self
            .0
            .bytes
            .get_or_init(|| crate::memory::serialized_bytes(&self.0.value))
    }
}

impl<'a, T> IntoIterator for &'a Shared<T>
where
    &'a T: IntoIterator,
{
    type Item = <&'a T as IntoIterator>::Item;
    type IntoIter = <&'a T as IntoIterator>::IntoIter;
    fn into_iter(self) -> Self::IntoIter {
        (&**self).into_iter()
    }
}

impl<'a, T: Clone> IntoIterator for &'a mut Shared<T>
where
    &'a mut T: IntoIterator,
{
    type Item = <&'a mut T as IntoIterator>::Item;
    type IntoIter = <&'a mut T as IntoIterator>::IntoIter;
    fn into_iter(self) -> Self::IntoIter {
        (&mut **self).into_iter()
    }
}

impl<T: Clone + IntoIterator> IntoIterator for Shared<T> {
    type Item = T::Item;
    type IntoIter = T::IntoIter;
    fn into_iter(self) -> Self::IntoIter {
        Arc::unwrap_or_clone(self.0).value.into_iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mutations_remain_private_with_fresh_counts() {
        let first: Shared<Vec<String>> = vec!["original content".into()].into();
        let bytes = first.bytes();
        let mut copy = first.clone();
        copy.push("new message".into());
        assert_eq!(first.len(), 1);
        assert_eq!(copy.len(), 2);
        assert_eq!(first.bytes(), bytes);
        assert_eq!(copy.bytes(), crate::memory::serialized_bytes(&*copy));
        assert_eq!(
            serde_json::to_value(&first).unwrap(),
            serde_json::to_value(&*first).unwrap()
        );
        let decoded: Shared<Vec<String>> =
            serde_json::from_value(serde_json::to_value(&copy).unwrap()).unwrap();
        assert_eq!(*decoded, *copy);
    }
}
