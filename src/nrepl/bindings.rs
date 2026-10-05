//! The vars an nREPL session binds, and how a thread installs them.
//!
//! The list is the JVM's (`nrepl.middleware.session/default-bindings`) as far
//! as Mova has the vars. `*ns*`, `*out*` and `*err*` are handled apart: `*ns*`
//! is per session (and per eval when a request has `ns`), `*out*` / `*err*`
//! are replaced for every eval by that eval's sinks.

use crate::env::VarCell;
use crate::eval::Interp;
use crate::value::{Str, Symbol, Value};
use std::sync::Arc;

/// What a var starts as in a fresh session.
#[derive(Clone, Copy)]
enum Init {
    Nil,
    /// The var's root value when the server booted.
    Root,
    True,
}

const PERSISTENT: &[(&str, Init)] = &[
    // `*1 *2 *3 *e` must stay first: `Vars::STAR1..` index them.
    ("*1", Init::Nil),
    ("*2", Init::Nil),
    ("*3", Init::Nil),
    ("*e", Init::Nil),
    ("*warn-on-reflection*", Init::Root),
    ("*math-context*", Init::Root),
    ("*print-meta*", Init::Root),
    ("*print-length*", Init::Root),
    ("*print-level*", Init::Root),
    ("*print-namespace-maps*", Init::True),
    ("*data-readers*", Init::Root),
    ("*default-data-reader-fn*", Init::Root),
    ("*compile-path*", Init::Root),
    ("*command-line-args*", Init::Root),
    ("*unchecked-math*", Init::Root),
    ("*assert*", Init::Root),
    ("*read-eval*", Init::Root),
    ("*file*", Init::Root),
];

pub(crate) const STAR1: usize = 0;
pub(crate) const STAR2: usize = 1;
pub(crate) const STAR3: usize = 2;
pub(crate) const STARE: usize = 3;

/// The bound values of one session, in `Vars::persistent` order. Cheap to
/// clone (an `Arc`); this is what `clone` copies.
pub(crate) type Snapshot = Arc<Vec<Value>>;

pub(crate) struct Vars {
    persistent: Vec<(Arc<VarCell>, Value)>,
    pub ns: Arc<VarCell>,
    pub out: Arc<VarCell>,
    pub err: Arc<VarCell>,
    pub inp: Arc<VarCell>,
    pub file: Option<Arc<VarCell>>,
    pub source_path: Option<Arc<VarCell>>,
}

impl Vars {
    /// Resolves the cells (once, from the booted base interpreter).
    pub(crate) fn resolve(interp: &Interp) -> Vars {
        let find = |name: &str| interp.globals.find_any_cell(&Symbol::simple(name));
        let persistent = PERSISTENT
            .iter()
            .filter_map(|(name, init)| {
                let cell = find(name)?;
                let default = match init {
                    Init::Nil => Value::Nil,
                    Init::True => Value::Bool(true),
                    Init::Root => cell.raw_root().unwrap_or(Value::Nil),
                };
                Some((cell, default))
            })
            .collect::<Vec<_>>();
        // The four star vars are what makes `*1`/`*e` work: they exist in core.mova.
        assert!(
            persistent.len() >= 4 && &*persistent[0].0.name.name == "*1",
            "core.mova must define *1 *2 *3 *e"
        );
        Vars {
            persistent,
            ns: interp.globals.intern(crate::ns::ns_sym()),
            out: interp.globals.intern(&Symbol::simple("*out*")),
            err: interp.globals.intern(&Symbol::simple("*err*")),
            inp: interp.globals.intern(&Symbol::simple("*in*")),
            file: find("*file*"),
            source_path: find("*source-path*"),
        }
    }

    /// The values a fresh session starts with.
    pub(crate) fn defaults(&self) -> Snapshot {
        Arc::new(self.persistent.iter().map(|(_, d)| d.clone()).collect())
    }

    /// Pushes one frame per session var on the calling thread: the persistent
    /// vars from `init` (or the defaults), `*ns*` as `user`, `*out*` / `*err*`
    /// as nil. Pair with [`Vars::pop_session`] on the same thread.
    pub(crate) fn push_session(&self, init: Option<&Snapshot>, input: Value) {
        for (i, (cell, default)) in self.persistent.iter().enumerate() {
            let v = init.and_then(|s| s.get(i)).unwrap_or(default);
            cell.push_binding(v.clone());
        }
        self.ns.push_binding(crate::ns::ns_value(&Str::from("user")));
        self.out.push_binding(Value::Nil);
        self.err.push_binding(Value::Nil);
        self.inp.push_binding(input);
    }

    pub(crate) fn pop_session(&self) {
        self.inp.pop_binding();
        self.err.pop_binding();
        self.out.pop_binding();
        self.ns.pop_binding();
        for (cell, _) in self.persistent.iter().rev() {
            cell.pop_binding();
        }
    }

    /// Reads the session's current values (call on the session thread).
    pub(crate) fn snapshot(&self) -> Snapshot {
        Arc::new(
            self.persistent
                .iter()
                .map(|(cell, default)| cell.current_binding().unwrap_or_else(|| default.clone()))
                .collect(),
        )
    }

    /// `*1`..`*e` and the other persistent vars: write the thread's frame.
    pub(crate) fn set(&self, index: usize, v: Value) {
        if let Some((cell, _)) = self.persistent.get(index) {
            cell.set_binding(v);
        }
    }

    pub(crate) fn get(&self, index: usize) -> Value {
        self.persistent.get(index).and_then(|(c, _)| c.current_binding()).unwrap_or(Value::Nil)
    }
}
