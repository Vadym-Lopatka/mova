// A non-`'static` lifetime parameter makes the whole struct non-`'static`,
// which fails `T: Any` -- the derive should say so directly
// ("MovaStruct requires T: 'static (found lifetime parameter 'a)"),
// not surface `ShapeBuilder`'s generic trait-bound error against generated
// code the user never wrote (DESIGN-hoststruct-derive.md §6).
use mova::embed::host::MovaStruct;

#[derive(Clone, MovaStruct)]
struct Borrowed<'a> {
    data: &'a [u8],
}

fn main() {}
