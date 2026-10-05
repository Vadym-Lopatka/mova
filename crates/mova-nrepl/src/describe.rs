//! The `describe` reply: fixed data, copied from the JVM nREPL 1.8.0 reply.
//!
//! * `describe_ops.bencode` is the `ops` map of a verbose JVM reply, encoded
//!   once (docs, requires, optional, returns for the 11 ops). It was cut from
//!   `oracle/goldens/a01_describe.json` and is copied to the wire as is.
//! * The short form (no `verbose?` key) is each op name with an empty map.
//! * `versions` come from a table the backend can replace (`Backend::versions`).

use crate::bencode::{write_reply, V};

/// The 10 middleware var names of the JVM default stack, in its order.
pub const MIDDLEWARE: &[&str] = &[
    "#'nrepl.middleware/wrap-describe",
    "#'nrepl.middleware.completion/wrap-completion",
    "#'nrepl.middleware.interruptible-eval/interruptible-eval",
    "#'nrepl.middleware.io/wrap-out",
    "#'nrepl.middleware.load-file/wrap-load-file",
    "#'nrepl.middleware.caught/wrap-caught",
    "#'nrepl.middleware.lookup/wrap-lookup",
    "#'nrepl.middleware.print/wrap-print",
    "#'nrepl.middleware.session/add-stdin",
    "#'nrepl.middleware.session/session",
];

/// Op names, sorted. These are the ops of nREPL 1.8.0 (no `add-middleware`,
/// `ls-middleware`, `swap-middleware`).
pub const OPS: &[&str] = &[
    "clone",
    "close",
    "completions",
    "describe",
    "eval",
    "forward-system-output",
    "interrupt",
    "load-file",
    "lookup",
    "ls-sessions",
    "stdin",
];

const OPS_VERBOSE: &[u8] = include_bytes!("describe_ops.bencode");

