//! [`Archive`] implementation for hashmaps.
//!
//! During archiving, hashmaps are built into minimal perfect hashmaps using
//! [compress, hash and displace](http://cmph.sourceforge.net/papers/esa09.pdf).

#[cfg(feature = "validation")]
pub mod validation;

use crate::{
    ser::Serializer,
    Archive,
    Archived,
    ArchivedUsize,
    FixedUsize,
    RawRelPtr,
    Serialize,
};
use core::{
    borrow::Borrow,
    cmp::Reverse,
    hash::{Hash, Hasher},
    iter::FusedIterator,
    marker::PhantomData,
    mem::{size_of, MaybeUninit},
    ops::Index,
    pin::Pin,
    ptr, slice,
};

#[cfg_attr(feature = "strict", repr(C))]
struct Entry<K, V> {
    key: K,
    value: V,
}

impl<K: Archive, V: Archive> Archive for Entry<&'_ K, &'_ V> {
    type Archived = Entry<K::Archived, V::Archived>;
    type Resolver = (K::Resolver, V::Resolver);

    #[inline]
    unsafe fn resolve(&self, pos: usize, resolver: Self::Resolver, out: &mut MaybeUninit<Self::Archived>) {
        let (fp, fo) = out_field!(out.key);
        self.key.resolve(pos + fp, resolver.0, fo);

        let (fp, fo) = out_field!(out.value);
        self.value.resolve(pos + fp, resolver.1, fo);
    }
}

/// An archived `HashMap`.
#[cfg_attr(feature = "strict", repr(C))]
pub struct ArchivedHashMap<K, V> {
    len: ArchivedUsize,
    displace: RawRelPtr,
    entries: RawRelPtr,
    _phantom: PhantomData<(K, V)>,
}

impl<K, V> ArchivedHashMap<K, V> {
    /// Gets the number of items in the hash map.
    #[inline]
    pub const fn len(&self) -> usize {
        from_archived!(self.len) as usize
    }

    fn make_hasher() -> seahash::SeaHasher {
        seahash::SeaHasher::with_seeds(
            0x08576fb6170b5f5f,
            0x587775eeb84a7e46,
            0xac701115428ee569,
            0x910feb91b92bb1cd,
        )
    }

    /// Gets the hasher for this hashmap. The hasher for all archived hashmaps is the same for
    /// reproducibility.
    #[inline]
    pub fn hasher(&self) -> seahash::SeaHasher {
        Self::make_hasher()
    }

    #[inline]
    unsafe fn displace(&self, index: usize) -> u32 {
        from_archived!(*self.displace.as_ptr().cast::<Archived<u32>>().add(index))
    }

    #[inline]
    unsafe fn entry(&self, index: usize) -> &Entry<K, V> {
        &*self.entries.as_ptr().cast::<Entry<K, V>>().add(index)
    }

    #[inline]
    unsafe fn entry_mut(&mut self, index: usize) -> &mut Entry<K, V> {
        &mut *self.entries.as_mut_ptr().cast::<Entry<K, V>>().add(index)
    }

    #[inline]
    fn index<Q: ?Sized>(&self, k: &Q) -> Option<usize>
    where
        K: Borrow<Q>,
        Q: Hash + Eq,
    {
        let mut hasher = self.hasher();
        k.hash(&mut hasher);
        let displace_index = hasher.finish() % self.len() as u64;
        let displace = unsafe { self.displace(displace_index as usize) };

        let index = if displace == u32::MAX {
            return None;
        } else if displace & 0x80_00_00_00 == 0 {
            displace as u64
        } else {
            let mut hasher = self.hasher();
            displace.hash(&mut hasher);
            k.hash(&mut hasher);
            hasher.finish() % self.len() as u64
        };

        let entry = unsafe { self.entry(index as usize) };
        if entry.key.borrow() == k {
            Some(index as usize)
        } else {
            None
        }
    }

    /// Finds the key-value entry for a key.
    #[inline]
    pub fn get_key_value<Q: ?Sized>(&self, k: &Q) -> Option<(&K, &V)>
    where
        K: Borrow<Q>,
        Q: Hash + Eq,
    {
        self.index(k).map(move |index| {
            let entry = unsafe { self.entry(index) };
            (&entry.key, &entry.value)
        })
    }

