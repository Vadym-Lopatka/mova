// `#[mova(skip)]` and `#[mova(getter = "...")]` on the SAME field disagree
// about whether the field is read at all -- must be a hard compile error.
use mova::embed::host::MovaStruct;

#[derive(Clone, MovaStruct)]
struct Conflicted {
    #[mova(skip)]
    #[mova(getter = "computed")]
    n: i64,
}

impl Conflicted {
    fn computed(&self) -> i64 {
        self.n
    }
}

fn main() {}
