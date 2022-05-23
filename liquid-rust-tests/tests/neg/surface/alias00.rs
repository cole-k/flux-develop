#![feature(register_tool)]
#![register_tool(lr)]

#[lr::alias(type Nat() = i32{v: 0 <= v})]
type _Nat = i32;

#[lr::alias(type Lb(n) = i32{v: n <= v})]
type _Lb = i32;

#[lr::sig(fn(x:Nat) -> Nat)]
pub fn test0(x: i32) -> i32 { //~ ERROR postcondition
    x - 1
}

#[lr::sig(fn(x:Lb[0]) -> Lb[10])]
pub fn test2(x: i32) -> i32 { //~ ERROR postcondition
    x + 1
}