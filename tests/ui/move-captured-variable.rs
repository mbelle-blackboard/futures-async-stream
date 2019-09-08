// compile-fail

#![feature(generators, proc_macro_hygiene)]

use futures_async_stream::async_stream_block;

fn foo<F: FnMut()>(_f: F) {}

fn main() {
    let a = String::new();
    foo(|| {
        async_stream_block! { //~ ERROR cannot move out of `a`, a captured variable in an `FnMut` closure
            yield a
        };
    });
}
