// A field type with no `embed::Value` conversion, no `#[mova(skip)]`, no
// `#[mova(getter = "...")]`: must be a hard compile error pointing at the
// field, never a silent truncation.
use mova::embed::host::MovaStruct;

#[derive(Clone, MovaStruct)]
struct BadField {
    count: u64,
}

fn main() {}
