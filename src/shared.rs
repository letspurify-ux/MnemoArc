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

/// A map entry's share of the map's JSON: its key, the colon and its value.
fn entry_bytes(key: &str, value: &impl Serialize) -> usize {
    crate::memory::serialized_bytes(key)
        .saturating_add(1)
        .saturating_add(crate::memory::serialized_bytes(value))
}

impl<V: Serialize + Clone> Shared<std::collections::BTreeMap<String, V>> {
    /// Insert, keeping a known serialized size by the entry's own: after a
    /// plain insert the next capacity check serialized the whole map again,
    /// a long session's every source excerpt on each new read.
    pub(crate) fn insert_counted(&mut self, key: String, value: V) -> Option<V> {
        let known = self.0.bytes.get().copied();
        let added = entry_bytes(&key, &value);
        let payload = Arc::make_mut(&mut self.0);
        payload.bytes.take();
        let first = payload.value.is_empty();
        let replaced_key = known.map(|_| crate::memory::serialized_bytes(key.as_str()));
        let old = payload.value.insert(key, value);
        let total = known.and_then(|total| {
            let total = match &old {
                Some(old) => total.checked_sub(
                    replaced_key?
                        .saturating_add(1)
                        .saturating_add(crate::memory::serialized_bytes(old)),
                )?,
                None => total.saturating_add(usize::from(!first)),
            };
            Some(total.saturating_add(added))
        });
        if let Some(total) = total.filter(|&total| total < usize::MAX) {
            let _ = payload.bytes.set(total);
        }
        old
    }

    /// Remove, keeping a known serialized size (see insert_counted).
    pub(crate) fn remove_counted(&mut self, key: &str) -> Option<V> {
        if !self.contains_key(key) {
            return None;
        }
        let known = self.0.bytes.get().copied();
        let payload = Arc::make_mut(&mut self.0);
        payload.bytes.take();
        let old = payload.value.remove(key)?;
        let total = known.and_then(|total| {
            total
                .checked_sub(entry_bytes(key, &old))?
                .checked_sub(usize::from(!payload.value.is_empty()))
        });
        if let Some(total) = total {
            let _ = payload.bytes.set(total);
        }
        Some(old)
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

    // A counted insert or removal keeps the map's known size, equal to
    // serializing it whole, so the next capacity check need not do that.
    #[test]
    fn counted_inserts_and_removals_keep_the_size_without_serializing() {
        let mut map: Shared<std::collections::BTreeMap<String, Arc<String>>> = Shared::default();
        let whole = |map: &Shared<std::collections::BTreeMap<String, Arc<String>>>| {
            crate::memory::serialized_bytes(&**map)
        };
        assert_eq!(map.bytes(), 2);
        let copy = map.clone();
        let steps: &[(&str, Option<&str>)] = &[
            ("b", Some("first \"quoted\" 한글")),
            ("a", Some("x")),
            ("c\n", Some("")),
            ("a", Some("replaced with a longer value")),
            ("b", None),
            ("missing", None),
            ("a", None),
            ("c\n", None),
            ("d", Some("again")),
        ];
        for &(key, value) in steps {
            match value {
                Some(value) => {
                    map.insert_counted(key.into(), Arc::new(value.into()));
                }
                None => {
                    map.remove_counted(key);
                }
            }
            assert_eq!(map.0.bytes.get().copied(), Some(whole(&map)), "{key:?}");
        }
        assert!(copy.is_empty());
        assert_eq!(copy.bytes(), 2);
    }

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
