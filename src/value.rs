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

//! Scheme value implementation.
//!
//! ## Allocation Strategy
//!
//! Scheme has a variety of types that must be allocated on the heap: cons
//! cells, strings, and vectors (currently unimplemented). Oxischeme's current
//! allocation strategy is deliberately as simple as possible. We represent the
//! heap as a pre-allocated vector of cons cells. We keep track of un-allocated
//! cons cells with a "free list" of indices into the pre-allocated vector of
//! cons cells. Whenever we need to allocate a new cons cell, we remove an entry
//! from this list and return a pointer to the object at that entry's index.
//! Garbage collection (currently unimplemented) will add new entries to the
//! free list whenever objects are reclaimed. If we run out of space in the
//! pre-allocated vector, we panic.

use std::cmp;
use std::default::{Default};
use std::fmt;
use std::iter::{range};
use std::ops::{Deref, DerefMut};

use context::{Context};

/// A cons cell is a pair of `car` and `cdr` values. A list is one or more cons
/// cells, daisy chained together via the `cdr`. A list is "proper" if the last
/// `cdr` is `Value::EmptyList`, or the scheme value `()`. Otherwise, it is
/// "improper".
///
/// You cannot directly create a cons cell, you must allocate one on the heap
/// with an `Arena` and get back a `ConsPtr`.
#[deriving(Copy, PartialEq)]
pub struct Cons {
    car: Value,
    cdr: Value,
}

impl Default for Cons {
    /// Create a new cons pair, with both `car` and `cdr` initialized to
    /// `Value::EmptyList`.
    fn default() -> Cons {
        Cons {
            car: Value::EmptyList,
            cdr: Value::EmptyList,
        }
    }
}

impl Cons {
    /// Get the car of this cons cell.
    pub fn car(&self) -> Value {
        self.car
    }

    /// Get the cdr of this cons cell.
    pub fn cdr(&self) -> Value {
        self.cdr
    }

    /// Set the car of this cons cell.
    pub fn set_car(&mut self, car: Value) {
        self.car = car;
    }

    /// Set the cdr of this cons cell.
    pub fn set_cdr(&mut self, cdr: Value) {
        self.cdr = cdr;
    }
}

/// We use a vector for our implementation of a free list. `Vector::push` to add
/// new entries, `Vector::pop` to remove the next entry when we allocate.
type FreeList = Vec<uint>;

/// An arena from which to allocate objects from.
pub struct Arena<T> {
    pool: Vec<T>,
    free: FreeList,
}

impl<T: Default> Arena<T> {
    /// Create a new `Arena` with the capacity to allocate the given number of
    /// `T` instances.
    pub fn new(capacity: uint) -> Arena<T> {
        assert!(capacity > 0);
        Arena {
            pool: range(0, capacity).map(|_| Default::default()).collect(),
            free: range(0, capacity).collect(),
        }
    }

    /// Get this heap's capacity for simultaneously allocated cons cells.
    pub fn capacity(&self) -> uint {
        self.pool.capacity()
    }

    /// Allocate a new `T` instance and return a pointer to it.
    ///
    /// ## Panics
    ///
    /// Panics if the number of `T` instances allocated is already equal to this
    /// `Arena`'s capacity and no more `T` instances can be allocated.
    pub fn allocate(&mut self) -> ArenaPtr<T> {
        match self.free.pop() {
            None      => panic!("Arena::allocate: out of memory!"),
            Some(idx) => {
                let self_ptr : *mut Arena<T> = self;
                ArenaPtr::new(self_ptr, idx)
            },
        }
    }
}

/// A pointer to a `T` instance in an arena.
pub struct ArenaPtr<T> {
    arena: *mut Arena<T>,
    index: uint,
}

// XXX: We have to manually declar that ArenaPtr<T> is copy-able because if we
// use `#[deriving(Copy)]` it wants T to be copy-able as well, despite the fact
// that we only need to copy our pointer to the Arena<T>, not any T or the Arena
// itself.
impl <T> ::std::kinds::Copy for ArenaPtr<T> { }

