// Copyright 2016 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

// Test that regions which appear only in the closure's generics (in
// this case, `'a`) are properly mapped to the creator's generics. In
// this case, the closure constrains its type parameter `T` to outlive
// the same `'a` for which it implements `Trait`, which can only be the `'a`
// from the function definition.

// compile-flags:-Znll -Zborrowck=mir -Zverbose

#![feature(rustc_attrs)]
#![allow(dead_code)]

trait Trait<'a> {}

fn establish_relationships<T, F>(value: T, closure: F)
where
    F: FnOnce(T),
{
    closure(value)
}

fn require<'a, T>(t: T)
where
    T: Trait<'a> + 'a,
{
}

#[rustc_regions]
fn supply<'a, T>(value: T)
where
    T: Trait<'a>,
{
    establish_relationships(value, |value| {
        //~^ ERROR `T` does not outlive

        // This function call requires that
        //
        // (a) T: Trait<'a>
        //
        // and
        //
        // (b) T: 'a
        //
        // The latter does not hold.

        require(value);
        //~^ WARNING not reporting region error due to -Znll
    });
}

fn main() {}
