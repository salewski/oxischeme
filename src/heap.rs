// Copyright 2014 Nick Fitzgerald
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! The `heap` module provides memory management for our Scheme implementation.
//!
//! ## Allocation
//!
//! Scheme has a variety of types that must be allocated on the heap: cons cells,
//! strings, procedures, and vectors (currently unimplemented).
//!
//! Oxischeme does not allocate each individual object directly from the OS,
//! which would have unnecessary bookkeeping overhead. Instead, we allocate
//! objects from an "arena" which contains a pre-allocated object pool reserved
//! for allocating future objects from. We keep track of an arena's un-used
//! objects with a "free list" of indices into this pool. When we allocate a new
//! object, we remove the first entry from this list and return a pointer to the
//! object at that entry's index in the object pool. Garbage collection adds new
//! entries to the free list when reclaiming dead objects. When allocating, if
//! our existing arenas' pools are already at capacity (ie, all of their free
//! lists are empty), then we allocate another arena from the OS, add it to our
//! set of arenas, and allocate from its object pool. During garbage collection,
//! if an arena is empty, its memory is returned to the OS.
//!
//! ## Garbage Collection
//!
//! Any type that is heap-allocated must be *garbage collected* so that the
//! memory of no-longer used instances of that type can be reclaimed for
//! reuse. This provides the illusion of infinite memory, and frees Scheme
//! programmers from manually managing allocations and frees. We refer to
//! GC-managed types as "GC things". Note that a GC thing does not need to be a
//! Scheme value type: activations are also managed by the GC, but are not a
//! first class Scheme value.
//!
//! Any structure that has references to a garbage collected type must
//! *participate in garbage collection* by telling the garbage collector about
//! all of the GC things it is holding alive. Participation is implemented via
//! the `Trace` trait. Note that the set of types that participate in garbage
//! collection is not the same as the set of all GC things. Some GC things do not
//! participate in garbage collection: strings do not hold references to any
//! other GC things.
//!
//! A "GC root" is a GC participant that is always reachable. For example, the
//! global activation is a root because global variables must always be
//! accessible.
//!
//! We use a simple *mark and sweep* garbage collection algorithm. In the mark
//! phase, we start from the roots and recursively trace every reachable object
//! in the heap graph, adding them to our "marked" set. If a GC thing is not
//! reachable, then it is impossible for the Scheme program to use it in the
//! future, and it is safe for the garbage collector to reclaim it. The
//! unreachable objects are the set of GC things that are not in the marked
//! set. We find these unreachable objects and return them to their respective
//! arena's free list in the sweep phase.
//!
//! ### Rooting
//!
//! Sometimes it is necessary to temporarily root GC things referenced by
//! pointers on the stack. Garbage collection can be triggered by allocating any
//! GC thing, and it isn't always clear which rust functions (or other functions
//! called by those functions, or even other functions called by those functions
//! called from the first function, and so on) might allocate a GC thing and
//! trigger collection. The situation we want to avoid is a rust function using a
//! temporary variable that references a GC thing, then calling another function
//! which triggers a collection and collects the GC thing that was referred to by
//! the temporary variable, and the temporary variable is now a dangling
//! pointer. If the rust function accesses it again, that is undefined behavior:
//! it might still get the value it was pointing at, or it might be a segfault,
//! or it might be a freshly allocated value used by something else! Not good!
//!
//! Here is what this scenario looks like in psuedo code:
//!
//!     let a = pointer_to_some_gc_thing;
//!     function_which_can_trigger_gc();
//!     // Oops! A collection was triggered and dereferencing this pointer leads
//!     // to undefined behavior!
//!     *a;
//!
//! There are two possible solutions to this problem. The first is *conservative*
//! garbage collection, where we walk the stack and if anything on the stack
//! looks like it might be a pointer and if coerced to a pointer happens to point
//! to a GC thing in the heap, we assume that it *is* a pointer and we consider
//! the GC thing that may or may not actually be pointed to by a variable on the
//! stack a GC root. The second is *precise rooting*. With precise rooting, it is
//! the responsibility of the rust function's author to explicitly root and
//! unroot pointers to GC things used in variables on the stack.
//!
//! Oxischeme uses precise rooting. Precise rooting is implemented with the
//! `Rooted<GcThingPtr>` smart pointer type, which roots its referent upon
//! construction and unroots it when the smart pointer goes out of scope and is
//! dropped.
//!
//! Using precise rooting and `Rooted`, we can solve the dangling pointer
//! problem like this:
//!
//!     {
//!         // The pointed to GC thing gets rooted when wrapped with `Rooted`.
//!         let a = Rooted::new(heap, pointer_to_some_gc_thing);
//!         function_which_can_trigger_gc();
//!         // Dereferencing `a` is now safe, because the referent is a GC root!
//!         *a;
//!     }
//!     // `a` goes out of scope, and its referent is unrooted.
//!
//! Tips for working with precise rooting if your function allocates GC things,
//! or calls other functions which allocate GC things:
//!
//! * Accept GC thing parameters as `&Rooted<T>` or `&mut Rooted<T>` to ensure
//!   that callers properly root them.
//!
//! * Accept a `&mut Heap` parameter and return `Rooted<T>` for getters and
//!   methods that return GC things. This greatly alleviates potential
//!   foot-guns, as a caller would have to explicitly unwrap the smart pointer
//!   and store that in a new variable to cause a dangling pointer. It also
//!   cuts down on `Rooted<T>` construction boiler plate.
//!
//! * Always root GC things whose lifetime spans a call which could trigger a
//!   collection!
//!
//! * When in doubt, Just Root It!

