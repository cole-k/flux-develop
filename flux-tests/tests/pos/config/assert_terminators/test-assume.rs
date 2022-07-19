#![feature(register_tool)]
#![register_tool(flux)]
#![feature(custom_inner_attributes)]
#![flux::cfg(check_asserts = "assume")]

#[flux::sig(fn(x: i32, y: i32) -> i32)]
pub fn test(x: i32, y: i32) -> i32 {
    x / y
}