impl<T: Default> ArenaPtr<T> {
    /// Create a new `ArenaPtr` to the `T` instance at the given index in the
    /// provided arena. **Not** publicly exposed, and should only be called by
    /// `Arena::allocate`.
    fn new(arena: *mut Arena<T>, index: uint) -> ArenaPtr<T> {
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
}

impl<T> Deref<T> for ArenaPtr<T> {
    fn deref<'a>(&'a self) -> &'a T {
        unsafe {
            let arena = self.arena.as_ref()
                .expect("ArenaPtr::deref should always have an Arena.");
            &arena.pool[self.index]
        }
    }
}

impl<T> DerefMut<T> for ArenaPtr<T> {
    fn deref_mut<'a>(&'a mut self) -> &'a mut T {
        unsafe {
            let arena = self.arena.as_mut()
                .expect("ArenaPtr::deref_mut should always have an Arena.");
            &mut arena.pool[self.index]
        }
    }
}

impl<T> fmt::Show for ArenaPtr<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "ArenaPtr({}, {})", self.arena.to_uint(), self.index)
    }
}

impl<T> cmp::PartialEq for ArenaPtr<T> {
    /// Note that `PartialEq` implements pointer object identity, not structural
    /// comparison. In other words, it is equivalent to the scheme function
    /// `eq?`, not the scheme function `equal?`
    fn eq(&self, other: &ArenaPtr<T>) -> bool {
        self.index == other.index && self.arena.to_uint() == other.arena.to_uint()
    }
}

/// A pointer to a cons cell on the heap.
pub type ConsPtr = ArenaPtr<Cons>;

/// A pointer to a string on the heap.
pub type StringPtr = ArenaPtr<String>;

/// The scheme heap, containing all allocated cons cells and strings (including
/// symbols).
pub struct Heap {
    cons_cells: Arena<Cons>,
    strings: Arena<String>,
}

/// The default capacity of cons cells.
pub static DEFAULT_CONS_CAPACITY : uint = 1 << 12;

/// The default capacity of strings.
pub static DEFAULT_STRINGS_CAPACITY : uint = 1 << 12;

impl Heap {
    /// Create a new `Heap` with the default capacity.
    pub fn new() -> Heap {
        Heap::with_arenas(Arena::new(DEFAULT_CONS_CAPACITY),
                          Arena::new(DEFAULT_STRINGS_CAPACITY))
    }

    /// Create a new `Heap` using the given arenas for allocating cons cells and
    /// strings within.
    pub fn with_arenas(cons_cells: Arena<Cons>, strings: Arena<String>) -> Heap {
        Heap {
            cons_cells: cons_cells,
            strings: strings
        }
    }

    /// Allocate a new cons cell and return a pointer to it.
    ///
    /// ## Panics
    ///
    /// Panics if the `Arena` for cons cells has already reached capacity.
    pub fn allocate_cons(&mut self) -> ConsPtr {
        self.cons_cells.allocate()
    }

    /// Allocate a new string and return a pointer to it.
    ///
    /// ## Panics
    ///
    /// Panics if the `Arena` for strings has already reached capacity.
    pub fn allocate_string(&mut self) -> StringPtr {
        self.strings.allocate()
    }
}

/// `Value` represents a scheme value of any type.
///
/// Note that `PartialEq` is object identity, not structural comparison, same as
/// with [`ArenaPtr`](struct.ArenaPtr.html).
#[deriving(Copy, PartialEq, Show)]
pub enum Value {
    /// The empty list: `()`.
    EmptyList,

    /// The scheme pair type is a pointer to a GC-managed `Cons` cell.
    Pair(ConsPtr),

    /// The scheme string type is a pointer to a GC-managed `String`.
    String(StringPtr),

    /// Scheme symbols are also implemented as a pointer to a GC-managed
    /// `String`.
    Symbol(StringPtr),

    /// Scheme integers are represented as 64 bit integers.
    Integer(i64),

    /// Scheme booleans are represented with `bool`.
    Boolean(bool),

    /// Scheme characters are `char`s.
    Character(char),
}

/// # `Value` Constructors
impl Value {
    /// Create a new integer value.
    pub fn new_integer(i: i64) -> Value {
        Value::Integer(i)
    }