use std::cmp;
use std::collections::{BitVec, HashMap};
use std::default::{Default};
use std::fmt;
use std::ops::{Deref, DerefMut};
use std::vec::{IntoIter};

use environment::{Activation, ActivationPtr, RootedActivationPtr, Environment};
use primitives::{define_primitives};
use read::{Location};
use value::{Cons, ConsPtr, Procedure, ProcedurePtr, RootedConsPtr,
            RootedProcedurePtr, RootedValue, Value};

/// We use a vector for our implementation of a free list. `Vector::push` to add
/// new entries, `Vector::pop` to remove the next entry when we allocate.
type FreeList = Vec<usize>;

/// An arena from which to allocate `T` objects from.
pub struct Arena<T> {
    pool: Vec<T>,

    /// The set of free indices into `pool` that are available for allocating an
    /// object from.
    free: FreeList,

    /// During a GC, if the nth bit of `marked` is set, that means that the nth
    /// object in `pool` has been marked as reachable.
    marked: BitVec,
}

impl<T: Default> Arena<T> {
    /// Create a new `Arena` with the capacity to allocate the given number of
    /// `T` instances.
    pub fn new(capacity: usize) -> Box<Arena<T>> {
        assert!(capacity > 0);
        Box::new(Arena {
            pool: range(0, capacity).map(|_| Default::default()).collect(),
            free: range(0, capacity).collect(),
            marked: BitVec::from_elem(capacity, false),
        })
    }

    /// Get this heap's capacity for simultaneously allocated cons cells.
    pub fn capacity(&self) -> usize {
        self.pool.len()
    }

    /// Return true if this arena is at full capacity, and false otherwise.
    pub fn is_full(&self) -> bool {
        self.free.is_empty()
    }

    /// Return true if this arena does not contain any reachable objects (ie,
    /// the free list is full), and false otherwise.
    pub fn is_empty(&self) -> bool {
        self.free.len() == self.capacity()
    }

    /// Allocate a new `T` instance and return a pointer to it.
    ///
    /// ## Panics
    ///
    /// Panics when this arena's pool is already at capacity.
    pub fn allocate(&mut self) -> ArenaPtr<T> {
        match self.free.pop() {
            Some(idx) => {
                let self_ptr : *mut Arena<T> = self;
                ArenaPtr::new(self_ptr, idx)
            },
            None => panic!("Arena is at capacity!"),
        }
    }

    /// Sweep the arena and add any reclaimed objects back to the free list.
    pub fn sweep(&mut self) {
        self.free = range(0, self.capacity())
            .filter(|&n| {
                !self.marked.get(n)
                    .expect("`marked` should always have length == self.capacity()")
            })
            .collect();

        // Reset `marked` to all zero.
        self.marked.set_all();
        self.marked.negate();
    }
}

/// A set of `Arena`s. Manages allocating and deallocating additional `Arena`s
/// from the OS, depending on the number of objects requested and kept alive by
/// the mutator.
pub struct ArenaSet<T> {
    capacity: usize,
    arenas: Vec<Box<Arena<T>>>,
}