    /// Finds the mutable key-value entry for a key.
    #[inline]
    pub fn get_key_value_pin<Q: ?Sized>(self: Pin<&mut Self>, k: &Q) -> Option<(&K, Pin<&mut V>)>
    where
        K: Borrow<Q>,
        Q: Hash + Eq,
    {
        unsafe {
            let hash_map = self.get_unchecked_mut();
            hash_map.index(k).map(move |index| {
                let entry = hash_map.entry_mut(index);
                (&entry.key, Pin::new_unchecked(&mut entry.value))
            })
        }
    }

    /// Returns whether a key is present in the hash map.
    #[inline]
    pub fn contains_key<Q: ?Sized>(&self, k: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: Hash + Eq,
    {
        self.index(k).is_some()
    }

    /// Gets the value associated with the given key.
    #[inline]
    pub fn get<Q: ?Sized>(&self, k: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq,
    {
        self.index(k)
            .map(|index| unsafe { &self.entry(index).value })
    }

    /// Gets the mutable value associated with the given key.
    #[inline]
    pub fn get_pin<Q: ?Sized>(self: Pin<&mut Self>, k: &Q) -> Option<Pin<&mut V>>
    where
        K: Borrow<Q>,
        Q: Hash + Eq,
    {
        unsafe {
            let hash_map = self.get_unchecked_mut();
            hash_map
                .index(k)
                .map(move |index| Pin::new_unchecked(&mut hash_map.entry_mut(index).value))
        }
    }

    /// Returns whether there are no items in the hashmap.
    #[inline]
    pub const fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[inline]
    fn raw_iter(&self) -> RawIter<'_, K, V> {
        RawIter::new(self.entries.as_ptr().cast(), self.len())
    }

    #[inline]
    fn raw_iter_pin(self: Pin<&mut Self>) -> RawIterPin<'_, K, V> {
        unsafe {
            let hash_map = self.get_unchecked_mut();
            RawIterPin::new(hash_map.entries.as_mut_ptr().cast(), hash_map.len())
        }
    }

