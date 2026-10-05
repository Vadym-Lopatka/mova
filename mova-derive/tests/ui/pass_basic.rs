// Sanity check for the trybuild harness itself: a struct exercising all
// three field attributes should compile cleanly and `wrap()`.
use mova::embed::host::{MovaStruct, WrapExt};

#[derive(Clone, MovaStruct)]
struct Widget {
    weight: i64,
    #[mova(rename = "display_name")]
    name: String,
    #[mova(skip)]
    secret: u64,
    #[mova(getter = "label")]
    computed: (),
}

impl Widget {
    fn label(&self) -> String {
        format!("widget-{}", self.weight)
    }
}

fn main() {
    let w = std::sync::Arc::new(Widget {
        weight: 1,
        name: "gizmo".into(),
        secret: 0,
        computed: (),
    });
    let _ = w.wrap();
    let _ = Widget::shape();
}
