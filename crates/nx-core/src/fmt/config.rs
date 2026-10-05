//! cljfmt configuration (subset used by clojure-lsp) and the compiled indent rule table.

use std::collections::HashMap;

/// `:function-arguments-indentation`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FnArgIndent {
    Community,
    Cursive,
    Zprint,
}

/// One cljfmt indent spec element.
#[derive(Clone, Debug, PartialEq)]
pub enum Spec {
    /// `[:inner depth idx?]`
    Inner(usize, Option<usize>),
    /// `[:block idx]`
    Block(usize),
    /// `[:default]`
    Default,
}

/// Key part of a vector indent key `[ns-part name-part]`.
#[derive(Clone, Debug)]
pub enum Part {
    Name(String),
    Re(String),
}

#[derive(Clone, Debug)]
pub enum Key {
    /// Simple symbol: matches on the symbol name (any namespace).
    Sym(String),
    /// Qualified symbol `ns/name`: matches the fully-qualified symbol.
    Qual(String, String),
    /// Regex on the symbol name (Rust regex syntax), or one of the builtin fuzzy patterns.
    Re(String),
    Vec(Part, Part),
}

#[derive(Clone, Debug)]
pub struct Rule {
    pub key: Key,
    pub specs: Vec<Spec>,
}

pub struct FmtConfig {
    pub indentation: bool,
    pub remove_consecutive_blank_lines: bool,
    pub remove_surrounding_whitespace: bool,
    pub insert_missing_whitespace: bool,
    pub remove_trailing_whitespace: bool,
    pub remove_multiple_non_indenting_spaces: bool,
    pub indent_line_comments: bool,
    pub normalize_newlines_at_file_end: bool,
    pub function_arguments_indentation: FnArgIndent,
    /// Effective indents (`:indents` merged with `:extra-indents`).
    pub indents: Vec<Rule>,
    /// `:alias-map` (alias -> namespace); used when indent keys are qualified.
    pub alias_map: HashMap<String, String>,
    /// `:refer-map` (referred name -> namespace).
    pub refer_map: HashMap<String, String>,
    cache: std::sync::OnceLock<Compiled>,
}

impl Clone for FmtConfig {
    fn clone(&self) -> Self {
        FmtConfig {
            indentation: self.indentation,
            remove_consecutive_blank_lines: self.remove_consecutive_blank_lines,
            remove_surrounding_whitespace: self.remove_surrounding_whitespace,
            insert_missing_whitespace: self.insert_missing_whitespace,
            remove_trailing_whitespace: self.remove_trailing_whitespace,
            remove_multiple_non_indenting_spaces: self.remove_multiple_non_indenting_spaces,
            indent_line_comments: self.indent_line_comments,
            normalize_newlines_at_file_end: self.normalize_newlines_at_file_end,
            function_arguments_indentation: self.function_arguments_indentation,
            indents: self.indents.clone(),
            alias_map: self.alias_map.clone(),
            refer_map: self.refer_map.clone(),
            cache: std::sync::OnceLock::new(),
        }
    }
}

use Spec::{Block, Inner};

fn sym(n: &str, specs: Vec<Spec>) -> Rule {
    Rule { key: Key::Sym(n.to_string()), specs }
}

