// Generic type parameters are rejected outright in v1: `ShapeBuilder<T>`
// requires `T: Any + Send + Sync` (i.e. `T: 'static`), and this macro
// cannot resolve every instantiation a generic struct might ever be used
// at to a concrete, dispatchable field-type set.
use mova::embed::host::MovaStruct;

#[derive(Clone, MovaStruct)]
struct Cache<V> {
    data: V,
}

fn main() {}
