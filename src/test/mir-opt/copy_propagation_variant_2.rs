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

// switch parameters
fn test(a: Enum<u32, i32>) -> Enum<u32, f32> {
    let z = 1.;
    match a {
        Enum::A(x) => Enum::A(x),
        Enum::B(_y) => Enum::B(z),
    }
}

fn main() { }

// test that copy propagation, constant propagation fire, but not
// variant propagation.

// END RUST SOURCE
// START rustc.node14.CopyPropagation.before.mir
//
//  bb0: {
//      _2 = _1;
//      _3 = const F32(1);
//      switch(_2) -> [A: bb1, B: bb2];
//  }
//
//  bb1: {
//      _4 = ((_2 as A).0: u32);
//      _6 = _4;
//      _0 = Enum<u32, f32>::A(_6,);
//      goto -> bb3;
//  }
//
//  bb2: {
//      _5 = ((_2 as B).0: i32);
//      _7 = _3;
//      _0 = Enum<u32, f32>::B(_7,);
//      goto -> bb3;
//  }
//
// END rustc.node14.CopyPropagation.before.mir
// START rustc.node14.CopyPropagation.after.mir
//  bb0: {
//      _2 = _1;
//      nop;
//      switch(_2) -> [A: bb1, B: bb2];
//  }
//
//  bb1: {
//      _4 = ((_2 as A).0: u32);
//      nop;
//      _0 = Enum<u32, f32>::A(_4,);
//      goto -> bb3;
//  }
//
//  bb2: {
//      _5 = ((_2 as B).0: i32);
//      nop;
//      _0 = Enum<u32, f32>::B(const F32(1),);
//      goto -> bb3;
//  }
// END rustc.node14.CopyPropagation.after.mir