    /// Gets an iterator over the key-value entries in the hash map.
    #[inline]
    pub fn iter(&self) -> Iter<'_, K, V> {
        Iter {
            inner: self.raw_iter(),
        }
    }

    /// Gets an iterator over the mutable key-value entries in the hash map.
    #[inline]
    pub fn iter_pin(self: Pin<&mut Self>) -> IterPin<'_, K, V> {
        IterPin {
            inner: self.raw_iter_pin(),
        }
    }

    /// Gets an iterator over the keys in the hash map.
    #[inline]
    pub fn keys(&self) -> Keys<'_, K, V> {
        Keys {
            inner: self.raw_iter(),
        }
    }

    /// Gets an iterator over the values in the hash map.
    #[inline]
    pub fn values(&self) -> Values<K, V> {
        Values {
            inner: self.raw_iter(),
        }
    }

    /// Gets an iterator over the mutable values in the hash map.
    #[inline]
    pub fn values_pin(self: Pin<&mut Self>) -> ValuesPin<'_, K, V> {
        ValuesPin {
            inner: self.raw_iter_pin(),
        }
    }

    /// Serializes an iterator of key-value pairs as a hash map.
    ///
    /// # Safety
    ///
    /// - Keys returned by the iterator must be unique
    /// - `len` must be the number of elements yielded by `iter`
    pub unsafe fn serialize_from_iter<
        'a,
        KU: 'a + Serialize<S, Archived = K> + Hash + Eq,
        VU: 'a + Serialize<S, Archived = V>,
        S: Serializer + ?Sized,
    >(
        iter: impl Iterator<Item = (&'a KU, &'a VU)>,
        len: usize,
        serializer: &mut S,
    ) -> Result<HashMapResolver, S::Error> {
        let mut bucket_size = vec![0u32; len];
        let mut displaces = Vec::with_capacity(len);

        for (key, value) in iter {
            let mut hasher = Self::make_hasher();
            key.hash(&mut hasher);
            let displace = (hasher.finish() % len as u64) as u32;
            displaces.push((displace, (key, value)));
            bucket_size[displace as usize] += 1;
        }

        displaces.sort_by_key(|&(displace, _)| (Reverse(bucket_size[displace as usize]), displace));

        let mut entries = Vec::with_capacity(len);
        entries.resize_with(len, || None);
        let mut displacements = vec![to_archived!(u32::MAX); len];

        let mut first_empty = 0;
        let mut assignments = Vec::with_capacity(8);

        let mut start = 0;
        while start < displaces.len() {
            let displace = displaces[start].0;
            let bucket_size = bucket_size[displace as usize] as usize;
            let end = start + bucket_size;
            let bucket = &displaces[start..end];
            start = end;

            if bucket_size > 1 {
                'find_seed: for seed in 0x80_00_00_00u32..=0xFF_FF_FF_FFu32 {
                    let mut base_hasher = Self::make_hasher();
                    seed.hash(&mut base_hasher);

                    assignments.clear();

                    for &(_, (key, _)) in bucket.iter() {
                        let mut hasher = base_hasher;
                        key.hash(&mut hasher);
                        let index = (hasher.finish() % len as u64) as u32;
                        if entries[index as usize].is_some() || assignments.contains(&index) {
                            continue 'find_seed;
                        } else {
                            assignments.push(index);
                        }
                    }

                    for i in 0..bucket_size {
                        entries[assignments[i] as usize] = Some(bucket[i].1);
                    }
                    displacements[displace as usize] = to_archived!(seed);
                    break;
                }
            } else {
                let offset = entries[first_empty..]
                    .iter()
                    .position(|value| value.is_none())
                    .unwrap();
                first_empty += offset;
                entries[first_empty] = Some(bucket[0].1);
                displacements[displace as usize] = to_archived!(first_empty as u32);
                first_empty += 1;
            }
        }

        // Archive entries
        let mut resolvers = entries
            .iter()
            .map(|e| {
                let (key, value) = e.unwrap();
                Ok((key.serialize(serializer)?, value.serialize(serializer)?))
            })
            .collect::<Result<Vec<_>, _>>()?;

        // Write blocks
        let displace_pos = serializer.align_for::<u32>()?;
        let displacements_slice = slice::from_raw_parts(
            displacements.as_ptr().cast::<u8>(),
            displacements.len() * size_of::<u32>(),
        );
        serializer.write(displacements_slice)?;

        let entries_pos = serializer.align_for::<Entry<K, V>>()?;
        for ((key, value), (key_resolver, value_resolver)) in
            entries.iter().map(|r| r.unwrap()).zip(resolvers.drain(..))
        {
            serializer
                .resolve_aligned(&Entry { key, value }, (key_resolver, value_resolver))?;
        }

        Ok(HashMapResolver {
            displace_pos,
            entries_pos,
        })
    }

    /// Resolves the archived hash map from a given `len`.
    ///
    /// # Safety
    ///
    /// - `len` must be the number of elements that were serialized
    /// - `pos` must be the position of `out` within the archive
    /// - `resolver` must be the result of serializing a hash map
    #[inline]
    pub unsafe fn resolve_from_len(
        len: usize,
        pos: usize,
        resolver: HashMapResolver,
        out: &mut MaybeUninit<Self>,
    ) {
        ptr::addr_of_mut!((*out.as_mut_ptr()).len).write(to_archived!(len as FixedUsize));

        let (fp, fo) = out_field!(out.displace);
        RawRelPtr::emplace(pos + fp, resolver.displace_pos, fo);

        let (fp, fo) = out_field!(out.entries);
        RawRelPtr::emplace(pos + fp, resolver.entries_pos, fo);
    }
}

struct RawIter<'a, K, V> {
    current: *const Entry<K, V>,
    remaining: usize,
    _phantom: PhantomData<(&'a K, &'a V)>,
}

impl<'a, K, V> RawIter<'a, K, V> {
    #[inline]
    fn new(pairs: *const Entry<K, V>, len: usize) -> Self {
        Self {
            current: pairs,
            remaining: len,
            _phantom: PhantomData,
        }
    }
}

impl<'a, K, V> Iterator for RawIter<'a, K, V> {
    type Item = *const Entry<K, V>;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        unsafe {
            if self.remaining == 0 {
                None
            } else {
                let result = self.current;
                self.current = self.current.add(1);
                self.remaining -= 1;
                Some(result)
            }
        }
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl<'a, K, V> ExactSizeIterator for RawIter<'a, K, V> {}
impl<'a, K, V> FusedIterator for RawIter<'a, K, V> {}

struct RawIterPin<'a, K, V> {
    current: *mut Entry<K, V>,
    remaining: usize,
    _phantom: PhantomData<(&'a K, Pin<&'a mut V>)>,
}