impl<T: Default> ArenaSet<T> {
    /// Create a new `ArenaSet`.
    pub fn new(capacity: usize) -> ArenaSet<T> {
        ArenaSet {
            capacity: capacity,
            arenas: vec!()
        }
    }

    /// Sweep all of the arenas in this set.
    pub fn sweep(&mut self) {
        for arena in self.arenas.iter_mut() {
            arena.sweep();
        }

        // Deallocate any arenas that do not contain any reachable objects.
        self.arenas.retain(|a| !a.is_empty());
    }

    /// Allocate a `T` object from one of the arenas in this set and return a
    /// pointer to it.
    pub fn allocate(&mut self) -> ArenaPtr<T> {
        for arena in self.arenas.iter_mut() {
            if !arena.is_full() {
                return arena.allocate();
            }
        }

        // All existing arenas are at capacity, allocate a new one for this
        // requested object allocation, get the requested object from it, and add it to our
        // set.
        let mut new_arena = Arena::new(self.capacity);
        let result = new_arena.allocate();
        self.arenas.push(new_arena);
        result
    }
}

/// A pointer to a `T` instance in an arena.
#[allow(raw_pointer_derive)]
#[derive(Hash)]
pub struct ArenaPtr<T> {
    arena: *mut Arena<T>,
    index: usize,
}

// XXX: We have to manually declare that ArenaPtr<T> is copy-able because if we
// use `#[derive(Copy)]` it wants T to be copy-able as well, despite the fact
// that we only need to copy our pointer to the Arena<T>, not any T or the Arena
// itself.
impl<T> ::std::marker::Copy for ArenaPtr<T> { }

impl<T: Default> ArenaPtr<T> {
    /// Create a new `ArenaPtr` to the `T` instance at the given index in the
    /// provided arena. **Not** publicly exposed, and should only be called by
    /// `Arena::allocate`.
    fn new(arena: *mut Arena<T>, index: usize) -> ArenaPtr<T> {
        unsafe {
            let arena_ref = arena.as_ref()
                .expect("ArenaPtr<T>::new should be passed a valid Arena.");
            assert!(index < arena_ref.capacity());
        }
        ArenaPtr {
            arena: arena,
            index: index,
        }
    }

    /// During a GC, mark this `ArenaPtr` as reachable.
    fn mark(&self) {
        unsafe {
            let arena = self.arena.as_mut()
                .expect("An ArenaPtr<T> should always have a valid Arena.");
            arena.marked.set(self.index, true);
        }
    }

    /// During a GC, determine if this `ArenaPtr` has been marked as reachable.
    fn is_marked(&self) -> bool {
        unsafe {
            let arena = self.arena.as_mut()
                .expect("An ArenaPtr<T> should always have a valid Arena.");
            return arena.marked.get(self.index)
                .expect("self.index should always be within the marked bitv's length.");
        }
    }
}

impl<T> Deref for ArenaPtr<T> {
    type Target = T;
    fn deref<'a>(&'a self) -> &'a T {
        unsafe {
            let arena = self.arena.as_ref()
                .expect("ArenaPtr::deref should always have an Arena.");
            &arena.pool[self.index]
        }
    }
}

impl<T> DerefMut for ArenaPtr<T> {
    fn deref_mut<'a>(&'a mut self) -> &'a mut T {
        unsafe {
            let arena = self.arena.as_mut()
                .expect("ArenaPtr::deref_mut should always have an Arena.");
            &mut arena.pool[self.index]
        }
    }
}

impl<T> fmt::Debug for ArenaPtr<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "ArenaPtr({:p}, {})", &self.arena, &self.index)
    }
}

impl<T> cmp::PartialEq for ArenaPtr<T> {
    /// Note that `PartialEq` implements pointer object identity, not structural
    /// comparison. In other words, it is equivalent to the scheme function
    /// `eq?`, not the scheme function `equal?`.
    fn eq(&self, other: &ArenaPtr<T>) -> bool {
        self.index == other.index
            && (self.arena as usize) == (other.arena as usize)
    }
}

impl<T> cmp::Eq for ArenaPtr<T> { }

