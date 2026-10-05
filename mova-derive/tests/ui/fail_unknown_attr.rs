// An unrecognized `#[mova(...)]` key must be a hard compile error, not a
// silent no-op (silently ignoring a typo'd attribute -- e.g. `rename`
// misspelled as `renme` -- would be a much worse failure mode: the field
// would just silently keep its default key).
use mova::embed::host::MovaStruct;

#[derive(Clone, MovaStruct)]
struct BadAttr {
    #[mova(renme = "oops")]
    n: i64,
}

fn main() {}
