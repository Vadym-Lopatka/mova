// Enums are not supported in v1: `Value::HostStruct` wraps map-shaped data
// only, and an enum's script representation isn't designed
// (DESIGN-hoststruct-derive.md §7's deferred list).
use mova::embed::host::MovaStruct;

#[derive(Clone, MovaStruct)]
enum NotAStruct {
    A,
    B(i64),
}

fn main() {}
