// Tuple structs are not supported in v1 (no natural field-name source for
// the script-visible key).
use mova::embed::host::MovaStruct;

#[derive(Clone, MovaStruct)]
struct Tuple(i64, String);

fn main() {}