    /// Create a new boolean value.
    pub fn new_boolean(b: bool) -> Value {
        Value::Boolean(b)
    }

    /// Create a new character value.
    pub fn new_character(c: char) -> Value {
        Value::Character(c)
    }

    /// Create a new cons pair value with the given car and cdr.
    pub fn new_pair(heap: &mut Heap, car: Value, cdr: Value) -> Value {
        let mut cons = heap.allocate_cons();
        cons.set_car(car);
        cons.set_cdr(cdr);
        Value::Pair(cons)
    }

    /// Create a new string value with the given string.
    pub fn new_string(heap: &mut Heap, str: String) -> Value {
        let mut value = heap.allocate_string();
        value.clear();
        value.push_str(str.as_slice());
        Value::String(value)
    }

    /// Create a new symbol value with the given string.
    pub fn new_symbol(str: StringPtr) -> Value {
        Value::Symbol(str)
    }
}

/// # `Value` Methods
impl Value {
    /// Assuming this value is a cons pair, get its car value. Otherwise, return
    /// `None`.
    pub fn car(&self) -> Option<Value> {
        match *self {
            Value::Pair(ref cons) => Some(cons.car()),
            _                     => None,
        }
    }

    /// Assuming this value is a cons pair, get its cdr value. Otherwise, return
    /// `None`.
    pub fn cdr(&self) -> Option<Value> {
        match *self {
            Value::Pair(ref cons) => Some(cons.cdr()),
            _                     => None,
        }
    }

    /// Return true if this value is a pair, false otherwise.
    pub fn is_pair(&self) -> bool {
        match *self {
            Value::Pair(_) => true,
            _              => false,
        }
    }

    /// Return true if this value is an atom, false otherwise.
    pub fn is_atom(&self) -> bool {
        !self.is_pair()
    }

    /// Convert this symbol value to a `StringPtr` to the symbol's string name.
    pub fn to_symbol(&self) -> Option<StringPtr> {
        match *self {
            Value::Symbol(sym) => Some(sym),
            _                  => None,
        }
    }

    /// Convert this pair value to a `ConsPtr` to the cons cell this pair is
    /// referring to.
    pub fn to_pair(&self) -> Option<ConsPtr> {
        match *self {
            Value::Pair(cons) => Some(cons),
            _                 => None,
        }
    }

    /// Assuming that this value is a proper list, get the length of the list.
    pub fn len(&self) -> Result<u64, ()> {
        match *self {
            Value::EmptyList => Ok(0),
            Value::Pair(p)   => {
                let cdr_len = try!(p.cdr().len());
                Ok(cdr_len + 1)
            },
            _                => Err(()),
        }
    }
}

/// Either a Scheme `Value`, or a `String` containing an error message.
pub type SchemeResult = Result<Value, String>;

/// A helper utility to create a cons list from the given values.
pub fn list(ctx: &mut Context, values: &[Value]) -> Value {
    list_helper(ctx, &mut values.iter())
}

fn list_helper<'a, T: Iterator<&'a Value>>(ctx: &mut Context,
                                           values: &mut T) -> Value {
    match values.next() {
        None      => Value::EmptyList,
        Some(car) => {
            let cdr = list_helper(ctx, values);
            Value::new_pair(ctx.heap(), *car, cdr)
        },
    }
}

/// The 28 car/cdr compositions.
impl Cons {
    pub fn cddr(&self) -> SchemeResult {
        self.cdr.cdr().ok_or("bad cddr".to_string())
    }

    pub fn cdddr(&self) -> SchemeResult {
        let cddr = try!(self.cddr());
        cddr.cdr().ok_or("bad cdddr".to_string())
    }

    // TODO FITZGEN: cddddr

    pub fn cadr(&self) -> SchemeResult {
        self.cdr.car().ok_or("bad cadr".to_string())
    }

    pub fn caddr(&self) -> SchemeResult {
        let cddr = try!(self.cddr());
        cddr.car().ok_or("bad caddr".to_string())
    }

    pub fn cadddr(&self) -> SchemeResult {
        let cdddr = try!(self.cdddr());
        cdddr.car().ok_or("bad caddr".to_string())
    }

    // TODO FITZGEN ...
}