/// A trait for types that can be coerced to a `GcThing`.
pub trait ToGcThing: fmt::Debug {
    /// Coerce this value to a `GcThing`.
    fn to_gc_thing(&self) -> Option<GcThing>;
}

/// A smart pointer wrapping the pointer type `T`. It keeps its referent rooted
/// while the smart pointer is in scope to prevent dangling pointers caused by a
/// garbage collection within the pointers lifespan. For more information see
/// the module level documentation about rooting.
#[allow(raw_pointer_derive)]
#[derive(Hash, Debug)]
pub struct Rooted<T> {
    heap: *mut Heap,
    ptr: T,
}

impl<T: ToGcThing> Rooted<T> {
    /// Create a new `Rooted<T>`, rooting the referent.
    pub fn new(heap: &mut Heap, ptr: T) -> Rooted<T> {
        let mut r = Rooted {
            heap: heap,
            ptr: ptr,
        };
        r.add_root();
        r
    }

    /// Unroot the current referent and replace it with the given referent,
    /// which then gets rooted.
    pub fn emplace(&mut self, rhs: T) {
        self.drop_root();
        self.ptr = rhs;
        self.add_root();
    }

    /// Add the current referent as a GC root.
    fn add_root(&mut self) {
        if let Some(r) = self.ptr.to_gc_thing() {
            unsafe {
                self.heap.as_mut()
                    .expect("Rooted<T>::drop should always have a Heap")
                    .add_root(r);
            }
        }
    }

    /// Unroot the current referent.
    fn drop_root(&mut self) {
        unsafe {
            let heap = self.heap.as_mut()
                .expect("Rooted<T>::drop should always have a Heap");
            heap.drop_root(self);
        }
    }
}

impl<T: ToGcThing> ToGcThing for Rooted<T> {
    fn to_gc_thing(&self) -> Option<GcThing> {
        self.ptr.to_gc_thing()
    }
}

impl<T> Deref for Rooted<T> {
    type Target = T;
    fn deref<'a>(&'a self) -> &'a T {
        &self.ptr
    }
}

impl<T> DerefMut for Rooted<T> {
    fn deref_mut<'a>(&'a mut self) -> &'a mut T {
        &mut self.ptr
    }
}

#[unsafe_destructor]
impl<T: ToGcThing> Drop for Rooted<T> {
    fn drop(&mut self) {
        self.drop_root();
    }
}

impl<T: Copy + ToGcThing> Clone for Rooted<T> {
    fn clone(&self) -> Self {
        unsafe {
            let heap = self.heap.as_mut()
                .expect("Rooted<T>::clone should always have a Heap");
            Rooted::new(heap, self.ptr)
        }
    }
}

impl<T: PartialEq> PartialEq for Rooted<T> {
    fn eq(&self, rhs: &Self) -> bool {
        **self == **rhs
    }
}
impl<T: PartialEq + Eq> Eq for Rooted<T> { }

/// A pointer to a string on the heap.
pub type StringPtr = ArenaPtr<String>;

impl ToGcThing for StringPtr {
    fn to_gc_thing(&self) -> Option<GcThing> {
        Some(GcThing::from_string_ptr(*self))
    }
}

/// A rooted pointer to a string on the heap.
pub type RootedStringPtr = Rooted<StringPtr>;

/// The scheme heap and GC runtime, containing all allocated cons cells,
/// activations, procedures, and strings (including strings for symbols).
pub struct Heap {
    /// The static environment.
    pub environment: Environment,

    cons_cells: ArenaSet<Cons>,
    strings: ArenaSet<String>,
    activations: ArenaSet<Activation>,
    procedures: ArenaSet<Procedure>,

    roots: Vec<(GcThing, usize)>,
    symbol_table: HashMap<String, StringPtr>,
    global_activation: ActivationPtr,
    allocations: usize,
    allocations_threshold: usize,

    locations: HashMap<ConsPtr, Location>,
}

/// The default capacity of cons cells per arena.
pub static DEFAULT_CONS_CAPACITY : usize = 1 << 10;

/// The default capacity of strings per arena.
pub static DEFAULT_STRINGS_CAPACITY : usize = 1 << 10;

/// The default capacity of activations per arena.
pub static DEFAULT_ACTIVATIONS_CAPACITY : usize = 1 << 10;

/// The default capacity of procedures per arena.
pub static DEFAULT_PROCEDURES_CAPACITY : usize = 1 << 10;