/// cljfmt 0.16.4 default indents (clojure + compojure + fuzzy resources).
pub fn default_indents() -> Vec<Rule> {
    let b = |i| vec![Block(i)];
    let inn = |d| vec![Inner(d, None)];
    let mut v = vec![
        sym("alt!", b(0)),
        sym("alt!!", b(0)),
        sym("are", b(2)),
        sym("as->", b(2)),
        sym("binding", b(1)),
        sym("bound-fn", inn(0)),
        sym("case", b(1)),
        sym("catch", b(2)),
        sym("comment", b(0)),
        sym("cond", b(0)),
        sym("condp", b(2)),
        sym("cond->", b(1)),
        sym("cond->>", b(1)),
        sym("def", inn(0)),
        sym("defmacro", inn(0)),
        sym("defmethod", inn(0)),
        sym("defmulti", inn(0)),
        sym("defn", inn(0)),
        sym("defn-", inn(0)),
        sym("defonce", inn(0)),
        sym("defprotocol", vec![Block(1), Inner(1, None)]),
        sym("defrecord", vec![Block(2), Inner(1, None)]),
        sym("defstruct", b(1)),
        sym("deftest", inn(0)),
        sym("deftype", vec![Block(2), Inner(1, None)]),
        sym("delay", b(0)),
        sym("do", b(0)),
        sym("doseq", b(1)),
        sym("dotimes", b(1)),
        sym("doto", b(1)),
        sym("extend", b(1)),
        sym("extend-protocol", vec![Block(1), Inner(1, None)]),
        sym("extend-type", vec![Block(1), Inner(1, None)]),
        sym("fdef", inn(0)),
        sym("finally", b(0)),
        sym("fn", inn(0)),
        sym("for", b(1)),
        sym("future", b(0)),
        sym("go", b(0)),
        sym("go-loop", b(1)),
        sym("if", b(1)),
        sym("if-let", b(1)),
        sym("if-not", b(1)),
        sym("if-some", b(1)),
        sym("let", b(1)),
        sym("let*", b(1)),
        sym("letfn", vec![Block(1), Inner(2, Some(0))]),
        sym("locking", b(1)),
        sym("loop", b(1)),
        sym("match", b(1)),
        sym("ns", b(1)),
        sym("proxy", vec![Block(2), Inner(1, None)]),
        sym("reify", vec![Inner(0, None), Inner(1, None)]),
        sym("struct-map", b(1)),
        sym("testing", b(1)),
        sym("thread", b(0)),
        sym("try", b(0)),
        sym("use-fixtures", inn(0)),
        sym("when", b(1)),
        sym("when-first", b(1)),
        sym("when-let", b(1)),
        sym("when-not", b(1)),
        sym("when-some", b(1)),
        sym("while", b(1)),
        sym("with-local-vars", b(1)),
        sym("with-open", b(1)),
        sym("with-out-str", b(0)),
        sym("with-precision", b(1)),
        sym("with-redefs", b(1)),
        // compojure
        sym("ANY", inn(0)),
        sym("DELETE", inn(0)),
        sym("GET", inn(0)),
        sym("HEAD", inn(0)),
        sym("OPTIONS", inn(0)),
        sym("PATCH", inn(0)),
        sym("POST", inn(0)),
        sym("PUT", inn(0)),
        sym("context", inn(0)),
        sym("defroutes", inn(0)),
        sym("let-routes", b(1)),
        sym("rfn", inn(0)),
    ];
    // fuzzy: builtin regexes (Java lookaheads are hand-coded in the matcher)
    v.push(Rule { key: Key::Re("^def(?!ault)(?!late)(?!er)".into()), specs: inn(0) });
    v.push(Rule { key: Key::Re("^with-".into()), specs: inn(0) });
    v
}

impl Default for FmtConfig {
    fn default() -> Self {
        FmtConfig {
            indentation: true,
            remove_consecutive_blank_lines: true,
            remove_surrounding_whitespace: true,
            insert_missing_whitespace: true,
            remove_trailing_whitespace: true,
            remove_multiple_non_indenting_spaces: false,
            indent_line_comments: false,
            normalize_newlines_at_file_end: false,
            function_arguments_indentation: FnArgIndent::Community,
            indents: default_indents(),
            alias_map: HashMap::new(),
            refer_map: HashMap::new(),
            cache: std::sync::OnceLock::new(),
        }
    }
}

impl FmtConfig {
    /// Add or replace (`merge`) an indent rule, as `:extra-indents` / style-indent metadata do.
    pub(crate) fn compiled(&self) -> &Compiled {
        self.cache.get_or_init(|| Compiled::new(&self.indents))
    }
    /// Call after mutating `indents` directly.
    pub fn invalidate(&mut self) {
        self.cache = std::sync::OnceLock::new();
    }
    pub fn set_indent(&mut self, key: Key, specs: Vec<Spec>) {
        self.cache = std::sync::OnceLock::new();
        fn same(a: &Key, b: &Key) -> bool {
            match (a, b) {
                (Key::Sym(x), Key::Sym(y)) => x == y,
                (Key::Qual(a1, a2), Key::Qual(b1, b2)) => a1 == b1 && a2 == b2,
                _ => false,
            }
        }
        if let Some(r) = self.indents.iter_mut().find(|r| same(&r.key, &key)) {
            r.specs = specs;
        } else {
            self.indents.push(Rule { key, specs });
        }
    }
}

