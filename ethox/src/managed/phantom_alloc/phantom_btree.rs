//! An uninhabited type masquerading as `BTreeMap<_, _>`.
#![allow(unused, dead_code)]

use core::borrow::Borrow;
use core::marker::PhantomData;
use core::ptr::NonNull;

/// A phantom data type mimicking `BTreeMap`.
///
/// The interface provided is the same as the `BTreeMap` but it can not be instantiated. This makes
/// the implementation of most methods empty. The benefit of having this data type is that methods,
/// structs, and modules can be written without regards to the support for the `alloc`/`std`
/// feature so long as they never try to instantiate a `BTreeMap` themselves. Code that only works
/// with instances or references to them given to it from the outside **compiles** and the compiler
/// will be deduce that it can never execute.
///
/// This allows large portions of the library to be written independent from the configuration and
/// feature options without leading to actual code bloat. It is up to the eventual consumer of the
/// library to choose the `BTreeMap` provider and instantiate instances if it is possible.
///
/// ## Usage
///
/// ```
/// use ethox::alloc::collections::btree_map::BTreeMap;
///
/// /// A pure consumer of `BTreeMap`.
/// ///
/// /// If this type were not available this would have to be marked with
/// /// #[cfg(feature = "std")] 
/// /// otherwise it would not compile as `BTreeMap` would reference a non-existant type. But since
/// /// it does not instantiate one the phantom mimick allows us to still use its interface. The
/// /// compiler will (likely) realize no instance can be created and mark this as unreachable.
/// fn insert_a_val(map: &mut BTreeMap<u32, u32>) {
///     map.insert(0, 42);
/// }
/// ```
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BTreeMap<K: Ord, V> {
    // We pretend to own (K, V) pairs.
    elements: PhantomData<NonNull<(K, V)>>,
    data: Void,
}

#[allow(unused, dead_code)]
impl<K: Ord, V> BTreeMap<K, V> {
    pub fn clear(&mut self) { }

    pub fn get<Q>(&self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized, 
    { 
        match self.data { }
    }

    pub fn contains_key<Q>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized, 
    { 
        match self.data { }
    }

    pub fn get_mut<Q>(&self, key: &Q) -> Option<&mut V>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized, 
    { 
        match self.data { }
    }

    pub fn insert(&mut self, key: K, value: V) -> Option<V> {
        match self.data { }
    }
    
    pub fn remove<Q>(&mut self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized, 
    {
        match self.data { }
    }

    pub fn entry(&mut self, key: K) -> Entry<K, V> {
        match self.data { }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Void { }

pub enum Entry<'a, K, V> {
    Vacant(VacantEntry<'a, K, V>),
    Occupied(OccupiedEntry<'a, K, V>),
}

pub struct VacantEntry<'a, K, V> {
    phantom: PhantomData<&'a (K, V)>,
    void: Void,
}

pub struct OccupiedEntry<'a, K, V> {
    phantom: PhantomData<&'a (K, V)>,
    void: Void,
}

impl<K, V> OccupiedEntry<'_, K, V> {
    pub fn get(&self) -> &V {
        match self.void { }
    }

    pub fn get_mut(&mut self) -> &mut V {
        match self.void { }
    }

    pub fn remove(self) { }

    pub fn remove_entry(self) -> (K, V) {
        match self.void { }
    }
}

impl<'a, K, V> OccupiedEntry<'a, K, V> {
    pub fn into_mut(self) -> &'a mut V {
        match self.void { }
    }
}

impl<K, V> VacantEntry<'_, K, V> {
    pub fn key(&self) -> &K {
        match self.void { }
    }

    pub fn into_key(self) -> K {
        match self.void { }
    }
}

impl<'a, K, V> VacantEntry<'a, K, V> {
    pub fn insert(self, value: V) -> &'a mut V {
        match self.void { }
    }
}
