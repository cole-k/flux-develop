#![flux::defs {
    fn nat(x: int) -> bool { leq(0, x) }
    fn leq(x: int, y: int) -> bool { x <= y }
    fn inc(x: int) -> int { x + 1 }
}]

#[flux::alias(type Nat[v: int] = { i32[v] | nat(v) })]
type Nat = i32;

#[flux::alias(type Lb(n: int)[v: int] = { i32[v] | leq(n, v) })]
type Lb = i32;

#[flux::sig(fn(x: Nat) -> Nat)]
pub fn test1(x: Nat) -> Nat {
    x - 1 //~ ERROR refinement type
}

#[flux::sig(fn(x: Lb(10)) -> Lb(10))]
pub fn test2(x: Lb) -> Lb {
    x - 1 //~ ERROR refinement type
}

#[flux::sig(fn(x: i32) -> i32[inc(x)])]
pub fn test3(x: i32) -> i32 {
    x + 2 //~ ERROR refinement type
}