/// ## `Heap` Constructors
impl Heap {
    /// Create a new `Heap` with the default capacity.
    pub fn new() -> Heap {
        Heap::with_arenas(ArenaSet::new(DEFAULT_CONS_CAPACITY),
                          ArenaSet::new(DEFAULT_STRINGS_CAPACITY),
                          ArenaSet::new(DEFAULT_ACTIVATIONS_CAPACITY),
                          ArenaSet::new(DEFAULT_PROCEDURES_CAPACITY))
    }

    /// Create a new `Heap` using the given arenas for allocating cons cells and
    /// strings within.
    pub fn with_arenas(cons_cells: ArenaSet<Cons>,
                       strings: ArenaSet<String>,
                       mut acts: ArenaSet<Activation>,
                       procs: ArenaSet<Procedure>) -> Heap {
        let mut global_act = acts.allocate();
        let mut env = Environment::new();
        define_primitives(&mut env, &mut global_act);

        let mut h = Heap {
            environment: env,

            cons_cells: cons_cells,
            strings: strings,
            activations: acts,
            procedures: procs,

            global_activation: global_act,
            roots: vec!(),
            symbol_table: HashMap::new(),
            allocations: 0,
            allocations_threshold: 0,

            locations: HashMap::new()
        };

        h.reset_gc_pressure();

        h
    }
}

/// ## `Heap` Allocation Methods
impl Heap {
    /// Allocate a new cons cell and return a pointer to it.
    ///
    /// ## Panics
    ///
    /// Panics if the `Arena` for cons cells has already reached capacity.
    pub fn allocate_cons(&mut self) -> RootedConsPtr {
        self.on_allocation();
        let c = self.cons_cells.allocate();
        Rooted::new(self, c)
    }

    /// Allocate a new string and return a pointer to it.
    ///
    /// ## Panics
    ///
    /// Panics if the `Arena` for strings has already reached capacity.
    pub fn allocate_string(&mut self) -> RootedStringPtr {
        self.on_allocation();
        let s = self.strings.allocate();
        Rooted::new(self, s)
    }

    /// Allocate a new `Activation` and return a pointer to it.
    ///
    /// ## Panics
    ///
    /// Panics if the `Arena` for activations has already reached capacity.
    pub fn allocate_activation(&mut self) -> RootedActivationPtr {
        self.on_allocation();
        let a = self.activations.allocate();
        Rooted::new(self, a)
    }

    /// Allocate a new `Procedure` and return a pointer to it.
    ///
    /// ## Panics
    ///
    /// Panics if the `Arena` for procedures has already reached capacity.
    pub fn allocate_procedure(&mut self) -> RootedProcedurePtr {
        self.on_allocation();
        let p = self.procedures.allocate();
        Rooted::new(self, p)
    }
}

/// ## `Heap` Methods for Garbage Collection
impl Heap {
    /// Perform a garbage collection on the heap.
    pub fn collect_garbage(&mut self) {
        self.reset_gc_pressure();

        // First, trace the heap graph and mark everything that is reachable.

        let mut pending_trace = self.get_roots();

        while !pending_trace.is_empty() {
            let mut newly_pending_trace = vec!();

            for thing in pending_trace.drain() {
                if !thing.is_marked() {
                    thing.mark();

                    for referent in thing.trace() {
                        newly_pending_trace.push(referent);
                    }
                }
            }

            pending_trace.append(&mut newly_pending_trace);
        }

        // Second, sweep each `ArenaSet`.

        self.strings.sweep();
        self.activations.sweep();
        self.cons_cells.sweep();
        self.procedures.sweep();
    }

    /// Explicitly add the given GC thing as a root.
    pub fn add_root(&mut self, root: GcThing) {
        for pair in self.roots.iter_mut() {
            let (ref r, ref mut count) = *pair;
            if *r == root {
                *count += 1;
                return;
            }
        }
        self.roots.push((root, 1));
    }

    /// Unroot a GC thing that was explicitly rooted with `add_root`.
    pub fn drop_root<T: ToGcThing>(&mut self, root: &Rooted<T>) {
        if let Some(r) = root.to_gc_thing() {
            self.roots = self.roots.iter().fold(vec!(), |mut v, pair| {
                let (ref rr, ref count) = *pair;
                if r == *rr {
                    if *count == 1 {
                        return v;
                    } else {
                        v.push((*rr, *count - 1));
                    }
                } else {
                    v.push((*rr, *count));
                }

                return v;
            });
        }
    }