const EMPTY: V<'static> = V::Dict(&[]);
const OPS_SHORT: &[(&str, V<'static>)] = &[
    ("clone", EMPTY),
    ("close", EMPTY),
    ("completions", EMPTY),
    ("describe", EMPTY),
    ("eval", EMPTY),
    ("forward-system-output", EMPTY),
    ("interrupt", EMPTY),
    ("load-file", EMPTY),
    ("lookup", EMPTY),
    ("ls-sessions", EMPTY),
    ("stdin", EMPTY),
];

/// The version of nREPL this server claims to be.
pub const NREPL_VERSION: &str = "1.8.0";

/// What `describe` reports under `versions`. `Default` is what the JVM
/// reference (nREPL 1.8.0 on Clojure 1.13.0-alpha6, Java 25.0.4) reports, which
/// is what the wire goldens expect. The interpreter supplies its own through
/// `Backend::versions`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Versions {
    /// `*clojure-version*` of the language the backend implements.
    pub clojure: ClojureVersion,
    /// What `(System/getProperty "java.version")` gives.
    pub java: JavaVersion,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClojureVersion {
    pub major: i64,
    pub minor: i64,
    pub incremental: i64,
    /// `None` for a release.
    pub qualifier: Option<String>,
    pub version_string: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JavaVersion {
    pub major: i64,
    pub version_string: String,
}

impl Default for Versions {
    fn default() -> Versions {
        Versions {
            clojure: ClojureVersion {
                major: 1,
                minor: 13,
                incremental: 0,
                qualifier: Some("alpha6".into()),
                version_string: "1.13.0-alpha6".into(),
            },
            java: JavaVersion { major: 25, version_string: "25.0.4".into() },
        }
    }
}

/// Writes the reply for `describe` into `out`.
///
/// `id` and `session` are echoed as the router decided; `current_ns` is the
/// session's namespace; `verbose` is true when the request had a `verbose?`
/// key (any value, as on the JVM).
pub fn write_describe(
    out: &mut Vec<u8>,
    id: Option<&[u8]>,
    session: V<'_>,
    current_ns: &str,
    verbose: bool,
    versions: &Versions,
) {
    let c = &versions.clojure;
    let mut cl: [(&str, V); 5] = [
        ("incremental", V::Int(c.incremental)),
        ("major", V::Int(c.major)),
        ("minor", V::Int(c.minor)),
        ("version-string", V::Str(&c.version_string)),
        ("qualifier", V::Str("")),
    ];
    let cl_len = match &c.qualifier {
        Some(q) => {
            cl[4].1 = V::Str(q);
            5
        }
        None => 4,
    };
    let j = &versions.java;
    let java = [("major", V::Int(j.major)), ("version-string", V::Str(&j.version_string))];
    let nrepl = [
        ("incremental", V::Int(0)),
        ("major", V::Int(1)),
        ("minor", V::Int(8)),
        ("version-string", V::Str(NREPL_VERSION)),
    ];
    let vers = [
        ("clojure", V::Dict(&cl[..cl_len])),
        ("java", V::Dict(&java)),
        ("nrepl", V::Dict(&nrepl)),
    ];
    let aux = [("current-ns", V::Str(current_ns))];
    let ops = if verbose { V::Raw(OPS_VERBOSE) } else { V::Dict(OPS_SHORT) };
    write_reply(
        out,
        id,
        session,
        &[
            ("aux", V::Dict(&aux)),
            ("middleware", V::Strs(MIDDLEWARE)),
            ("ops", ops),
            ("status", V::Strs(&["done"])),
            ("versions", V::Dict(&vers)),
        ],
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bencode::{decode, Value};

    fn describe(verbose: bool) -> Value {
        let mut out = Vec::new();
        write_describe(&mut out, Some(b"1:1"), V::Str("S"), "user", verbose, &Versions::default());
        let (v, used) = decode(&out).unwrap().unwrap();
        assert_eq!(used, out.len());
        v
    }

    #[test]
    fn verbose_ops_blob_is_valid_and_lists_all_ops() {
        let (v, used) = decode(OPS_VERBOSE).unwrap().unwrap();
        assert_eq!(used, OPS_VERBOSE.len());
        let Value::Dict(m) = v else { panic!() };
        let names: Vec<_> = m.keys().map(|k| String::from_utf8_lossy(k).to_string()).collect();
        assert_eq!(names, OPS);
    }

    #[test]
    fn short_and_verbose() {
        let Some(Value::Dict(ops)) = describe(false).get("ops").cloned() else { panic!() };
        assert_eq!(ops.len(), 11);
        assert!(ops.values().all(|v| *v == Value::Dict(Default::default())));
        let Some(Value::Dict(ops)) = describe(true).get("ops").cloned() else { panic!() };
        assert!(ops[&b"eval".to_vec()].get("doc").is_some());
    }

    #[test]
    fn versions_and_aux() {
        let d = describe(false);
        let v = d.get("versions").unwrap();
        assert_eq!(v.get("nrepl").unwrap().get("version-string").unwrap().as_str(), Some("1.8.0"));
        assert_eq!(v.get("clojure").unwrap().get("qualifier").unwrap().as_str(), Some("alpha6"));
        assert_eq!(v.get("java").unwrap().get("major"), Some(&Value::Int(25)));
        assert_eq!(d.get("aux").unwrap().get("current-ns").unwrap().as_str(), Some("user"));
        assert_eq!(d.get("session").unwrap().as_str(), Some("S"));
    }

    #[test]
    fn release_has_no_qualifier() {
        let mut v = Versions::default();
        v.clojure.qualifier = None;
        let mut out = Vec::new();
        write_describe(&mut out, None, V::Str("S"), "user", false, &v);
        let (d, _) = decode(&out).unwrap().unwrap();
        assert!(d.get("versions").unwrap().get("clojure").unwrap().get("qualifier").is_none());
    }
}
