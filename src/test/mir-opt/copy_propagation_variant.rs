// Copyright 2016 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

// compile-flags: -Z mir-opt-level=1

enum Enum<T, U> {
    A(T),
    B(U),
}

fn test(a: Enum<u32, i32>) -> Enum<u32, i32> {
    match a {
        Enum::A(x) => Enum::A(x),
        Enum::B(y) => Enum::B(y),
    }
}

fn main() { }

// END RUST SOURCE
// START rustc.node14.CopyPropagation.before.mir
//  bb1: {
//      _3 = ((_2 as A).0: u32);
//      _5 = _3;
//      _0 = Enum<u32, i32>::A(_5,);
//      goto -> bb3;
//  }
//
//  bb2: {
//      _4 = ((_2 as B).0: i32);
//      _6 = _4;
//      _0 = Enum<u32, i32>::B(_6,);
//      goto -> bb3;
//  }
//
// END rustc.node14.CopyPropagation.before.mir
// START rustc.node14.CopyPropagation.after.mir
//  bb1: {
//      nop;
//      nop;
//      _0 = _2;
//      goto -> bb3;
//  }
//
//  bb2: {
//      nop;
//      nop;
//      _0 = _2;
//      goto -> bb3;
//  }
//
// END rustc.node14.CopyPropagation.after.mir