    /// Apply pressure to the GC, and if enough pressure has built up, then
    /// perform a garbage collection.
    pub fn increase_gc_pressure(&mut self) {
        self.allocations += 1;
        if self.is_too_much_pressure() {
            self.collect_garbage();
        }
    }

    /// Get a vector of all of the GC roots.
    fn get_roots(&self) -> Vec<GcThing> {
        let mut roots: Vec<GcThing> = self.symbol_table
            .values()
            .map(|s| GcThing::from_string_ptr(*s))
            .collect();

        roots.push(GcThing::from_activation_ptr(self.global_activation));

        for pair in self.roots.iter() {
            let (ref root, _) = *pair;
            roots.push(*root);
        }

        for cons in self.locations.keys() {
            roots.push(GcThing::from_cons_ptr(*cons));
        }

        roots
    }

    /// A method that should be called on every allocation.
    fn on_allocation(&mut self)  {
        self.increase_gc_pressure();
    }

    /// Returns true when we have built up too much GC pressure, and it is time
    /// to collect garbage. False otherwise.
    fn is_too_much_pressure(&mut self) -> bool {
        self.allocations > self.allocations_threshold
    }

    /// Resets the GC pressure, so that it must build all the way back up to the
    /// max again before a GC is triggered.
    #[inline]
    fn reset_gc_pressure(&mut self) {
        self.allocations = 0;
        self.allocations_threshold =
            ((self.cons_cells.capacity / 2) * self.cons_cells.arenas.len())
            + ((self.strings.capacity / 2) * self.strings.arenas.len())
            + ((self.activations.capacity / 2) * self.activations.arenas.len())
            + ((self.procedures.capacity / 2) * self.procedures.arenas.len());
    }
}

/// ## `Heap` Environment Methods
impl Heap {
    /// Get the global activation.
    pub fn global_activation(&mut self) -> RootedActivationPtr {
        let act = self.global_activation;
        Rooted::new(self, act)
    }

    /// Extend the environment with a new lexical block containing the given
    /// variables and then perform some work before popping the new block.
    pub fn with_extended_env<T>(&mut self,
                                names: Vec<String>,
                                block: &Fn(&mut Heap) -> T) -> T {
        self.environment.extend(names);
        let result = block(self);
        self.environment.pop();
        result
    }
}

/// ## `Heap` Methods for Source Locations
impl Heap {
    /// Register the given pair as having originated from the given location.
    pub fn enlocate(&mut self, loc: Location, cons: RootedConsPtr) {
        self.locations.insert(*cons, loc);
    }

    /// Get the registered source location of the given pair. If the pair was
    /// not created by the reader, then None is returned.
    pub fn locate(&self, cons: &RootedConsPtr) -> Location {
        self.locations.get(&**cons)
            .map(|loc| loc.clone())
            .unwrap_or_else(Location::unknown)
    }
}

/// ## `Heap` Methods for Symbols
impl Heap {
    /// Ensure that there is an interned symbol extant for the given `String`
    /// and return it.
    pub fn get_or_create_symbol(&mut self, str: String) -> RootedValue {
        if self.symbol_table.contains_key(&str) {
            let sym_ptr = self.symbol_table[str];
            let rooted_sym_ptr = Rooted::new(self, sym_ptr);
            return Value::new_symbol(self, rooted_sym_ptr);
        }

        let mut symbol = self.allocate_string();
        symbol.clear();
        symbol.push_str(str.as_slice());
        self.symbol_table.insert(str, *symbol);
        return Value::new_symbol(self, symbol);
    }

    pub fn quote_symbol(&mut self) -> RootedValue {
        self.get_or_create_symbol("quote".to_string())
    }

    pub fn if_symbol(&mut self) -> RootedValue {
        self.get_or_create_symbol("if".to_string())
    }

    pub fn begin_symbol(&mut self) -> RootedValue {
        self.get_or_create_symbol("begin".to_string())
    }

    pub fn define_symbol(&mut self) -> RootedValue {
        self.get_or_create_symbol("define".to_string())
    }

