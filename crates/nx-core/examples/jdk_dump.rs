//! Dump the JDK index as TSV: class, name, kind, type, params, row, col, end_row, end_col (validation against kondo).
use nx_core::jdk;
fn main() {
    let zip = jdk::resolve_zip(None).expect("jdk zip");
    let j = jdk::open_zip(&zip, 4).expect("index");
    for i in 0..j.class_count() {
        let c = jdk::ClassRef(i as u32);
        println!("C\t{}\t{}", j.class_name(c), j.class_entry(c));
        let src = j.class_source(c);
        for m in j.members(c) {
            let (r, co, er, ec) = j.member_pos(m);
            println!(
                "M\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                j.class_name(c),
                j.member_name(m),
                if j.is_method(m) { "method" } else { "field" },
                j.member_type(m).unwrap_or(""),
                j.member_params(m).map(|p| p.join(", ")).unwrap_or("-".into()).replace('\n', "\\n"),
                r,
                co,
                er,
                ec
            );
            let (lo, hi) = j.member_doc_span(m);
            if hi > lo {
                if let Some(d) = src.as_deref().and_then(|t| t.get(lo..hi)) {
                    println!("D\t{}", d.replace('\n', "\\n"));
                }
            }
        }
    }
}