impl<'a, K, V> RawIterPin<'a, K, V> {
    #[inline]
    fn new(pairs: *mut Entry<K, V>, len: usize) -> Self {
        Self {
            current: pairs,
            remaining: len,
            _phantom: PhantomData,
        }
    }
}

impl<'a, K, V> Iterator for RawIterPin<'a, K, V> {
    type Item = *mut Entry<K, V>;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        unsafe {
            if self.remaining == 0 {
                None
            } else {
                let result = self.current;
                self.current = self.current.add(1);
                self.remaining -= 1;
                Some(result)
            }
        }
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl<K, V> ExactSizeIterator for RawIterPin<'_, K, V> {}
impl<K, V> FusedIterator for RawIterPin<'_, K, V> {}

/// An iterator over the key-value pairs of a hash map.
#[repr(transparent)]
pub struct Iter<'a, K, V> {
    inner: RawIter<'a, K, V>,
}

impl<'a, K, V> Iterator for Iter<'a, K, V> {
    type Item = (&'a K, &'a V);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|x| unsafe {
            let pair = &*x;
            (&pair.key, &pair.value)
        })
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl<K, V> ExactSizeIterator for Iter<'_, K, V> {}
impl<K, V> FusedIterator for Iter<'_, K, V> {}

/// An iterator over the mutable key-value pairs of a hash map.
#[repr(transparent)]
pub struct IterPin<'a, K, V> {
    inner: RawIterPin<'a, K, V>,
}

impl<'a, K, V> Iterator for IterPin<'a, K, V> {
    type Item = (&'a K, Pin<&'a mut V>);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|x| unsafe {
            let pair = &mut *x;
            (&pair.key, Pin::new_unchecked(&mut pair.value))
        })
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl<K, V> ExactSizeIterator for IterPin<'_, K, V> {}
impl<K, V> FusedIterator for IterPin<'_, K, V> {}

/// An iterator over the keys of a hash map.
#[repr(transparent)]
pub struct Keys<'a, K, V> {
    inner: RawIter<'a, K, V>,
}

impl<'a, K, V> Iterator for Keys<'a, K, V> {
    type Item = &'a K;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|x| unsafe {
            let pair = &*x;
            &pair.key
        })
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl<K, V> ExactSizeIterator for Keys<'_, K, V> {}
impl<K, V> FusedIterator for Keys<'_, K, V> {}

/// An iterator over the values of a hash map.
#[repr(transparent)]
pub struct Values<'a, K, V> {
    inner: RawIter<'a, K, V>,
}

impl<'a, K, V> Iterator for Values<'a, K, V> {
    type Item = &'a V;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|x| unsafe {
            let pair = &*x;
            &pair.value
        })
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl<K, V> ExactSizeIterator for Values<'_, K, V> {}
impl<K, V> FusedIterator for Values<'_, K, V> {}

/// An iterator over the mutable values of a hash map.
#[repr(transparent)]
pub struct ValuesPin<'a, K, V> {
    inner: RawIterPin<'a, K, V>,
}

impl<'a, K, V> Iterator for ValuesPin<'a, K, V> {
    type Item = Pin<&'a mut V>;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|x| unsafe {
            let pair = &mut *x;
            Pin::new_unchecked(&mut pair.value)
        })
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl<K, V> ExactSizeIterator for ValuesPin<'_, K, V> {}
impl<K, V> FusedIterator for ValuesPin<'_, K, V> {}

/// The resolver for archived hash maps.
pub struct HashMapResolver {
    displace_pos: usize,
    entries_pos: usize,
}

impl<K: Hash + Eq, V: PartialEq> PartialEq for ArchivedHashMap<K, V> {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        if self.len() != other.len() {
            false
        } else {
            self.iter()
                .all(|(key, value)| other.get(key).map_or(false, |v| *value == *v))
        }
    }
}

impl<K: Hash + Eq, V: Eq> Eq for ArchivedHashMap<K, V> {}

impl<K: Eq + Hash + Borrow<Q>, Q: Eq + Hash + ?Sized, V> Index<&'_ Q> for ArchivedHashMap<K, V> {
    type Output = V;

    #[inline]
    fn index(&self, key: &Q) -> &V {
        self.get(key).unwrap()
    }
}