    pub fn set_bang_symbol(&mut self) -> RootedValue {
        self.get_or_create_symbol("set!".to_string())
    }

    pub fn unspecified_symbol(&mut self) -> RootedValue {
        self.get_or_create_symbol("unspecified".to_string())
    }

    pub fn lambda_symbol(&mut self) -> RootedValue {
        self.get_or_create_symbol("lambda".to_string())
    }

    pub fn eof_symbol(&mut self) -> RootedValue {
        // Per R4RS, the EOF object must be something that is impossible to
        // read. We fulfill that contract by having spaces in a symbol.
        self.get_or_create_symbol("< END OF FILE >".to_string())
    }
}

/// An iterable of `GcThing`s.
pub type IterGcThing = IntoIter<GcThing>;

/// The `Trace` trait allows GC participants to inform the collector of their
/// references to other GC things.
///
/// For example, imagine we had a `Trio` type that contained three cons cells:
///
///     struct Trio {
///         first: ConsPtr,
///         second: ConsPtr,
///         third: ConsPtr,
///     }
///
/// `Trio`'s implementation of `Trace` must yield all of its cons pointers, or
/// else their referents could be reclaimed by the garbage collector, and the
/// `Trio` would have dangling pointers, leading to undefined behavior and bad
/// things when it dereferences them in the future.
///
///     impl Trace for Trio {
///         fn trace(&self) -> IterGcThing {
///             let refs = vec!(GcThing::from_cons_ptr(self.first),
///                             GcThing::from_cons_ptr(self.second),
///                             GcThing::from_cons_ptr(self.third));
///             refs.into_iter()
///         }
///     }
pub trait Trace {
    /// Return an iterable of all of the GC things referenced by this structure.
    fn trace(&self) -> IterGcThing;
}

/// The union of the various types that are GC things.
#[derive(Copy, Eq, Hash, PartialEq, Debug)]
pub enum GcThing {
    Cons(ConsPtr),
    String(StringPtr),
    Activation(ActivationPtr),
    Procedure(ProcedurePtr),
}

/// ## `GcThing` Constructors
impl GcThing {
    /// Create a `GcThing` from a `StringPtr`.
    pub fn from_string_ptr(str: StringPtr) -> GcThing {
        GcThing::String(str)
    }

    /// Create a `GcThing` from a `ConsPtr`.
    pub fn from_cons_ptr(cons: ConsPtr) -> GcThing {
        GcThing::Cons(cons)
    }

    /// Create a `GcThing` from a `ProcedurePtr`.
    pub fn from_procedure_ptr(procedure: ProcedurePtr) -> GcThing {
        GcThing::Procedure(procedure)
    }

    /// Create a `GcThing` from an `ActivationPtr`.
    pub fn from_activation_ptr(act: ActivationPtr) -> GcThing {
        GcThing::Activation(act)
    }
}

impl GcThing {
    /// During a GC, mark this `GcThing` as reachable.
    #[inline]
    fn mark(&self) {
        match *self {
            GcThing::Cons(ref p) => p.mark(),
            GcThing::String(ref p) => p.mark(),
            GcThing::Activation(ref p) => p.mark(),
            GcThing::Procedure(ref p) => p.mark(),
        }
    }

    /// During a GC, determine if this `GcThing` has been marked as reachable.
    #[inline]
    fn is_marked(&self) -> bool {
        match *self {
            GcThing::Cons(ref p) => p.is_marked(),
            GcThing::String(ref p) => p.is_marked(),
            GcThing::Activation(ref p) => p.is_marked(),
            GcThing::Procedure(ref p) => p.is_marked(),
        }
    }
}

impl Trace for GcThing {
    fn trace(&self) -> IterGcThing {
        match *self {
            GcThing::Cons(cons)      => cons.trace(),
            GcThing::Activation(act) => act.trace(),
            GcThing::Procedure(p)    => p.trace(),
            // Strings don't hold any strong references to other `GcThing`s.
            GcThing::String(_)       => vec!().into_iter(),
        }
    }
}

#[test]
fn test_heap_allocate_tons() {
    use eval::evaluate_file;

    let heap = &mut Heap::new();
    evaluate_file(heap, "./tests/test_heap_allocate_tons.scm")
        .ok()
        .expect("Should be able to eval a file.");
    assert!(true, "Should have successfully run the program and allocated many cons cells");
}
