//! fmt_stdin: format stdin with the default cljfmt config and print the result.
use std::io::Read;
fn main() {
    let mut s = String::new();
    std::io::stdin().read_to_string(&mut s).unwrap();
    print!("{}", nx_core::fmt::format(&s, &nx_core::fmt::FmtConfig::default()));
}