// ---- compiled form ----

pub(crate) enum Pat {
    FuzzyDef,
    FuzzyWith,
    Re(regex::Regex),
}

impl Pat {
    pub fn compile(src: &str) -> Option<Pat> {
        match src {
            "^def(?!ault)(?!late)(?!er)" => Some(Pat::FuzzyDef),
            "^with-" => Some(Pat::FuzzyWith),
            _ => regex::Regex::new(src).ok().map(Pat::Re),
        }
    }
    pub fn find(&self, s: &str) -> bool {
        match self {
            Pat::FuzzyDef => match s.strip_prefix("def") {
                Some(r) => !(r.starts_with("ault") || r.starts_with("late") || r.starts_with("er")),
                None => false,
            },
            Pat::FuzzyWith => s.starts_with("with-"),
            Pat::Re(r) => r.is_match(s),
        }
    }
}

pub(crate) enum CPart {
    Name(String),
    Re(Pat),
}

pub(crate) enum CKey {
    Sym(String),
    Qual(String, String),
    Re(Pat),
    Vec(CPart, CPart),
}

pub(crate) struct CRule {
    pub key: CKey,
    pub specs: Vec<Spec>,
}

/// Indent rules sorted like cljfmt's `indent-order`, with a name index for pruning.
pub(crate) struct Compiled {
    pub rules: Vec<CRule>,
    pub by_name: HashMap<String, Vec<u32>>,
    pub others: Vec<u32>,
    pub max_depth: usize,
    pub needs_ctx: bool,
}

impl Compiled {
    pub fn new(rules: &[Rule]) -> Compiled {
        // sort key: (-max-depth, key-order, str key)
        struct Item<'a> {
            r: &'a Rule,
            depth: usize,
            order: i32,
            s: String,
        }
        let mut items: Vec<Item> = rules
            .iter()
            .map(|r| {
                let depth = r.specs.iter().map(|s| if let Spec::Inner(d, _) = s { *d } else { 0 }).max().unwrap_or(0);
                let (order, s) = match &r.key {
                    Key::Qual(n, m) => (0, format!("{}/{}", n, m)),
                    Key::Sym(n) => (1, n.clone()),
                    Key::Re(p) => (2, p.clone()),
                    Key::Vec(a, b) => {
                        let f = |p: &Part| match p {
                            Part::Name(n) | Part::Re(n) => n.clone(),
                        };
                        (-1, format!("[{} {}]", f(a), f(b)))
                    }
                };
                Item { r, depth, order, s }
            })
            .collect();
        // stable sort; strings compare by UTF-16 units like Java compareTo
        items.sort_by(|a, b| {
            b.depth
                .cmp(&a.depth)
                .then(a.order.cmp(&b.order))
                .then_with(|| a.s.encode_utf16().cmp(b.s.encode_utf16()))
        });
        let mut c = Compiled { rules: Vec::new(), by_name: HashMap::new(), others: Vec::new(), max_depth: 0, needs_ctx: false };
        for it in items {
            let key = match &it.r.key {
                Key::Sym(n) => CKey::Sym(n.clone()),
                Key::Qual(n, m) => CKey::Qual(n.clone(), m.clone()),
                Key::Re(p) => match Pat::compile(p) {
                    Some(p) => CKey::Re(p),
                    None => continue,
                },
                Key::Vec(a, b) => {
                    let f = |p: &Part| match p {
                        Part::Name(n) => Some(CPart::Name(n.clone())),
                        Part::Re(n) => Pat::compile(n).map(CPart::Re),
                    };
                    match (f(a), f(b)) {
                        (Some(a), Some(b)) => CKey::Vec(a, b),
                        _ => continue,
                    }
                }
            };
            let idx = c.rules.len() as u32;
            c.max_depth = c.max_depth.max(it.depth);
            match &key {
                CKey::Sym(n) => c.by_name.entry(n.clone()).or_default().push(idx),
                CKey::Qual(_, m) => {
                    c.needs_ctx = true;
                    c.by_name.entry(m.clone()).or_default().push(idx)
                }
                CKey::Re(_) => c.others.push(idx),
                CKey::Vec(..) => {
                    c.needs_ctx = true;
                    c.others.push(idx)
                }
            }
            c.rules.push(CRule { key, specs: it.r.specs.clone() });
        }
        c
    }
}
