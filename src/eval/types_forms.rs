//! S3 special forms: `defprotocol`/`defrecord`/`deftype`/`extend-type`/
//! `extend-protocol`/`new`, plus the two symbol-shaped hooks `(.field x)`
//! and `(Ctor. args)`. Tree-walk tier ONLY -- the compiled tier bails to
//! the tree-walker on every one of these heads (they are unresolvable
//! symbols to it), which is the correct/honest v1: type definitions and
//! suite test bodies are not hot paths. Measured semantics are quoted per
//! form; the probe transcripts live with `crate::types`' module doc.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::env::Env;
use crate::error::{JvmClass, RjError};
use crate::reader::{Form, FormValue, Span};
use crate::types::{ClassVal, InstVal, MethodTable, ProtoDef, TypeDef};
use crate::value::{Keyword, NativeFn, PMap, PVec, Str, Symbol, Value};

use super::{form_as_symbol, Interp};

impl Interp {
    /// `(defprotocol P doc? (m [x] [x y] doc?) ...)` -- interns `P` as the
    /// protocol map (measured key set: :impls :method-builders :method-map
    /// :on :on-interface :sigs :var) and one dispatch fn per method.
    /// Returns the name SYMBOL (measured: real defprotocol expands to
    /// `'~name` last).
    pub(super) fn eval_defprotocol(
        &mut self,
        args: &[Form],
        span: Span,
        env: &Env,
    ) -> Result<Value, RjError> {
        let name = form_as_symbol(args.first().ok_or_else(|| {
            self.err_here(RjError::arity("defprotocol: expected a name".to_string()), span)
        })?)
        .cloned()
        .ok_or_else(|| {
            self.err_here(
                RjError::other("defprotocol: first argument must be a symbol"),
                span,
            )
        })?;

        // Intern the protocol var FIRST so its cell exists: the cell's
        // address is the registry key and the method fns capture it.
        let qualified = self.qualify_def(&name);
        self.globals.set(qualified.clone(), Value::Nil);
        let cell = self.resolve_var_cell(&qualified);
        let key = Arc::as_ptr(&cell) as usize;
        let var_display = format!("#'{}/{}", self.current_ns, name.name);

        // C14 (protocols): mints the protocol's GENERATED-INTERFACE class,
        // same munge `defrecord`/`deftype` use (`eval_deftype_like`'s own
        // comment) -- `clojure.test-clojure.protocols.examples/
        // MarkerProtocol` gets the class `clojure.test_clojure.protocols.
        // examples.MarkerProtocol`, bound BARE (like `import` binds a
        // short name) so a fully-qualified reference resolves with no
        // `:import` needed, matching real Clojure's auto-visible compiled
        // interface. `iface_methods` accumulates `(munged-name,
        // arity-count)` per sig below, for `.getMethods`
        // (`builtins::types::reflect_methods_of`) -- see that fn's doc.
        let munged_ns = self.current_ns.replace('-', "_");
        let iface_full = format!("{munged_ns}.{}", name.name);
        let iface_val = crate::builtins::types::interface_class(&iface_full);
        self.globals.set(Symbol::simple(iface_full.clone()), iface_val);
        let mut iface_methods: Vec<(Str, usize)> = Vec::new();
        // W4B-MESSAGES: see `ProtoDef::declared_methods`'s doc.
        let mut declared_methods: std::collections::HashSet<Str> = std::collections::HashSet::new();
        // W-PROTO: this evaluation's inline-cache identity. Minted BEFORE
        // the sig loop so every dispatch fn built below can capture it
        // alongside its own `method_idx` (its position in declared order,
        // which is what indexes `ProtoIc`'s per-method banks). Both are
        // only ever used to reach the cache -- see `ProtoDef::epoch`.
        let epoch = crate::types::next_proto_epoch();
        let mut method_idx = 0usize;

        let mut sigs = PMap::new();
        for sig in &args[1..] {
            let FormValue::List(items) = &sig.value else {
                // A docstring between name and first sig is tolerated and
                // discarded, like `def`'s.
                if matches!(&sig.value, FormValue::Atom(Value::Str(_))) {
                    continue;
                }
                return Err(self.err_here(
                    RjError::other("defprotocol: expected a method signature list"),
                    sig.span,
                ));
            };
            let msym_form = items.first().ok_or_else(|| {
                self.err_here(
                    RjError::other("defprotocol: method signature must start with a symbol"),
                    sig.span,
                )
            })?;
            let msym = form_as_symbol(msym_form).ok_or_else(|| {
                self.err_here(
                    RjError::other("defprotocol: method signature must start with a symbol"),
                    sig.span,
                )
            })?;
            let mname = msym.name.clone();
            declared_methods.insert(mname.clone());
            // W3d2, measured (`protocols-test`'s "error conditions checked
            // when defining protocols"): naming the same method in two
            // separate signature forms is an error, not a silent
            // last-writer-wins overwrite. Real Clojure's wording verbatim.
            if sigs.get(&Value::Keyword(Keyword::from(&mname))).is_some() {
                return Err(self.err_here(
                    RjError::other(format!(
                        "Function {mname} in protocol {} was redefined. \
                         Specify all arities in single definition.",
                        name.name
                    )),
                    sig.span,
                ));
            }
            let mut arglists = PVec::new();
            let mut min_arity = usize::MAX;
            let mut max_arity = 0usize;
            // Measured (`compat/proto2-probe.clj`): a trailing string atom
            // after the arglist vectors is the method's `:doc`, same
            // "docstring, discarded from arglist parsing but captured for
            // var meta" shape `def`'s own `docstring` param uses.
            let mut method_doc: Option<Value> = None;
            for arg_form in &items[1..] {
                match &arg_form.value {
                    FormValue::Vector(params) => {
                        min_arity = min_arity.min(params.len());
                        max_arity = max_arity.max(params.len());
                        let mut plist = PVec::new();
                        for p in params {
                            if let Some(ps) = form_as_symbol(p) {
                                plist.push_back(Value::Sym(ps.clone()));
                            }
                        }
                        arglists.push_back(Value::Vector(plist));
                    }
                    FormValue::Atom(Value::Str(s)) => method_doc = Some(Value::Str(s.clone())),
                    _ => {
                        return Err(self.err_here(
                            RjError::other(
                                "defprotocol: method arglist must be a vector of symbols",
                            ),
                            arg_form.span,
                        ))
                    }
                }
            }
            // Measured: `(^String baz [a] [a b] "doc")`'s `^String` reads
            // as metadata on the METHOD NAME symbol form (`msym_form`,
            // like any other `^Tag sym` reader shape) -- pull `:tag` back
            // out of it exactly the way `publish_var_meta` does for a
            // plain `def`'s name symbol, defaulting to `nil` (measured:
            // real Clojure's own generated method-fn meta always has a
            // `:tag` key, `nil` when unhinted, never an absent key).
            let mut tag = match &msym_form.meta {
                Some(meta_form) => match self.eval_meta_form(meta_form, env)? {
                    Value::Map(m) => m
                        .get(&Value::Keyword(Keyword::from("tag")))
                        .cloned()
                        .unwrap_or(Value::Nil),
                    _ => Value::Nil,
                },
                None => Value::Nil,
            };
            // Measured: the reader hands `^String` back as the SHORT bare
            // symbol `String` (`reader.rs`'s own doc), but real Clojure's
            // `defprotocol` resolves that tag against the class table
            // before publishing var meta -- `(:tag (meta (var baz)))` is
            // `'java.lang.String`, not `'String`. Reuse the SAME bare
            // global bindings `import`/`builtin_classes` install (every
            // alias, short or fully-qualified, resolves to the one
            // canonical `Value::Class`) to expand it; a tag that ISN'T a
            // known class alias (a user type not yet defined, or simply
            // absent) passes through unchanged.
            if let Value::Sym(s) = &tag {
                if s.ns.is_none() {
                    if let Some(Value::Class(c)) =
                        self.lookup_global(&Symbol::simple(s.name.clone()))
                    {
                        tag = Value::Sym(Symbol::simple(c.name()));
                    }
                }
            }
            if arglists.is_empty() || min_arity == 0 {
                return Err(self.err_here(
                    RjError::other(format!(
                        // W3d2, measured (`protocols-test`'s "error
                        // conditions checked when defining protocols"):
                        // real Clojure's wording, reproduced verbatim.
                        "Definition of function {mname} in protocol {} must take at least one arg.",
                        name.name
                    )),
                    sig.span,
                ));
            }
            iface_methods.push((Str::from(mname.replace('-', "_")), arglists.len()));
            let arglists_val = Value::List(arglists);
            let mut sig_map = PMap::new();
            sig_map.insert(
                Value::Keyword(Keyword::from("name")),
                Value::Sym(Symbol::simple(mname.clone())),
            );
            sig_map.insert(Value::Keyword(Keyword::from("arglists")), arglists_val.clone());
            sigs.insert(Value::Keyword(Keyword::from(&mname)), Value::Map(sig_map));

            // The dispatch fn: measured lookup rule in
            // `builtins::types::lookup_method`; measured no-impl error
            // shape ("No implementation of method: :m of protocol:
            // #'user/P found for class: java.lang.Long", class "nil" for
            // nil).
            let ic_midx = method_idx;
            method_idx += 1;
            let dispatch = proto_dispatch_native(mname.clone(), var_display.clone(), Str::from(iface_full.clone()), name.name.clone(), (min_arity, max_arity), cell.clone(), epoch, ic_midx);
            let method_qualified = self.qualify_def(&Symbol::simple(mname.clone()));
            self.globals.set(method_qualified.clone(), Value::Native(Arc::new(dispatch)));
            // Measured (`compat/proto2-probe.clj`, mirrors real Clojure
            // 1.13.0-alpha6): a protocol method's generated fn var gets
            // EXACTLY these six meta keys -- `:ns`/`:protocol` point back
            // at the DEFINING namespace/protocol var (not wherever a
            // caller happens to be), `:tag`/`:doc` default to `nil`
            // rather than being absent. `:ns` uses the same interned
            // namespace-instance `find-ns` hands back (`crate::ns::
            // ns_value`), not a bare symbol -- `protocols-test`'s `are`
            // block compares this map against `(merge {:ns (find-ns
            // ..)} ..)` with plain `=`, which would fail on a symbol/
            // instance type mismatch even though both print the same.
            let mut method_meta = PMap::new();
            method_meta.insert(
                Value::Keyword(Keyword::from("name")),
                Value::Sym(Symbol::simple(mname.clone())),
            );
            method_meta.insert(Value::Keyword(Keyword::from("arglists")), arglists_val);
            method_meta.insert(
                Value::Keyword(Keyword::from("doc")),
                method_doc.unwrap_or(Value::Nil),
            );
            method_meta.insert(Value::Keyword(Keyword::from("tag")), tag);
            method_meta.insert(
                Value::Keyword(Keyword::from("protocol")),
                Value::Var(cell.clone()),
            );
            method_meta.insert(
                Value::Keyword(Keyword::from("ns")),
                crate::ns::ns_value(&self.current_ns),
            );
            self.globals
                .intern(&method_qualified)
                .set_var_meta(Value::Map(method_meta));
        }
        crate::builtins::types::register_protocol_iface_methods(
            Str::from(iface_full),
            iface_methods,
        );

        // Register in the shared registry, then fill the protocol map.
        crate::sync::lock_write(&self.protocols.0).insert(
            key,
            ProtoDef {
                var_name: Str::from(var_display),
                impls: HashMap::new(),
                declared_methods,
                epoch,
                // W-PROTO: one empty bank per method actually minted a
                // dispatch fn above, so `ic_midx` always indexes in range
                // for a fn of THIS epoch.
                ic: crate::types::ProtoIc::new(method_idx),
            },
        );
        crate::jit::PROTO_EPOCH.fetch_add(1, std::sync::atomic::Ordering::Release);

        let mut pmap = PMap::new();
        pmap.insert(Value::Keyword(Keyword::from("sigs")), Value::Map(sigs));
        pmap.insert(
            Value::Keyword(Keyword::from("on")),
            Value::Sym(Symbol::simple(format!("{}.{}", self.current_ns, name.name))),
        );
        pmap.insert(Value::Keyword(Keyword::from("on-interface")), Value::Nil);
        pmap.insert(Value::Keyword(Keyword::from("var")), Value::Var(cell.clone()));
        pmap.insert(Value::Keyword(Keyword::from("method-map")), Value::Map(PMap::new()));
        pmap.insert(
            Value::Keyword(Keyword::from("method-builders")),
            Value::Map(PMap::new()),
        );
        pmap.insert(Value::Keyword(Keyword::from("impls")), Value::Nil);
        cell.store(Value::Map(pmap), false);
        Ok(Value::Sym(name))
    }

    /// Shared `defrecord`/`deftype` core. Returns the class value
    /// (measured: both forms return the new class).
    /// `deftype`/`defrecord`/`definterface`'s shared "you cannot use a
    /// name already referred from another namespace" guard (S7 tail wave,
    /// measured: `(definterface String)` throws `IllegalStateException:
    /// String already refers to: class java.lang.String`, same for
    /// `(deftype StringBuffer [])` and `(defrecord Integer [])`).
    /// Deliberately narrow: only a BARE-installed `ClassVal::Builtin`
    /// collides (exactly how `builtins::types::install` binds every
    /// `java.lang`/`clojure.lang` short alias -- see that fn's doc) --
    /// re-running `defrecord`/`deftype` for one's OWN previously-defined
    /// `ClassVal::User` name (`ns_libs.clj`'s `reimporting-deftypes`
    /// deftest does exactly this, twice, for the same `ReimportMe` name)
    /// is unaffected, since a user type is never `ClassVal::Builtin`.
    fn check_name_not_builtin_class(&self, name: &Symbol, span: Span) -> Result<(), RjError> {
        if let Some(Value::Class(c)) = self.globals.get_exact(&Symbol::simple(name.name.clone())) {
            if let ClassVal::Builtin { name: full, .. } = c.as_ref() {
                // W3a: the doc above already recorded the measured class
                // (`IllegalStateException`); it is carried now, not just
                // quoted. Oracle: `(definterface String)` =>
                // `java.lang.IllegalStateException: String already refers
                // to: class java.lang.String in namespace: user` -- raised
                // RAW, not wrapped in a `Compiler$CompilerException`.
                return Err(self.err_here(
                    RjError::other(format!("{} already refers to: class {full}", name.name))
                        .with_class(JvmClass::IllegalState),
                    span,
                ));
            }
        }
        Ok(())
    }

    fn eval_deftype_like(
        &mut self,
        args: &[Form],
        span: Span,
        env: &Env,
        is_record: bool,
    ) -> Result<Value, RjError> {
        let who = if is_record { "defrecord" } else { "deftype" };
        let name = args
            .first()
            .and_then(form_as_symbol)
            .cloned()
            .ok_or_else(|| {
                self.err_here(RjError::other(format!("{who}: expected a name symbol")), span)
            })?;
        self.check_name_not_builtin_class(&name, span)?;
        let fields_form = args.get(1).ok_or_else(|| {
            self.err_here(RjError::other(format!("{who}: expected a field vector")), span)
        })?;
        let FormValue::Vector(field_forms) = &fields_form.value else {
            return Err(self.err_here(
                RjError::other(format!("{who}: fields must be a vector of symbols")),
                fields_form.span,
            ));
        };
        let mut basis = Vec::with_capacity(field_forms.len());
        // C14 (protocols): parallel to `basis`, each field's `^Tag` hint,
        // read back by `RecordName/getBasis` (below) via `(:tag (meta (rb
        // idx)))`. UNLIKE `eval_defprotocol`'s method tags, this is the
        // reader's BARE spelling, verbatim, never resolved to a canonical
        // class name -- measured (`test-statics`'s "basis hinting" sub-
        // test): `(:tag (meta (rbh 0)))` on a `^String a` field is `'String`,
        // not `'java.lang.String`. `getBasis` hands back the SAME basis-
        // symbol forms `defrecord`/`deftype` were written with, unprocessed;
        // `defprotocol` method metadata goes through its own separate
        // resolve step in real Clojure's macro codegen, which is why the
        // two differ.
        let mut field_tags = Vec::with_capacity(field_forms.len());
        // clojure-lsp campaign (mova/PLAN.md): parallel to `basis`/
        // `field_tags` -- `true` where the field carries `^:unsynchron
        // ized-mutable` or `^:volatile-mutable` metadata (either can
        // appear stacked with a `^Tag`, e.g. `^:unsynchronized-mutable
        // ^long s-pos`; `eval_meta_form` below already merges stacked
        // metadata into one map, same mechanism `field_tags` above
        // relies on). See `TypeDef::mutable`'s doc for why this exists.
        let mut mutable = Vec::with_capacity(field_forms.len());
        for f in field_forms {
            let fs = form_as_symbol(f).ok_or_else(|| {
                // W3a/W4B-MESSAGES: measured -- real Clojure's
                // `defrecord`/`deftype` macro asserts this and the
                // `AssertionError` surfaces to user code wrapped in a
                // `clojure.lang.Compiler$CompilerException` ("Syntax error
                // compiling ..", cause "defrecord and deftype fields must
                // be symbols, <ns>.<Name> had: <value>" -- the qualified
                // name is `(str (ns-name *ns*) "." rname)`, UNMUNGED
                // (hyphens intact, not `_`-substituted the way a real
                // generated class name would need to be -- measured
                // directly, see compat/w4b-defrecord-ns-oracle-
                // transcript.txt: `*ns*` genuinely drives this text, not a
                // fixed "user").
                //
                // W3a claimed `user.MyRecord` was unreachable here because
                // ns_libs.clj's regex hardcodes it while the suite runner
                // leaves `*ns*` on the vendored file's OWN namespace
                // (`clojure.test-clojure.ns-libs`) for the rest of the
                // file. Measured again this session (same transcript):
                // that diagnosis holds, and goes one level deeper than a
                // message-wording gap. Real `require`/`load` restore
                // `*ns*` to whatever it was BEFORE loading a file once
                // loading finishes (`(binding [*ns* *ns*] ...)` around the
                // whole load) -- which is why upstream's `run-tests`,
                // called after every test file is already loaded, sees
                // `*ns*` = `user` (or whatever the outer REPL/script
                // started as) regardless of which ns any given deftest was
                // DEFINED in. mova's own top-level `(ns X)` has no such
                // load-scoped restore -- it switches `current_ns`
                // permanently for the rest of whatever script is running,
                // suite-scratch files included. Matching `user.MyRecord`
                // here would need that restore machinery (a `require`/file-
                // loading architecture change), not a wording change to
                // this one assertion -- out of scope for a message-wording
                // pass, so this reports the SAME shape with the ACTUAL
                // current `*ns*` (honest under this harness's real
                // architecture) rather than fabricating "user".
                // Deliberately NOT `{who}` interpolated: real Clojure's
                // message is the SAME literal "defrecord and deftype"
                // regardless of which macro triggered it (both the
                // defrecord and deftype rows in ns_libs.clj pin the
                // identical leading text) -- `defrecord`/`deftype` share
                // one field-validation helper upstream.
                self.err_here(
                    RjError::other(format!(
                        "defrecord and deftype fields must be symbols, {}.{} had: {}",
                        self.current_ns,
                        name.name,
                        crate::printer::pr_str(&crate::reader::form_to_value(f))
                    ))
                    .with_class(JvmClass::CompilerException),
                    f.span,
                )
            })?;
            basis.push(fs.name.clone());
            let (tag, is_mutable) = match &f.meta {
                Some(meta_form) => match self.eval_meta_form(meta_form, env)? {
                    Value::Map(m) => {
                        let tag = m
                            .get(&Value::Keyword(Keyword::from("tag")))
                            .cloned()
                            .unwrap_or(Value::Nil);
                        let truthy = |k: &str| {
                            !matches!(
                                m.get(&Value::Keyword(Keyword::from(k))),
                                None | Some(Value::Nil) | Some(Value::Bool(false))
                            )
                        };
                        (tag, truthy("unsynchronized-mutable") || truthy("volatile-mutable"))
                    }
                    _ => (Value::Nil, false),
                },
                None => (Value::Nil, false),
            };
            field_tags.push(tag);
            mutable.push(is_mutable);
        }

        // S5: split the body into `(head, method-clause forms)` groups
        // BEFORE minting the `TypeDef`, because the type has to KNOW which
        // interfaces it implements at construction time (`TypeDef::
        // interfaces` drives `instance?`). Only the group HEADS are
        // evaluated here; method bodies stay unevaluated `Form`s and are
        // turned into closures further down, so nothing observable moved
        // relative to the pre-S5 order -- a method body's own symbols
        // resolve at CALL time in the tree-walker, not now.
        let groups = self.split_impl_groups(&args[2..], env)?;
        // clojure-lsp campaign (mova/PLAN.md): an `Object`-headed group
        // (`(defrecord R [...] Object (toString [this] ...))`) now gets
        // the SAME "java.lang.Object" pseudo-interface name `definterface`
        // impls already use -- see the `is_object_class` arm in the impl
        // loop below for why this makes `.toString`/`.equals`/`.hashCode`
        // (and therefore `str`, which calls `.toString` on any `Value::
        // Inst`) actually reach a user override instead of silently
        // discarding it.
        let interfaces: Vec<Str> = groups
            .iter()
            .filter_map(|(head, _)| {
                if is_object_class(head) {
                    Some(Str::from("java.lang.Object"))
                } else if let Some(name) = known_builtin_interface_name(head) {
                    Some(name)
                } else {
                    interface_name_of(head)
                }
            })
            .collect();

        // Measured class-name munge: ns dots stay, ns dashes -> `_`
        // (real class of clojure.test-clojure.protocols' R is
        // clojure.test_clojure.protocols.R).
        let munged_ns = self.current_ns.replace('-', "_");
        let full_name = format!("{}.{}", munged_ns, name.name);
        let tdef = Arc::new(TypeDef {
            name: Str::from(full_name.clone()),
            basis: basis.clone(),
            is_record,
            interfaces,
            field_tags,
            mutable,
            // A named type's impls live in the shared registries, keyed
            // by this `TypeDef`'s identity -- only `reify` uses the
            // per-type table (see `TypeDef::methods`' doc).
            methods: MethodTable::new(),
            protocols: Vec::new(),
        });
        let class_val = Value::Class(Arc::new(ClassVal::User(tdef.clone())));

        // `(def R <class>)` + positional/map factories.
        self.globals.set(self.qualify_def(&name), class_val.clone());
        // C14 (protocols): ALSO bind the fully-munged dotted class name
        // bare (like `import`'s short-name binding, but the FULL
        // spelling) -- `(clojure.test_clojure.protocols.R. 1 2)` and
        // ctor-literal desugaring (`reader.rs`'s `read_ctor_vector_
        // literal`/`read_ctor_map_literal`) both need this symbol to
        // resolve with no `:import` required, matching real Clojure's
        // auto-visible compiled class.
        self.globals.set(Symbol::simple(full_name.clone()), class_val.clone());
        {
            let tdef = tdef.clone();
            let ctor_name = format!("->{}", name.name);
            let ctor = record_ctor_native(tdef.clone(), ctor_name.clone());
            let ctor_qualified = self.qualify_def(&Symbol::simple(ctor_name.clone()));
            self.globals.set(ctor_qualified.clone(), Value::Native(Arc::new(ctor)));
            // Measured: `->Name`'s var gets a non-nil `:doc` and an
            // `:arglists` of the basis field names -- `test-record-
            // factory-fns`'s own check is loose (`(false? (-> ->R var
            // meta :doc nil?))`), so exact wording is out of scope, only
            // "present" matters.
            let mut ctor_meta = PMap::new();
            let mut al = PVec::new();
            let mut params = PVec::new();
            for f in &basis {
                params.push_back(Value::Sym(Symbol::simple(f.clone())));
            }
            al.push_back(Value::Vector(params));
            ctor_meta.insert(Value::Keyword(Keyword::from("arglists")), Value::List(al));
            ctor_meta.insert(
                Value::Keyword(Keyword::from("doc")),
                Value::Str(Str::from(format!(
                    "Positional factory function for class {full_name}."
                ))),
            );
            ctor_meta.insert(
                Value::Keyword(Keyword::from("name")),
                Value::Sym(Symbol::simple(ctor_name)),
            );
            ctor_meta.insert(
                Value::Keyword(Keyword::from("ns")),
                crate::ns::ns_value(&self.current_ns),
            );
            self.globals
                .intern(&ctor_qualified)
                .set_var_meta(Value::Map(ctor_meta));
        }
        // C14 (protocols): `RecordName/getBasis` -- a real Clojure
        // `defrecord`/`deftype` compiles a static `getBasis()` returning
        // the field-name vector, tags riding along via `field_tags`
        // (measured: `test-statics`'s own oracle probe). Registered under
        // BOTH the short bare spelling (`RecordName/getBasis`, reachable
        // once the type is defined in the same file/ns) and the full
        // munged-class spelling (`ns.pkg.RecordName/getBasis`, what the
        // suite actually calls) -- same two-spelling registration
        // `/create` below uses, for the same reason.
        {
            let get_basis = get_basis_native(tdef.clone());
            let get_basis_val = Value::Native(Arc::new(get_basis));
            self.globals.set(
                Symbol {
                    ns: Some(Str::from(name.name.clone())),
                    name: Str::from("getBasis"),
                },
                get_basis_val.clone(),
            );
            self.globals.set(
                Symbol {
                    ns: Some(Str::from(full_name.clone())),
                    name: Str::from("getBasis"),
                },
                get_basis_val,
            );
        }
        if is_record {
            let tdef = tdef.clone();
            let map_ctor_name = format!("map->{}", name.name);
            let map_ctor = map_ctor_native(tdef.clone(), map_ctor_name.clone());
            let map_ctor_val = Value::Native(Arc::new(map_ctor));
            let map_ctor_qualified = self.qualify_def(&Symbol::simple(map_ctor_name.clone()));
            self.globals.set(map_ctor_qualified.clone(), map_ctor_val.clone());
            let mut map_ctor_meta = PMap::new();
            let mut al = PVec::new();
            al.push_back(Value::Vector(crate::pvec![Value::Sym(Symbol::simple("m"))]));
            map_ctor_meta.insert(Value::Keyword(Keyword::from("arglists")), Value::List(al));
            map_ctor_meta.insert(
                Value::Keyword(Keyword::from("doc")),
                Value::Str(Str::from(format!(
                    "Factory function for class {full_name}, taking a map of keywords to field values."
                ))),
            );
            map_ctor_meta.insert(
                Value::Keyword(Keyword::from("name")),
                Value::Sym(Symbol::simple(map_ctor_name)),
            );
            map_ctor_meta.insert(
                Value::Keyword(Keyword::from("ns")),
                crate::ns::ns_value(&self.current_ns),
            );
            self.globals
                .intern(&map_ctor_qualified)
                .set_var_meta(Value::Map(map_ctor_meta));
            // C14 (protocols): `RecordName/create` -- measured IDENTICAL
            // to `map->RecordName` (same "fromMap" body; real Clojure's
            // compiled static `create` method and `map->X` fn share one
            // implementation), registered under the same two spellings
            // `getBasis` above uses (`test-statics` calls the short form,
            // `hinting-test` the fully-qualified one).
            self.globals.set(
                Symbol {
                    ns: Some(Str::from(name.name.clone())),
                    name: Str::from("create"),
                },
                map_ctor_val.clone(),
            );
            self.globals.set(
                Symbol {
                    ns: Some(Str::from(full_name.clone())),
                    name: Str::from("create"),
                },
                map_ctor_val,
            );
        }

        // Inline protocol/interface impls (measured: field names are
        // visible as bare symbols inside inline method bodies; params
        // shadow fields).
        for (head, clauses) in groups {
            let methods =
                self.collect_methods_mut(&clauses, span, env, Some(&basis), &tdef.mutable)?;
            match interface_name_of(&head) {
                // S5: an interface's methods are reachable ONLY through
                // `.method` interop (real `definterface` mints no fns and
                // no protocol var), so they go to the interface registry,
                // not the protocol one.
                Some(iface) => crate::builtins::types::register_interface_impls(
                    &self.interfaces,
                    &iface,
                    &tdef,
                    methods,
                ),
                // clojure-lsp campaign (mova/PLAN.md): `Object` is never
                // a protocol (see `is_object_class`'s doc for why it
                // can't go through `register_protocol_impls_inline`),
                // but its methods (`toString`/`equals`/`hashCode`, the
                // ones a defrecord/deftype can actually override) are
                // now filed into the SAME interface-method registry
                // `definterface` impls use, under the fixed pseudo-name
                // `interfaces` (just above) already added to this
                // `TypeDef` for exactly this group -- reachable through
                // ordinary `.method` interop AND through `str`'s own
                // `.toString` dispatch (see `printer`'s `display_str`
                // caller), where a discarded override previously fell
                // back to mova's generic `#ns.R{...}` record dump.
                None if is_object_class(&head) => crate::builtins::types::register_interface_impls(
                    &self.interfaces,
                    &Str::from("java.lang.Object"),
                    &tdef,
                    methods,
                ),
                // clojure-lsp campaign: same routing as the `Object` arm
                // above, for `known_builtin_interface_name`'s fixed list
                // of `clojure.lang.*` marker interfaces that resolve as
                // `ClassVal::Builtin` (a real `is_map`/`is_fn`/... predicate
                // backs their OWN `instance?` semantics) rather than
                // `ClassVal::Interface` -- see that fn's doc.
                None if known_builtin_interface_name(&head).is_some() => {
                    crate::builtins::types::register_interface_impls(
                        &self.interfaces,
                        &known_builtin_interface_name(&head).expect("checked Some above"),
                        &tdef,
                        methods,
                    )
                }
                None => self.register_protocol_impls_inline(&head, &class_val, methods)?,
            }
        }
        Ok(class_val)
    }

    /// S5: `(definterface Name (^long m [x]) ...)` -- registers `Name` as
    /// an interface class in the current namespace and returns that class
    /// (measured: `(definterface IBar (h [x]))` evaluates to the CLASS,
    /// printing `p1.IBar`, and `(class IFoo)` is `java.lang.Class`).
    ///
    /// The method signatures are recorded nowhere on purpose. Real
    /// `definterface` emits a JVM interface whose signatures constrain
    /// nothing a Clojure caller can observe short of reflection:
    /// `definterface` interns no fn, and `.method` interop dispatches on
    /// the METHOD NAME against whatever the implementing type provided.
    /// Arity and type hints are therefore parse-and-discard here -- the
    /// same treatment `def`/`defn` already give docstrings -- rather than
    /// unmeasured surface pretending to enforce something.
    pub(super) fn eval_definterface(
        &mut self,
        args: &[Form],
        span: Span,
        _env: &Env,
    ) -> Result<Value, RjError> {
        let name = args
            .first()
            .and_then(form_as_symbol)
            .cloned()
            .ok_or_else(|| {
                self.err_here(RjError::other("definterface: expected a name symbol"), span)
            })?;
        self.check_name_not_builtin_class(&name, span)?;
        for sig in &args[1..] {
            let FormValue::List(items) = &sig.value else {
                return Err(self.err_here(
                    RjError::other("definterface: expected a method signature list"),
                    sig.span,
                ));
            };
            if items.first().and_then(form_as_symbol).is_none() {
                return Err(self.err_here(
                    RjError::other("definterface: method signature must start with a symbol"),
                    sig.span,
                ));
            }
        }
        // Same munge as `defrecord`/`deftype` above -- a `definterface` in
        // `clojure.test-clojure.protocols.examples` is really
        // `clojure.test_clojure.protocols.examples.ExampleInterface`,
        // which is exactly the spelling that file's own consumer
        // (`protocols.clj`) uses in its `:import` and in a fully-qualified
        // `reify` position.
        let munged_ns = self.current_ns.replace('-', "_");
        let full = format!("{}.{}", munged_ns, name.name);
        let iface = crate::builtins::types::interface_class(&full);
        self.globals.set(self.qualify_def(&name), iface.clone());
        Ok(iface)
    }

    pub(super) fn eval_defrecord(
        &mut self,
        args: &[Form],
        span: Span,
        env: &Env,
    ) -> Result<Value, RjError> {
        self.eval_deftype_like(args, span, env, true)
    }

    pub(super) fn eval_deftype(
        &mut self,
        args: &[Form],
        span: Span,
        env: &Env,
    ) -> Result<Value, RjError> {
        self.eval_deftype_like(args, span, env, false)
    }

    /// `(extend-type Class P (m [x] ..) (m [x y] ..) Q (q [x] ..))`
    pub(super) fn eval_extend_type(
        &mut self,
        args: &[Form],
        span: Span,
        env: &Env,
    ) -> Result<Value, RjError> {
        let class_form = args.first().ok_or_else(|| {
            self.err_here(RjError::other("extend-type: expected a class"), span)
        })?;
        let class_val = self.eval_form_in(class_form, env)?;
        let groups = self.parse_impl_groups(&args[1..], span, env, None)?;
        for (proto_val, methods) in groups {
            self.register_protocol_impls(&proto_val, &class_val, methods)?;
        }
        Ok(Value::Nil)
    }

    /// `(extend-protocol P Class1 (m [x] ..) Class2 (m [x] ..) ...)` --
    /// the inverse grouping of `extend-type`: first symbol is the
    /// protocol, every subsequent non-list form starts a new class group.
    pub(super) fn eval_extend_protocol(
        &mut self,
        args: &[Form],
        span: Span,
        env: &Env,
    ) -> Result<Value, RjError> {
        let proto_form = args.first().ok_or_else(|| {
            self.err_here(RjError::other("extend-protocol: expected a protocol"), span)
        })?;
        let proto_val = self.eval_form_in(proto_form, env)?;
        let mut idx = 1;
        while idx < args.len() {
            let class_val = self.eval_form_in(&args[idx], env)?;
            idx += 1;
            let start = idx;
            while idx < args.len() && matches!(&args[idx].value, FormValue::List(_)) {
                idx += 1;
            }
            let methods = self.collect_methods(&args[start..idx], span, env, None)?;
            self.register_protocol_impls(&proto_val, &class_val, methods)?;
        }
        Ok(Value::Nil)
    }

    /// `(new C a b)` / `(C. a b)` -- positional construction for user
    /// classes (records AND deftypes; measured `(R. 1 2)` prints
    /// like `->R`'s result).
    pub(super) fn eval_new(
        &mut self,
        args: &[Form],
        span: Span,
        env: &Env,
    ) -> Result<Value, RjError> {
        let class_form = args.first().ok_or_else(|| {
            self.err_here(RjError::other("new: expected a class"), span)
        })?;
        let class_val = self.eval_form_in(class_form, env)?;
        let Value::Class(c) = &class_val else {
            return Err(self.err_here(
                RjError::type_err(format!(
                    "new: expected a class, got {}",
                    class_val.type_name()
                )),
                class_form.span,
            ));
        };
        // S5 (host-class shims): `java.util.Random`/`java.util.Date`/
        // `java.lang.ThreadLocal` are BUILTIN classes with real
        // constructor interop -- checked before the `ClassVal::User`-only
        // path below, which still owns every OTHER builtin class (no
        // constructor interop, same error as before this task).
        if let ClassVal::Builtin { name, .. } = c.as_ref() {
            let mut vals = Vec::with_capacity(args.len().saturating_sub(1));
            for a in &args[1..] {
                vals.push(self.eval_form_in(a, env)?);
            }
            return crate::hostclass::construct(self, name, &vals, span);
        }
        // lsp/io (clj-kondo stdin campaign): `java.io.PushbackReader`/
        // `clojure.lang.LineNumberingPushbackReader` are registered as
        // `ClassVal::Interface` (see `types.rs`'s `builtin_interfaces`
        // `ALL` table -- `rewrite-clj.reader`'s own `extend-type` target
        // need, unrelated to this one), NOT `ClassVal::Builtin`, so the
        // arm just above never sees them; `with-in-str`'s macroexpansion
        // still needs `(clojure.lang.LineNumberingPushbackReader. r)` to
        // construct (an identity passthrough, see `hostclass::construct`'s
        // arm for it). Narrowly named rather than opening interface
        // construction generally: only these two reader-wrapper names.
        if let ClassVal::Interface { name } = c.as_ref() {
            if name == "java.io.PushbackReader" || name == "clojure.lang.LineNumberingPushbackReader" {
                let mut vals = Vec::with_capacity(args.len().saturating_sub(1));
                for a in &args[1..] {
                    vals.push(self.eval_form_in(a, env)?);
                }
                return crate::hostclass::construct(self, name.as_ref(), &vals, span);
            }
        }
        let ClassVal::User(tdef) = c.as_ref() else {
            return Err(self.err_here(
                RjError::other(format!(
                    "new: cannot construct builtin class {} (no constructor interop)",
                    c.name()
                )),
                class_form.span,
            ));
        };
        // S5 (measured): a RECORD's generated constructor has a second
        // arity, basis + 2 -- a metadata map and an "extension" map of
        // non-basis keys. `(TestRecord. 1 2 {:m 1} {:c 4})` prints
        // `#p2.TestRecord{:a 1, :b 2, :c 4}` with `(meta r)` => `{:m 1}`,
        // is `=` to `(assoc (TestRecord. 1 2) :c 4)`, and `(keys r)` is
        // `(:a :b :c)`; passing `nil nil` yields a plain record with nil
        // meta. `deftype` gets no such arity, and neither does the
        // `->Name` positional factory (measured: `(->TestRecord 1 2 {}
        // {})` throws "Wrong number of args (4)"), which is why this
        // lives here in `new` and not in the factory built by
        // `eval_deftype_like`.
        let basis_n = tdef.basis.len();
        let given = args.len() - 1;
        let extended = tdef.is_record && given == basis_n + 2;
        if given != basis_n && !extended {
            return Err(self.err_here(
                RjError::arity(format!(
                    "new {}: expected {}{} args, got {}",
                    tdef.name,
                    basis_n,
                    if tdef.is_record {
                        format!(" or {}", basis_n + 2)
                    } else {
                        String::new()
                    },
                    given
                )),
                span,
            ));
        }
        let mut vals = Vec::with_capacity(given);
        for a in &args[1..] {
            vals.push(self.eval_form_in(a, env)?);
        }
        // W3d2: same primitive-hint constraint the `->R`/`map->R`
        // factories enforce -- `(RecordToTestLongHint. "")` and the
        // `#..RecordToTestLongHint[""]` ctor literal (which desugars to
        // exactly that form) both route through here.
        check_field_tags(tdef, &vals[..basis_n.min(vals.len())])
            .map_err(|e| self.err_here(e, span))?;
        if !extended {
            return Ok(make_instance(tdef, &vals));
        }
        let inst = make_instance(tdef, &vals[..basis_n]);
        let Value::Inst(inst) = inst else {
            unreachable!("make_instance always builds an Inst")
        };
        // A non-map (measured with `nil`) in either slot simply means
        // "none" -- no error, matching the JVM ctor's `null` handling.
        let meta = match &vals[basis_n] {
            Value::Map(m) if !m.is_empty() => Some(m.clone()),
            _ => None,
        };
        let mut data = inst.data.clone();
        if let Value::Map(ext) = &vals[basis_n + 1] {
            for (k, v) in ext.iter() {
                data.insert(k.clone(), v.clone());
            }
        }
        let fields_snapshot = crate::sync::lock_mutex(&inst.fields).clone();
        Ok(Value::Inst(Arc::new(InstVal {
            tdef: inst.tdef.clone(),
            data,
            fields: Mutex::new(fields_snapshot),
            meta,
        })))
    }

    /// K5: `Ir::New` fast-path gate: the user type `class_form` names iff `(new C ..n args)` is plain positional construction.
    pub(crate) fn new_fast_class(&mut self, class_form: &Form, env: &Env, n: usize) -> Option<Value> {
        match self.eval_form_in(class_form, env) {
            Ok(v @ Value::Class(_)) if matches!(&v, Value::Class(c) if matches!(c.as_ref(), ClassVal::User(t) if t.basis.len() == n)) => Some(v),
            _ => None,
        }
    }

    /// K5: `Ir::New` fast path proper -- `eval_new`'s non-extended tail.
    pub(crate) fn new_fast_make(&mut self, class: &Value, vals: &[Value], span: Span) -> Result<Value, RjError> {
        let Value::Class(c) = class else { unreachable!("new_fast_make: not a class") };
        let ClassVal::User(tdef) = c.as_ref() else { unreachable!("new_fast_make: not a user class") };
        check_field_tags(tdef, vals).map_err(|e| self.err_here(e, span))?;
        Ok(make_instance(tdef, vals))
    }

    /// `(Ctor. a b)` head hook -- strips the trailing dot and defers to
    /// `new` semantics.
    pub(super) fn eval_ctor_form(
        &mut self,
        head_name: &str,
        args: &[Form],
        span: Span,
        env: &Env,
    ) -> Result<Value, RjError> {
        let bare = &head_name[..head_name.len() - 1];
        let mut full = Vec::with_capacity(args.len() + 1);
        full.push(Form {
            meta: None,
            value: FormValue::Atom(Value::Sym(Symbol::simple(bare))),
            span,
        });
        full.extend_from_slice(args);
        self.eval_new(&full, span, env)
    }

    /// `(.field x)` / `(.-field x)` / -- S5 -- `(.method x arg...)` head
    /// hook: field access on records and deftypes, plus interface-method
    /// dispatch for a type that declared the method's interface in its
    /// `defrecord`/`deftype` body. Interop against anything that is NOT a
    /// mova instance stays an unresolved-symbol error (honest: mova has
    /// no JVM).
    ///
    /// Precedence, when the spelling is `.name` with no extra args: FIELD
    /// first, interface method second. `.-name` is field-only (that is
    /// what the `.-` spelling means in Clojure), and any call with extra
    /// args can only be a method. No vendored form has a basis field and
    /// an interface method sharing a name, so field-first is a stable
    /// tie-break, not a measured claim -- and it is the ordering that
    /// keeps `wrap_fields_let`'s generated `(.f this)` field reads exact.
    /// D5: `(. target member arg...)` and `(. target (member arg...))` --
    /// Clojure's canonical interop special form. Both spellings mean
    /// exactly what the sugar means, so this REWRITES rather than
    /// reimplements: there is one interop implementation in mova and this
    /// form routes into it, which is the only way the two spellings can
    /// be guaranteed not to drift apart.
    ///
    /// Two destinations, matching Clojure's own rule:
    ///   - `target` names a CLASS (`(. clojure.lang.Var
    ///     (pushThreadBindings m))`) -> the STATIC call `(Class/member
    ///     arg...)`.
    ///   - anything else -> the instance call `(.member target arg...)`.
    ///
    /// The class test is on the SYMBOL's resolved value, not its
    /// spelling, so a local shadowing a class name behaves like the local
    /// -- same precedence ordinary symbol resolution already has.
    /// `(. target -field)` reaches `.-field`, the field-access sugar.
    pub(super) fn eval_dot_special(
        &mut self,
        args: &[Form],
        span: Span,
        env: &Env,
    ) -> Result<Value, RjError> {
        let [target, rest @ ..] = args else {
            return Err(self.err_here(
                RjError::other(". : expected a target and a member"),
                span,
            ));
        };
        // `(. x (m a b))` -- unwrap the member list into the flat shape.
        let (member_form, call_args): (&Form, &[Form]) = match rest {
            [only] => match &only.value {
                FormValue::List(items) if !items.is_empty() => (&items[0], &items[1..]),
                _ => (only, &[]),
            },
            [m, a @ ..] => (m, a),
            [] => {
                return Err(self.err_here(RjError::other(". : expected a member name"), span));
            }
        };
        let Some(member) = form_as_symbol(member_form) else {
            return Err(self.err_here(
                RjError::other(". : member name must be a symbol"),
                member_form.span,
            ));
        };
        // Static call, when the target is a class-valued symbol.
        if let Some(target_sym) = form_as_symbol(target) {
            if matches!(self.eval_form_in(target, env), Ok(Value::Class(_))) {
                let static_sym = Symbol {
                    ns: Some(target_sym.name.clone()),
                    name: member.name.clone(),
                };
                let mut items = vec![Form {
                    meta: None,
                    value: FormValue::Atom(Value::Sym(static_sym)),
                    span: member_form.span,
                }];
                items.extend(call_args.iter().cloned());
                return self.eval_form_in(
                    &Form { meta: None, value: FormValue::List(items), span },
                    env,
                );
            }
        }
        // `-field` becomes `.-field` (field access); a plain name becomes
        // `.name` (method call) -- `eval_dot_form` parses both prefixes.
        let head = format!(".{}", member.name);
        let mut dot_args = vec![target.clone()];
        dot_args.extend(call_args.iter().cloned());
        self.eval_dot_form(&head, &dot_args, span, env)
    }

    pub(super) fn eval_dot_form(
        &mut self,
        head_name: &str,
        args: &[Form],
        span: Span,
        env: &Env,
    ) -> Result<Value, RjError> {
        let field_only = head_name.starts_with(".-");
        let field = head_name
            .strip_prefix(".-")
            .or_else(|| head_name.strip_prefix('.'))
            .expect("caller checked the dot prefix");
        if args.is_empty() {
            return Err(self.err_here(
                RjError::other(format!(
                    "Unable to resolve symbol: {head_name} (interop needs a target)"
                )),
                span,
            ));
        }
        // S5 (host-class shims): `(.method obj a b ...)` -- ANY arg count
        // -- on a `java.util.Random`/`java.util.Date`/`Thread`/`proxy
        // [ThreadLocal]` instance. Checked FIRST (target evaluated once,
        // here) so it never falls into the record-field/interface path
        // below; every non-`HostInst` target reuses that already-evaluated
        // value instead of re-evaluating `args[0]`.
        // clojure-lsp campaign (mova/PLAN.md): `.field`/`.method` interop
        // must see through a `Value::Meta` wrapper -- on the real JVM,
        // metadata never changes an object's class, so `(.tag (with-meta
        // seq-node {...}))` dispatches exactly like the unwrapped node.
        // Every match below keyed on `target`'s variant (`Inst`, `Str`,
        // `Map`, `HostInst`, ...) would otherwise see `Value::Meta` and
        // miss, falling through to "Unable to resolve symbol: .field" --
        // measured via rewrite-clj's parser, which wraps EVERY parsed
        // node in row/col metadata (`reader/read-with-meta`) before any
        // protocol method's generated `(.field this)` field-getter sugar
        // (`wrap_fields_let`) ever runs against it.
        let target = self.eval_form_in(&args[0], env)?.into_unmeta();
        // C10: the universal-`Object`-method fallback (`.equals`/
        // `.hashCode`/`.getName`/`.size`/`.iterator`, defined further
        // below) ALSO covers a `HostInst` receiver -- measured,
        // `(.size (java.util.HashMap. {:a 1 :b 2}))` is `2` on the real
        // JVM (`java.util.Map` declares `size()` even though it doesn't
        // extend `Collection`) -- checked BEFORE the `HostInst` early
        // return below, which otherwise unconditionally dispatches to
        // `hostclass::call_method` and never falls through to it. None of
        // the host classes' own methods (`.put`, `Random`/`Date`/
        // `ThreadLocal`'s surface, ...) are named any of these four, so
        // this can never shadow a real one.
        // D5: the receiver's OWN method wins over the universal fallback.
        // `universal_object_dot_method` answers `.size` structurally (as
        // `count`), which is right for a vector or a record but wrong for
        // a `proxy`/`reify`/`deftype` that OVERRIDES `size` -- and
        // `java.util.List`'s `size` is exactly such an override. Before
        // this guard the fallback shadowed the override and `(.size
        // (proxy [java.util.List] [] (size [] 7)))` died trying to seq
        // the proxy. Checked with the same lookup the dispatch below
        // uses, so the two can never disagree.
        let overridden = match &target {
            Value::Inst(inst) => crate::builtins::types::lookup_interface_method(
                &self.interfaces,
                inst,
                field,
            )
            .is_some(),
            _ => false,
        };
        if !field_only
            && !overridden
            && matches!(
                field,
                "equals" | "hashCode" | "getName" | "size" | "iterator" | "isArray"
            )
        {
            if let Some(result) = self.universal_object_dot_method(field, &target, args, env)? {
                return Ok(result);
            }
        }
        if let Value::HostInst(h) = &target {
            let mut vals = Vec::with_capacity(args.len() - 1);
            for a in &args[1..] {
                vals.push(self.eval_form_in(a, env)?);
            }
            return crate::hostclass::call_method(self, h, field, &vals, span);
        }
        // D5: `(.addMethod multifn dispatch-val f)` -- see
        // `builtins::multi`'s `--multifn-add-method` for why this method
        // exists at all. A multimethod is a `Value::Native` (S6, see
        // `builtins::types`' `MultiFn` note), so this arm is keyed on the
        // method name and validated by the delegate, which errors cleanly
        // if the receiver is an ordinary native rather than a multimethod.
        // D5: `(.start m)`/`(.end m)` on a `re-matcher` -- the two
        // `java.util.regex.Matcher` accessors the vendored
        // `clojure.pprint` uses (its format-directive parser advances
        // through a format string with `(subs s (.end m))`). See
        // `MatcherState::last_span` for the char-offset contract and why
        // "no match" throws here exactly where `re-groups` throws.
        if !field_only && matches!(field, "start" | "end") && args.len() == 1 {
            if let Value::Matcher(m) = &target {
                let guard = crate::sync::lock_mutex(m);
                let (start, end) = guard.last_span.ok_or_else(|| {
                    self.err_here(RjError::other(format!(".{field}: No match found")), span)
                })?;
                return Ok(Value::Int(if field == "start" { start } else { end } as i64));
            }
        }
        // clojure-lsp campaign (mova/PLAN.md): `.matcher`/`.matches`/
        // `.group` -- see `builtins::regex::dot_matcher`'s doc for why
        // (`clojure.tools.reader.impl.commons`'s number parser, a
        // transitive dependency of `rewrite-clj.reader`).
        if !field_only && field == "matcher" && matches!(target, Value::Regex(_)) && args.len() == 2 {
            let s = self.eval_form_in(&args[1], env)?;
            return crate::builtins::regex::dot_matcher(&target, &s).map_err(|e| self.err_here(e, span));
        }
        if !field_only && field == "matches" && matches!(target, Value::Matcher(_)) && args.len() == 1 {
            return crate::builtins::regex::dot_matches(&target).map_err(|e| self.err_here(e, span));
        }
        if !field_only && field == "group" && matches!(target, Value::Matcher(_)) && args.len() == 2 {
            let idx = self.eval_form_in(&args[1], env)?;
            let Value::Int(idx) = idx else {
                return Err(self.err_here(
                    RjError::type_err(format!(".group: expected an int index, got {}", idx.type_name())),
                    span,
                ));
            };
            return crate::builtins::regex::dot_group(&target, idx).map_err(|e| self.err_here(e, span));
        }
        // (`args` is `[target, dispatch-val, f]`: the receiver plus two.)
        if !field_only && field == "addMethod" && args.len() == 3 {
            let mut vals = Vec::with_capacity(3);
            vals.push(target.clone());
            for a in &args[1..] {
                vals.push(self.eval_form_in(a, env)?);
            }
            return crate::builtins::multi::multifn_add_method(self, &vals)
                .map_err(|e| self.err_here(e, span));
        }
        // SPEC-W1 task 5: `(.dispatchFn mm)` and `(.getMethod mm dv)` --
        // the two remaining `clojure.lang.MultiFn` instance methods,
        // added for the same reason `.addMethod` above was:
        // `clojure.spec.alpha`'s `multi-spec-impl` calls them directly on
        // a deref'd multimethod var (`#(let [^clojure.lang.MultiFn mm
        // @mmvar] (and (.getMethod mm ((.dispatchFn mm) %)) (mm %)))`),
        // and `defmethod` cannot express "the dispatch value this
        // argument would take". Same receiver shape and same
        // validate-in-the-delegate discipline as `.addMethod`: a
        // multimethod is a `Value::Native`, so the arm keys on the method
        // name and `multi_key_or_err` inside the delegate rejects an
        // ordinary native cleanly.
        //
        // `.getMethod` IS `get-method` (identical contract: best method
        // for the dispatch value, falling back to the `:default` one,
        // `nil` if neither) and `.dispatchFn` returns the fn `defmulti`
        // was given, verbatim.
        if !field_only && field == "dispatchFn" && args.len() == 1 {
            return crate::builtins::multi::multifn_dispatch_fn(self, &target)
                .map_err(|e| self.err_here(e, span));
        }
        if !field_only && field == "getMethod" && args.len() == 2 {
            let dispatch_val = self.eval_form_in(&args[1], env)?;
            return crate::builtins::multi::multifn_get_method(self, ".getMethod", &target, &dispatch_val)
                .map_err(|e| self.err_here(e, span));
        }
        // SPEC-W3: `(.shiftLeft big n)` -- see
        // `builtins::numbers::biginteger_shift_left`'s doc for why
        // test.check's `gen/simple-type` cannot generate without it. It
        // sits here, beside `.getMethod`, rather than in the
        // `numeric_dot_method` call below, purely because it takes an
        // argument and that helper's signature does not. A non-bignum
        // receiver falls THROUGH (the helper answers `None`) to the
        // ordinary field/method path, so nothing else changes shape.
        if !field_only && field == "shiftLeft" && args.len() == 2 {
            let distance = self.eval_form_in(&args[1], env)?;
            if let Some(r) = crate::builtins::numbers::biginteger_shift_left(&target, &distance) {
                return r.map_err(|e| self.err_here(e, span));
            }
        }
        if field_only && args.len() != 1 {
            return Err(self.err_here(
                RjError::other(format!(
                    "Unable to resolve symbol: {head_name} (field access takes exactly one target)"
                )),
                span,
            ));
        }
        // S5 (SPEC-numtower): the numeric tower's one host method,
        // `(.toBigInteger 5N)` -- measured to return a
        // `java.math.BigInteger`, which is a genuinely different class
        // from the `clojure.lang.BigInt` receiver. Checked before the
        // record/deftype field path below so it costs nothing for any
        // non-numeric target. (`target` was already evaluated once, above,
        // for the `HostInst` dispatch -- reuse it, never re-evaluate.)
        if let Some(v) = crate::builtins::numbers::numeric_dot_method(field, &target) {
            return Ok(v);
        }
        // S6 (strdot): `(.startsWith s "a")`-shaped instance methods on a
        // `Value::Str` receiver -- see `builtins::strings::str_dot_method`'s
        // doc for the exact JVM-shape deviations from the bare
        // `clojure.string`-ish builtins it delegates to (`.indexOf`
        // returning `-1` not `nil`, `.split`'s pattern always being a
        // regex, `.replace` always being literal, ...). Checked after the
        // numeric-tower arm above (same "already evaluated once" reuse of
        // `target`) and before the `Inst` field/interface path below, since
        // a string is never an `Inst`. `field_only` (`.-length`) is NOT a
        // valid string field access on the JVM -- `String` has no public
        // instance fields, `.length` etc. are all methods -- measured:
        // `(.-length "abc")` throws "No matching field found", so that
        // shape goes straight to an error instead of the method table.
        if let Value::Str(s) = &target {
            if field_only {
                return Err(self.err_here(
                    RjError::other(format!("no field {field} on java.lang.String")),
                    span,
                ));
            }
            let mut vals = Vec::with_capacity(args.len() - 1);
            for a in &args[1..] {
                vals.push(self.eval_form_in(a, env)?);
            }
            return match crate::builtins::strings::str_dot_method(field, s, &vals) {
                Some(Ok(v)) => Ok(v),
                Some(Err(e)) => Err(e.with_span(span)),
                // W4B-MESSAGES (errors.clj's `compile-error-examples`,
                // the `.jump` row): a method name `str_dot_method`
                // doesn't recognize at ALL (as opposed to one it knows
                // but was called with the wrong arg count -- that's
                // `Some(Err(..))` above, already its own message) is NOT
                // an unresolved-symbol condition on the real JVM --
                // `String` is a perfectly good reflectable class, `jump`
                // just isn't one of its methods, and reflective dispatch
                // reports that as `IllegalArgumentException: "No
                // matching method <name> found taking <n> args for
                // class <class>"` (measured against the oracle,
                // compat/w4b-method-arity-oracle-transcript.txt). mova's
                // own dot-dispatch for strings genuinely IS a reflection
                // table (`str_dot_method`'s match on `field`), so this
                // reports the same real absence the JVM does, worded the
                // way it words it -- not a fabricated condition.
                None => Err(self.err_here(
                    RjError::type_err(format!(
                        "No matching method {field} found taking {} args for class java.lang.String",
                        vals.len()
                    )),
                    span,
                )),
            };
        }
        // S7: the four READ methods `java.util.Map$Entry`/`IMapEntry`
        // expose on a native map entry -- measured, `(.key (first {:a 1}))`
        // and `(.getKey ..)` are `:a`, `(.val ..)`/`(.getValue ..)` are
        // `1`. Same "already evaluated once" reuse of `target` as the
        // numeric/string arms above.
        //
        // Deliberately these FOUR and no more. Real `MapEntry` also has
        // `.setValue` (which throws `UnsupportedOperationException`),
        // `.count`, `.nth`, `.seq`, ...; none appears in the vendored
        // suite, and inventing unmeasured interop surface is what the
        // design directive ("`java.*` names are a veneer, never JVM
        // emulation deeper than the suite demands") rules out. They fall
        // to the same unresolved-symbol error every other unimplemented
        // dot-method does.
        if let Value::MapEntry(items) = &target {
            if !field_only && args.len() == 1 {
                match field {
                    "key" | "getKey" => return Ok(items[0].clone()),
                    "val" | "getValue" => return Ok(items[1].clone()),
                    _ => {}
                }
            }
        }
        // clj-kondo campaign: `(.sym k)` on a `clojure.lang.Keyword` --
        // `clj_kondo.impl.utils/kw->sym` (`(defn kw->sym [^clojure.lang.Keyword
        // k] (.sym k))`) calls this directly. Real `Keyword.sym()` returns
        // the underlying `Symbol` with the SAME ns/name split (`(.sym
        // :ns/foo)` is `ns/foo`, a symbol) -- `symbol_from_str` is the same
        // ns/name splitter `name`/`namespace`'s own `Keyword` arms already
        // use, so this agrees with both. Previously unhandled here, `target`
        // fell all the way through to the generic `Value::Inst` check below
        // and raised "Unable to resolve symbol: .sym" for every call,
        // unconditionally -- not a race, just a missing dot-method arm that
        // clj-kondo's parallel analyzer happens to reach for some files and
        // not others, which is what made it look like a `:parallel`-only
        // flake.
        if let Value::Keyword(k) = &target {
            if !field_only && field == "sym" && args.len() == 1 {
                return Ok(Value::Sym(crate::builtins::strings::symbol_from_str(k)));
            }
        }
        // C14 (protocols): `.equals` on a plain `Value::Map` receiver --
        // `defrecord-acts-like-a-map`'s last two assertions call it with
        // a RECORD on the other side (`set/rename-keys`/`merge-with`'s
        // results). Measured: `.equals` here is `java.util.Map`'s
        // CLASS-AGNOSTIC contract (same key/value pairs, regardless of
        // the other side's concrete class), genuinely DIFFERENT from
        // `=`/`values_equal` -- `(.equals {:a 1 :b 2} (R. 1 2))` is
        // `true` on the real JVM even though `(= {:a 1 :b 2} (R. 1 2))`
        // is `false` (records' own `equals` is type-strict; `Map.equals`
        // is not). So this compares the two sides' entries directly
        // rather than delegating to `values_equal`.
        if let Value::Map(m) = &target {
            if !field_only && field == "equals" {
                if args.len() != 2 {
                    return Err(self.err_here(
                        RjError::arity(format!("equals: wrong number of args ({})", args.len() - 1)),
                        span,
                    ));
                }
                let other = self.eval_form_in(&args[1], env)?;
                let other_map = match &other {
                    Value::Map(om) => Some(om.clone()),
                    Value::Inst(inst) if inst.tdef.is_record => Some(inst.data.clone()),
                    Value::HostStruct(hs) => Some(crate::host_struct::as_pmap(hs).clone()),
                    Value::LazyMap(hs) => Some(crate::lazy_map::as_pmap(hs).clone()),
                    _ => None,
                };
                return Ok(Value::Bool(other_map.is_some_and(|om| *m == om)));
            }
        }
        // C10: a narrow universal-`Object`-method fallback -- `.equals`/
        // `.hashCode` on whatever target shape isn't `Inst` (`Map`/`Set`/
        // `SortedMap`/`SortedSet`/`StructMap`/the `java.util.HashMap`/
        // `HashSet` veneer/a `Value::Class`/`Vector`/`List`/... all
        // included), plus `.getName` on a `Class` and `.size` (== `count`)
        // -- `data_structures.clj`'s `is-same-collection` helper needs all
        // four, unconditionally, on maps/sets/queues/sequences/vectors
        // alike. Checked BEFORE the `Vector`/`TypedVec`/`List`/`VecSeq`
        // block below on purpose: that block unconditionally errors on an
        // unmatched field (never falls through), so `.hashCode`/`.size`/
        // `.getName` on a vector would never reach a later fallback; for
        // `.equals` specifically this duplicates (never conflicts with)
        // `vecdot::vec_dot_method`'s own arm -- both compute the exact
        // same `values_equal`. Checked after every OTHER more specific
        // arm (`HostInst`/numeric/`Str`/`MapEntry`'s four read methods)
        // and unconditionally excludes `Inst` targets, so a `deftype`/
        // `defrecord` still gets its own `.equals`/`.hashCode` if it
        // declares one via `Object`/`IHashEq` (none in this suite's scope
        // do; `Inst` targets simply never reach this block, falling to
        // the ordinary field/interface-method path below instead).
        //
        // `.equals` is exactly `values_equal` (the SAME relation `=`
        // uses) -- real Clojure's persistent collections implement
        // `Object.equals` in terms of `equiv`, measured identical on
        // every pair this suite tries. `.hashCode` reuses the exact
        // computation the `hash` builtin exposes (`sorted::hash_value`,
        // C10-fixed so a realized lazy seq hashes as its content) --
        // deliberately NOT a bit-exact port of Java's `List.hashCode`/
        // `Set.hashCode`/`Map.hashCode` (see `hash_value`'s own doc): the
        // ONE call site in scope only ever compares it RELATIVELY (`(=
        // (.hashCode a) (.hashCode b))` for two `=`-equal collections),
        // which this satisfies for free because it's already the same
        // relation `hash`/`=` agree on.
        if !field_only && !matches!(target, Value::Inst(_)) {
            if let Some(result) = self.universal_object_dot_method(field, &target, args, env)? {
                return Ok(result);
            }
        }
        // C7 (vecveneer): `(.rseq v)`/`(.containsKey v ..)`/`(.equals a
        // b)`/... instance methods on a `Vector`/`TypedVec` receiver, on
        // the SEQ of one (`(seq v)`, a plain `List`), or on the two
        // vector-derived `VecSeq` shapes (`.rseq`'s `RSeq`,
        // `.chunkedNext`'s `Chunked`) -- see `builtins::vecdot`'s module
        // doc for the full measured method table. None of these are field
        // access (`.-length` on a vector isn't a thing on the real JVM
        // either -- `Vec`/`PersistentVector` have no public instance
        // fields, only methods), same `field_only` rejection shape the
        // `Str` arm above uses. Checked after `Str` AND after the C10
        // universal-`Object`-method fallback above (never overlaps: a
        // vector/list/VecSeq is never a `Str`; `.equals` on one of these
        // four is answered identically by either arm) and before the
        // `Inst` field/interface path below, since none of these four are
        // ever `Inst` either.
        // C10: unwraps a `^Tag`/`with-meta` wrapper for the PURPOSES of
        // this guard and the call below -- measured root cause of
        // `ireduce-reduced`'s `(.reduce ^clojure.lang.IReduce (list 1 2 3
        // 4 5) f)` throwing "unresolved symbol" even though the SAME call
        // without the type hint works: `target` was `Value::Meta(List)`,
        // which matched neither this `matches!` guard nor `vec_items`/the
        // `List` arm inside `vec_dot_method`, so the whole block was
        // skipped as if the receiver were some unrelated type. Every
        // other arm in this fn either already sees through `Meta` (the
        // C10 universal fallback just above, `values_equal`, `uncons`) or
        // is never reached with a `Meta` receiver at all (`Str`/
        // `HostInst`/numbers can't carry metadata in mova); this is the
        // one dot-dispatch gap where it mattered.
        let vec_target = target.unmeta();
        if matches!(vec_target, Value::Vector(_) | Value::TypedVec(_) | Value::List(_) | Value::VecSeq(_)) {
            if field_only {
                return Err(self.err_here(
                    RjError::other(format!("no field {field} on {}", target.type_name())),
                    span,
                ));
            }
            let mut vals = Vec::with_capacity(args.len() - 1);
            for a in &args[1..] {
                vals.push(self.eval_form_in(a, env)?);
            }
            if let Some(result) = crate::builtins::vecdot::vec_dot_method(self, field, vec_target, &vals, span) {
                return result;
            }
            // Falls through to the same "unresolved symbol" error as
            // every other unimplemented dot-method (matches `Str`'s own
            // `None` arm above) -- NOT an early return, so `Inst`-typed
            // targets (impossible here, `target` is one of the four
            // matched types) never reach this branch at all.
            return Err(self.err_here(
                RjError::unresolved(format!("Unable to resolve symbol: {head_name}")),
                span,
            ));
        }
        // Wave-C small sweep item 5 (errors.clj's `ex-info-arities-
        // construct-equivalent-exceptions`): `ex-info` (core.mova)
        // represents Clojure's `ExceptionInfo` as a plain `Value::Map`
        // under `:ex/`-namespaced keys (see that fn's own doc), so the
        // three real `ExceptionInfo`/`IExceptionInfo` accessor methods
        // the vendored suite calls directly on an `ex-info` result --
        // `.getMessage`/`.getData`/`.getCause` -- need their own narrow
        // arm here. NOT a generic `java.util.Map` veneer: gated on the
        // map actually carrying `:ex/message` (i.e. being an `ex-info`
        // result), and only these three method names; every other
        // `Value::Map` dot-call still falls through to the unresolved-
        // symbol error below, same as before this arm existed.
        if let Value::Map(m) = &target {
            if !field_only
                && args.len() == 1
                && m.get(&Value::Keyword(Keyword::from("ex/message"))).is_some()
            {
                let got = match field {
                    "getMessage" => Some("ex/message"),
                    "getData" => Some("ex/data"),
                    "getCause" => Some("ex/cause"),
                    _ => None,
                };
                if let Some(key) = got {
                    return Ok(m.get(&Value::Keyword(Keyword::from(key))).cloned().unwrap_or(Value::Nil));
                }
            }
        }
        // SPEC-W1 task 3: the same three accessors on a CAUGHT INTERNAL
        // ERROR's info map (`eval::special_forms::error_to_info_map`'s
        // `{:type :error/<kind> :message ..}`), which `instance?
        // Throwable` now answers `true` for -- see `hostclass::
        // is_exception_named`'s doc. The pairing is load-bearing, not
        // cosmetic: `clojure.spec.alpha`'s `validate-fn` reads
        // `(.getMessage ^Throwable ret)` on the very value its
        // `(instance? Throwable ret)` test just accepted, so an
        // `instance?` that says yes and a `.getMessage` that says
        // "Unable to resolve symbol" would be a worse asymmetry than the
        // one task 3 removed. `.getData`/`.getCause` are `nil` on these
        // (an internal error carries neither), which is exactly what a
        // real `RuntimeException` with no ex-data and no cause answers.
        if let Value::Map(m) = &target {
            if !field_only && args.len() == 1 && crate::hostclass::is_error_info_map(m) {
                match field {
                    "getMessage" => {
                        return Ok(m
                            .get(&Value::Keyword(Keyword::from("message")))
                            .cloned()
                            .unwrap_or(Value::Nil))
                    }
                    "getData" | "getCause" => return Ok(Value::Nil),
                    _ => {}
                }
            }
        }
        // C14 (protocols): `(.getMethods c)` on a protocol's GENERATED-
        // INTERFACE class -- `method-names`' helper in `protocols.clj`
        // (`marker-tests`/`protocols-test`). See `builtins::types::
        // reflect_methods_of`'s doc for the full veneer shape; anything
        // that isn't a KNOWN protocol interface (a plain `definterface`,
        // a builtin class, ...) falls through to the ordinary unresolved-
        // symbol error below, same as every other unimplemented
        // dot-method.
        if let Value::Class(c) = &target {
            if field == "getMethods" && !field_only && args.len() == 1 {
                if let Some(v) = crate::builtins::types::reflect_methods_of(c.name()) {
                    return Ok(v);
                }
            }
        }
        // W3f (small-tail sweep): `.comparator` on a `SortedMap`/`SortedSet`
        // receiver -- `clojure_walk.clj`'s `walk` test asserts `(=
        // (.comparator c) (.comparator walked))` for a `sorted-set-by`/
        // `sorted-map-by` collection round-tripped through `w/walk identity
        // identity`. `walk`'s `(into (empty form) ..)` path already clones
        // `SortedMapVal`/`SortedSetVal`'s `cmp` (see `"empty"`'s own arm in
        // `builtins::collections`), so a `Comparator::Fn` receiver returns
        // the SAME `Value` (an `Arc`-shared closure) both times and `=`
        // agrees for free. `Comparator::Default` has no real JVM-visible
        // analog exercised anywhere in this suite (no in-scope call site
        // ever inspects `.comparator` on a plain `sorted-map`/`sorted-set`)
        // -- `nil`, matching this codebase's never-deeper-than-measured
        // policy rather than inventing a `RT$DefaultComparator` stand-in.
        if let Value::SortedMap(m) = &target {
            if field == "comparator" && !field_only && args.len() == 1 {
                return Ok(match &m.cmp {
                    crate::value::Comparator::Fn(f) => f.clone(),
                    crate::value::Comparator::Default => Value::Nil,
                });
            }
        }
        if let Value::SortedSet(s) = &target {
            if field == "comparator" && !field_only && args.len() == 1 {
                return Ok(match &s.cmp {
                    crate::value::Comparator::Fn(f) => f.clone(),
                    crate::value::Comparator::Default => Value::Nil,
                });
            }
        }
        // S7 (tail wave): `.bindRoot` on a `Var` receiver -- measured
        // (rt.clj's `binding-root-clears-macro-metadata`): sets the var's
        // ROOT value (bypassing any thread `binding` frame, same as
        // `VarCell::store` already does for ordinary `def`) and, if the
        // var's metadata carries a truthy `:macro` key, strips it --
        // real `clojure.lang.Var.bindRoot` does exactly that (a var is
        // only a macro while ITS root is the special-cased macro fn it
        // was `defmacro`d with; rebinding the root to something else
        // un-macros it). No other `Var` instance method is in scope here.
        if let Value::Var(cell) = &target {
            if !field_only && args.len() == 2 && field == "bindRoot" {
                let new_root = self.eval_form_in(&args[1], env)?;
                cell.store(new_root, false);
                if let Value::Map(m) = cell.var_meta() {
                    if m.get(&Value::Keyword(Keyword::from("macro"))).is_some() {
                        let mut m2 = m.clone();
                        m2.remove(&Value::Keyword(Keyword::from("macro")));
                        cell.set_var_meta(Value::Map(m2));
                    }
                }
                return Ok(Value::Nil);
            }
        }
        // S7 (tail wave): `.toString` on a `java.util.UUID` value --
        // measured, `(.toString (java.util.UUID/randomUUID))` is the same
        // string `str`/`pr-str` already print (parse.clj's
        // `test-parse-uuid` needs exactly this one method; no other UUID
        // instance method is in scope, same narrow-veneer reasoning as the
        // `MapEntry`/`Str` dot-method arms above).
        if let Value::Uuid(_) = &target {
            if !field_only && args.len() == 1 && field == "toString" {
                return Ok(Value::Str(crate::printer::display_str(&target).into()));
            }
        }
        // S7 (tail wave): `.get`/`.getAsBoolean`/`.getAsInt`/`.getAsLong`/
        // `.getAsDouble` on an `Atom`/`Delay` receiver -- see
        // `builtins::conc::supplier_dot_method`'s doc. Checked before the
        // `Inst` fallthrough below (neither `Atom` nor `Delay` is ever an
        // `Inst`), same "already evaluated once" reuse of `target` as the
        // string/vector arms above.
        if matches!(target, Value::Atom(_) | Value::Delay(_)) && !field_only && args.len() == 1 {
            if let Some(result) = crate::builtins::conc::supplier_dot_method(self, field, &target) {
                return result.map_err(|e| e.with_span(span));
            }
        }
        // D5: the `java.io.StringWriter` surface on an `Atom` receiver --
        // see the `java.io.StringWriter` row in `types::builtin_classes()`
        // for why a StringWriter IS an atom holding its accumulated text.
        // Exactly the four methods the vendored `clojure.pprint` calls on
        // one, and no more: `.write` (a String, or an int char code --
        // `column_writer.clj` and `cl_format.clj`'s case-converting
        // proxies both pass `(int c)`), `.toString`, `.flush` (nothing to
        // flush; a no-op that must still succeed), `.append`. Placed
        // after the `Supplier` arm above so `.get` keeps its existing
        // meaning on an atom, and before the `Inst` fallthrough for the
        // same reason that arm is: an `Atom` is never an `Inst`.
        if let Value::Atom(cell) = &target {
            if !field_only {
                if let Some(result) =
                    self.string_writer_dot_method(cell.clone(), field, &args[1..], env)
                {
                    return result.map_err(|e| e.with_span(span));
                }
            }
        }
        let Value::Inst(inst) = &target else {
            // Reflection on a value that has no such member: the JVM's
            // `IllegalArgumentException`, not an unresolved symbol.
            let cls = match crate::builtins::types::class_of(&target) {
                Value::Class(c) => c.name().to_string(),
                _ => "java.lang.Object".to_string(),
            };
            let msg = if args.len() == 1 {
                format!("No matching field found: {} for class {cls}", field.trim_start_matches('-'))
            } else {
                format!("No matching method {field} found taking {} args for class {cls}", args.len() - 1)
            };
            return Err(self.err_here(
                RjError::type_err(msg).with_class(crate::error::JvmClass::IllegalArgument),
                span,
            ));
        };
        // `Throwable.printStackTrace()`: the JVM writes the trace to System.err
        // (a process stream, not `*err*`). Mova has no JVM frames: the header only.
        if field == "printStackTrace" && !field_only && args.len() == 1 && crate::printer::inst_is_throwable(inst) {
            eprintln!("{}", crate::printer::display_str(&target));
            return Ok(Value::Nil);
        }
        if args.len() == 1 {
            if let Some(v) = inst_field(inst, field) {
                return Ok(v);
            }
        }
        if field_only {
            return Err(self.err_here(
                RjError::other(format!("no field {field} on {}", inst.tdef.name)),
                span,
            ));
        }
        // Extra args evaluated ONCE here (same convention as every other
        // arm above), reused below by whichever of the two dispatch
        // attempts (C14 record veneer, then interface method) claims
        // `field`.
        let mut vals = Vec::with_capacity(args.len().saturating_sub(1));
        for a in &args[1..] {
            vals.push(self.eval_form_in(a, env)?);
        }
        // C14 (protocols): `(.size rec)`/`.isEmpty`/`.containsKey`/
        // `.containsValue`/`.get`/`.put`/`.remove`/`.putAll`/`.clear`/
        // `.keySet`/`.values`/`.entrySet`/`.equals`/`.cons` -- the
        // `java.util.Map`+`IPersistentCollection` surface `defrecord-
        // interfaces-test`/`defrecord-acts-like-a-map`/`degenerate-
        // defrecord-test` call on a RECORD directly (never a plain
        // `deftype`, which has no map-like `data`). Checked BEFORE the
        // interface-method table below, but AFTER `inst_field`/
        // `field_only` above -- see `builtins::recorddot`'s module doc
        // for the exact measured table and why records specifically
        // (`deftype`'s `Inst` has no `data` map to view this way).
        if inst.tdef.is_record {
            if let Some(result) =
                crate::builtins::recorddot::record_dot_method(self, field, &target, &vals, span)
            {
                return result;
            }
        }
        // C3c (errors.clj's `Throwable->map-test` "nil stack handled"
        // sub-test): `.setStackTrace` on any of our synthetic exception
        // instances -- a documented no-op, not a stored field. mova has
        // no real stack-trace capture at all (`Throwable->map`'s `:trace`
        // is unconditionally `[]`, `core/core.mova`'s own doc), so
        // accepting and discarding the array here is honest: nothing
        // downstream could observe a difference between "captured then
        // overwritten with empty" and "never captured", and the test's
        // own comment explicitly frames the call as "simulate what can
        // happen when Java omits stack traces" -- it never reads the
        // trace back off `e` afterward, only off `(Throwable->map t)`.
        if field == "setStackTrace" && vals.len() == 1 {
            return Ok(Value::Nil);
        }
        // W4-veneer (test.clj's `clj-1102-empty-stack-trace-should-not-
        // throw-exceptions`): `.getStackTrace` on any of our synthetic
        // exception instances -- symmetric with `.setStackTrace` above,
        // and for the same reason: mova captures no real per-frame stack
        // trace at all (see that arm's doc), so an ALWAYS-empty
        // `StackTraceElement[]` is the honest answer regardless of any
        // prior `.setStackTrace` call (which already discards its
        // argument rather than storing it). This is exactly the value
        // real Clojure's `(.getStackTrace exception)` would need to
        // return for THIS deftest's `t` to make `clojure.test/file-and-
        // line`'s `(if (< depth (count stacktrace)) ...)` take its
        // `false` branch (depth 0, count 0) and never touch a
        // `StackTraceElement`'s `.getFileName`/`.getLineNumber` (neither
        // implemented -- see `types::builtin_classes()`'s
        // `StackTraceElement` row, a pure marker with no real members).
        // Same "Object"-kind empty array shape `hostclass::call_thread_
        // method`'s own `.getStackTrace` arm already uses for `Thread/
        // currentThread`.
        if field == "getStackTrace" && vals.is_empty() {
            return Ok(Value::Array(Arc::new(crate::value::ArrayVal {
                kind: crate::value::ArrayKind::Object("java.lang.Object"),
                dims: 1,
                data: std::sync::Mutex::new(Vec::new()),
            })));
        }
        // C3c (rt.clj's `ns-intern-policies`): `(.refer ns sym var)` --
        // see `builtins::nsfns::refer_dot_method`'s doc for the full
        // policy (warn-and-replace vs. reject). Checked here, ahead of
        // `lookup_interface_method` (a `Namespace` value declares no
        // interfaces at all, so it would otherwise always fall through
        // to the generic "no field or interface method" error this
        // exact deftest was hitting before this task).
        if field == "refer" && vals.len() == 2 {
            if let Some(result) = crate::builtins::nsfns::refer_dot_method(self, &target, &vals, span) {
                return result;
            }
        }
        let Some(f) =
            crate::builtins::types::lookup_interface_method(&self.interfaces, inst, field)
        else {
            // W3d2, oracle-measured: on an ANONYMOUS type (`reify`/
            // `proxy` -- the ones carrying their own method table), an
            // unimplemented interface method is `AbstractMethodError`
            // ("Receiver class ... does not define or inherit an
            // implementation of the resolved method ..."), which is an
            // `Error`, not a `RuntimeException`. `reify-test`'s
            // "unimplemented methods" row asserts exactly that class, and
            // the shim checks classes now. A NAMED `deftype`/`defrecord`
            // keeps the generic message: real Clojure rejects `(.foo r)`
            // on a named type at COMPILE time (`No matching method`), a
            // different condition this arm has never claimed to model.
            // Error path only; costs one `is_empty` on a call that is
            // already failing.
            let err = if inst.tdef.methods.is_empty() {
                RjError::other(format!(
                    "no field or interface method {field} on {}",
                    inst.tdef.name
                ))
            } else {
                RjError::other(format!(
                    "{} does not define or inherit an implementation of {field}",
                    inst.tdef.name
                ))
                .with_class(crate::error::JvmClass::AbstractMethod)
            };
            return Err(self.err_here(err, span));
        };
        // Measured shape: the instance is the method's first argument
        // (`(.f t 5)` on `(deftype T [a] IFoo (f [_ x] (+ a x)))` with
        // `(T. 10)` => 15).
        let mut call_args = Vec::with_capacity(vals.len() + 1);
        call_args.push(target.clone());
        call_args.extend(vals);
        self.apply_value(&f, &call_args, span)
    }

    /// C10: the universal-`Object`-method fallback -- `.equals`/
    /// `.hashCode` on whatever target shape isn't `Inst` (`Map`/`Set`/
    /// `SortedMap`/`SortedSet`/`StructMap`/the `java.util.HashMap`/
    /// `HashSet`/`ArrayList` veneer/a `Value::Class`/`Vector`/`List`/...
    /// all included), `.getName` on a `Class`, `.size` (== `count`,
    /// measured to work even on a `java.util.HashMap` -- real `java.util.
    /// Map` declares `size()` despite not extending `Collection`), and
    /// `.iterator` -- `data_structures.clj`'s `is-same-collection`/
    /// `seq-iter-match` helpers need all five, unconditionally, on maps/
    /// sets/queues/sequences/vectors/the `java.util.*` veneer alike.
    ///
    /// Returns `Ok(None)` (not an error) for an unmatched field/arity so
    /// callers can keep falling through their own more specific arms --
    /// see the two call sites: BEFORE the `HostInst` early-return in
    /// `eval_dot_form` (that branch otherwise unconditionally dispatches
    /// to `hostclass::call_method` and never reaches here), and again
    /// after `Str`/`MapEntry`'s narrower arms but BEFORE `Vector`/
    /// `TypedVec`/`List`/`VecSeq`'s own block (which unconditionally
    /// errors on an unmatched field, never falling through on its own).
    ///
    /// `.equals` is exactly `values_equal` (the SAME relation `=` uses)
    /// -- real Clojure's persistent collections implement `Object.equals`
    /// in terms of `equiv`, measured identical on every pair this suite
    /// tries. `.hashCode` reuses the exact computation the `hash` builtin
    /// exposes (`sorted::hash_value`, C10-fixed so a realized lazy seq
    /// hashes as its content) -- deliberately NOT a bit-exact port of
    /// Java's `List.hashCode`/`Set.hashCode`/`Map.hashCode` (see
    /// `hash_value`'s own doc): every call site in scope only ever
    /// compares it RELATIVELY (`(= (.hashCode a) (.hashCode b))` for two
    /// `=`-equal collections), which this satisfies for free because it's
    /// already the same relation `hash`/`=` agree on. `.iterator` builds
    /// a `HostKind::Iterator` cursor from a SNAPSHOT `seq` of the target
    /// (see `mk_iterator`'s doc); `nil` (an empty/no-seq target) becomes
    /// an immediately-exhausted iterator, matching `(.hasNext (.iterator
    /// []))` => `false` on the oracle.
    fn universal_object_dot_method(
        &mut self,
        field: &str,
        target: &Value,
        args: &[Form],
        env: &Env,
    ) -> Result<Option<Value>, RjError> {
        match field {
            // C3e: `Object.equals`, not `=` -- class-strict at a numeric
            // leaf (`(.equals 3 3N)` is `false` where `(= 3 3N)` is
            // `true`). See `Interp::values_equal_strict`'s doc for the
            // measured rows and for what is deliberately NOT tightened.
            "equals" if args.len() == 2 => {
                let other = self.eval_form_in(&args[1], env)?;
                Ok(Some(Value::Bool(self.values_equal_strict(target, &other)?)))
            }
            // W4B-WARNINGS scope addendum (control.clj's `test-case`,
            // "test correct behavior on hash collision": `(is (== (.hashCode
            // 1) (.hashCode 9223372039002259457N)))`): `.hashCode` used to
            // fall straight through to `sorted::hash_value`, which is
            // CLOJURE's `hasheq` (Murmur3-mixed for a `long`), not real
            // JVM `.hashCode()` -- the two genuinely differ (measured on
            // the oracle: `(.hashCode 1)` is `1` either way, but a `long`'s
            // REAL `.hashCode()` is `(int)(x ^ (x >>> 32))`, e.g. `(.hashCode
            // -1)` is `0`, not `-1`'s `hasheq`). `jvm_hash_code_for` below
            // reproduces the real algorithm for the two numeric-tower
            // shapes this row exercises (`Value::Int`/`Value::BigInt`);
            // anything else keeps falling through to the prior
            // `hash_value` behavior unchanged (not JVM-exact for OTHER
            // types either, but not this task's mandate, and not what
            // regressed anything to fix).
            "hashCode" if args.len() == 1 => {
                if let Some(h) = jvm_hash_code_for(target) {
                    return Ok(Some(Value::Int(h)));
                }
                Ok(Some(Value::Int(crate::builtins::sorted::hash_value(self, target)?)))
            }
            "getName" if args.len() == 1 => match target {
                Value::Class(c) => Ok(Some(Value::Str(Str::from(c.name())))),
                _ => Ok(None),
            },
            // D5: `(.isArray (class obj))` -- the first branch of
            // vendored `clojure.pprint`'s `pprint-simple-default`, so
            // EVERY value pprint falls back on (keywords, numbers,
            // strings, ...) goes through it. Answered off the class
            // NAME's JVM array spelling (`[Ljava.lang.Long;`,
            // `[D`, ...), which `types::array_jvm_name` is the one
            // producer of, so this stays in sync with `class` by
            // construction rather than by a second table.
            "isArray" if args.len() == 1 => match target {
                Value::Class(c) => Ok(Some(Value::Bool(c.name().starts_with('[')))),
                _ => Ok(None),
            },
            "size" if args.len() == 1 => {
                Ok(Some(Value::Int(crate::builtins::collections::count_value(self, target)?)))
            }
            "iterator" if args.len() == 1 => {
                let items = self.seq_items(target)?;
                let remaining = items.map(Value::List).unwrap_or(Value::Nil);
                Ok(Some(crate::hostclass::mk_iterator(remaining)))
            }
            // W3d2: `java.util.Collection.contains` -- MEMBERSHIP, not
            // `contains?`' key lookup. Oracle-measured: `(.contains [:a :b]
            // :b)` and `(.contains #{:a} :a)` are both `true`, where
            // `(contains? [:a :b] :b)` is `false` (a vector's `contains?`
            // asks about INDICES). `transients.clj`'s `empty-transient`
            // (`(.contains (transient #{}) :bogus-key)`) is the call site;
            // implementing it as membership rather than special-casing sets
            // keeps the vector case honest instead of silently wrong.
            //
            // Deliberately NOT added to `eval_dot_form`'s EARLY gate list
            // (the one running ahead of `HostInst` dispatch): the
            // `java.util.ArrayList`/`HashSet` veneers have their own
            // `.contains`, which must keep winning on their own receivers.
            "contains" if args.len() == 2 => {
                let needle = self.eval_form_in(&args[1], env)?;
                let Some(items) = self.seq_items(target)? else {
                    return Ok(Some(Value::Bool(false)));
                };
                for x in items.iter() {
                    if self.values_equal(x, &needle)? {
                        return Ok(Some(Value::Bool(true)));
                    }
                }
                Ok(Some(Value::Bool(false)))
            }
            _ => Ok(None),
        }
    }

    /// D1: `(reify Head (m [this args] body...) ... Head2 ...)` -- an
    /// anonymous instance implementing interfaces and/or protocols.
    ///
    /// Deliberately built out of the pieces `deftype` already uses, with
    /// exactly one thing subtracted and one added. Subtracted: the basis
    /// (`reify` has no fields, so no `wrap_fields_let`, no factories, no
    /// `def`). Added: the method table rides on the freshly minted
    /// `TypeDef` instead of the shared interface/protocol registries --
    /// see `types::TypeDef::methods`' doc for why that is the only
    /// lifetime-correct choice here.
    ///
    /// One anonymous class per EVALUATION (real Clojure compiles one per
    /// FORM, but its methods close over the enclosing locals, and a
    /// closure is exactly what `collect_methods`/`eval_fn_form` build
    /// here -- so the per-evaluation `TypeDef` IS the closure, and
    /// per-form sharing would be wrong, not merely different). Instances
    /// are ordinary `Value::Inst`s with an empty basis, which is what
    /// makes `=`/`hash` identity-based for them for free (`value.rs`'s
    /// non-record `Inst` arms) -- matching a JVM `reify` that overrides
    /// neither `equals` nor `hashCode`.
    ///
    /// Accepted heads: an interface class (`definterface`-minted or a
    /// `types::builtin_interfaces()` row), any other builtin class
    /// (`java.lang.Object`, which real `reify` also allows), or a
    /// protocol. Method names must be unique ACROSS heads (real Clojure:
    /// "Duplicate method name"); several clauses of the SAME name under
    /// ONE head are the ordinary multi-arity spelling `collect_methods`
    /// already merges.
    ///
    /// Known gap, deliberate: real Clojure also rejects a method name
    /// that is not declared by any listed interface (`protocols.clj`'s
    /// `(reify java.util.List (foo [_]))` row) and demands type hints to
    /// disambiguate same-arity overloads. Both need a JVM method
    /// inventory per interface; mova has none, and inventing one for two
    /// negative assertions is exactly the "deeper than the suite demands"
    /// this module's veneers are bound not to do.
    pub(super) fn eval_reify(
        &mut self,
        args: &[Form],
        span: Span,
        env: &Env,
    ) -> Result<Value, RjError> {
        let mut interfaces: Vec<Str> = Vec::new();
        let mut methods = MethodTable::new();
        let mut protocols: Vec<usize> = Vec::new();
        for (head, clauses) in self.split_impl_groups(args, env)? {
            match &head {
                // Interface OR plain builtin class (`java.lang.Object`):
                // both are named implements-position heads here, and
                // `TypeDef::interfaces` is a name list, so both record
                // the same way.
                Value::Class(c) => interfaces.push(Str::from(c.name())),
                // W3d2: a protocol head now records its registry KEY (see
                // `TypeDef::protocols`) -- D1 deliberately recorded
                // nothing here because no suite assertion asked, but
                // `tests/conformance/pending/records.corpus`'s last row
                // (`(satisfies? P (reify P ..))` => `true` on the oracle)
                // did, and that is the datum it needs.
                Value::Map(_) => {
                    let key = crate::builtins::types::proto_key(&head, "reify")
                        .map_err(|e| self.err_here(e, span))?;
                    if !protocols.contains(&key) {
                        protocols.push(key);
                    }
                }
                other => {
                    return Err(self.err_here(
                        RjError::type_err(format!(
                            "reify: expected an interface, class or protocol, got {}",
                            other.type_name()
                        )),
                        span,
                    ))
                }
            }
            // W3d2: reject a method the head does not declare, for the
            // host heads whose method sets mova KNOWS (oracle-transcribed
            // -- see `types::host_interface_methods`). D1 left this as a
            // deliberate gap on the grounds that it needs a JVM method
            // inventory mova does not have; measuring one off the pinned
            // oracle is what changed, and `reify-test`'s `(reify
            // java.util.List (foo [_]))` row asserts it. Unknown heads
            // (protocols, `definterface`s, every other host type) stay
            // permissive, so nothing legal can be rejected.
            let declared = match &head {
                Value::Class(c) => crate::types::host_interface_methods(c.name()),
                _ => None,
            };
            for (name, f) in self.collect_methods(&clauses, span, env, None)? {
                if let Some(declared) = declared {
                    if !declared.contains(&name.as_ref()) {
                        return Err(self.err_here(
                            RjError::other(format!(
                                "reify: can't define method {name} -- {} declares no such method",
                                match &head {
                                    Value::Class(c) => c.name(),
                                    _ => "this head",
                                }
                            )),
                            span,
                        ));
                    }
                }
                if methods.contains_key(&name) {
                    return Err(self.err_here(
                        RjError::other(format!("reify: duplicate method name: {name}")),
                        span,
                    ));
                }
                methods.insert(name, f);
            }
        }
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // Same class-name munge `eval_deftype_like` documents, with real
        // Clojure's `$reify__N` anonymous-class suffix.
        let name = format!("{}$reify__{n}", self.current_ns.replace('-', "_"));
        Ok(Value::Inst(Arc::new(InstVal {
            tdef: Arc::new(TypeDef {
                name: Str::from(name),
                basis: Vec::new(),
                is_record: false,
                interfaces,
                field_tags: Vec::new(),
            mutable: Vec::new(),
                methods,
                protocols,
            }),
            data: PMap::new(),
            fields: Mutex::new(PVec::new()),
            meta: None,
        })))
    }

    /// D5: `.write`/`.toString`/`.flush`/`.append` on a `java.io.
    /// StringWriter` (an `Atom` holding a string -- see the
    /// `java.io.StringWriter` row in `types::builtin_classes()`).
    /// `None` means "not one of these four methods", so the caller falls
    /// through to its ordinary error path; `Some(Err(..))` is a real
    /// arity/type error on a method that IS one of them.
    ///
    /// Text is appended by the SAME code path `strings::out_write` uses
    /// for a `with-out-str` atom (read, concat, bump the version
    /// counter), deliberately: a StringWriter bound to `*out*` and a
    /// StringWriter written to explicitly with `.write` have to
    /// accumulate into one string in call order, and the only way to
    /// guarantee that is for both to be the same append.
    fn string_writer_dot_method(
        &mut self,
        cell: Arc<crate::value::AtomCell>,
        field: &str,
        arg_forms: &[Form],
        env: &Env,
    ) -> Option<Result<Value, RjError>> {
        let arity = |n: usize| -> Option<Result<Value, RjError>> {
            Some(Err(RjError::arity(format!(
                "java.io.StringWriter/{field}: expected {n} arg(s), got {}",
                arg_forms.len()
            ))))
        };
        match field {
            "toString" => {
                if !arg_forms.is_empty() {
                    return arity(0);
                }
                let guard = crate::sync::lock_mutex(&cell.state);
                Some(Ok(match &guard.1 {
                    Value::Str(s) => Value::Str(s.clone()),
                    _ => Value::Str(Str::from("")),
                }))
            }
            // Real `Writer.flush()` returns void; nothing is buffered
            // behind this atom, so there is genuinely nothing to do.
            "flush" | "close" => {
                if !arg_forms.is_empty() {
                    return arity(0);
                }
                Some(Ok(Value::Nil))
            }
            "write" | "append" => {
                if arg_forms.len() != 1 {
                    return arity(1);
                }
                let v = match self.eval_form_in(&arg_forms[0], env) {
                    Ok(v) => v,
                    Err(e) => return Some(Err(e)),
                };
                // A `Writer.write` overload set accepts a String or an
                // int codepoint; `append` accepts a CharSequence or a
                // char. Both vendored spellings appear, so both are
                // handled by one text-of-the-argument step.
                let text = match v.unmeta() {
                    Value::Str(s) => s.to_string(),
                    Value::Char(c) => c.to_string(),
                    Value::Int(n) => match u32::try_from(*n).ok().and_then(char::from_u32) {
                        Some(c) => c.to_string(),
                        None => {
                            return Some(Err(RjError::type_err(format!(
                                "java.io.StringWriter/{field}: {n} is not a character codepoint"
                            ))))
                        }
                    },
                    other => {
                        return Some(Err(RjError::type_err(format!(
                            "java.io.StringWriter/{field}: expected a string, char or codepoint, got {}",
                            other.type_name()
                        ))))
                    }
                };
                let mut guard = crate::sync::lock_mutex(&cell.state);
                let mut appended = match &guard.1 {
                    Value::Str(existing) => existing.to_string(),
                    _ => String::new(),
                };
                appended.push_str(&text);
                guard.0 = guard.0.wrapping_add(1);
                guard.1 = Value::Str(Str::from(appended));
                Some(Ok(Value::Nil))
            }
            _ => None,
        }
    }

    /// D5: `(proxy [Base IFace...] [ctor-args] (method [args] body...))`
    /// -- an anonymous instance, built on EXACTLY the machinery `reify`
    /// already uses (a per-evaluation `TypeDef` carrying its own method
    /// table, whose closures capture the lexical env), with three
    /// deliberate differences that are the whole of what "proxy" means
    /// here relative to "reify":
    ///
    /// 1. `this` IS IMPLICIT. A `reify` method writes `(m [this x] ...)`;
    ///    a `proxy` method writes `(m [x] ...)` and still refers to
    ///    `this` in its body (upstream `clojure.pprint`'s own `getf`
    ///    macro expands to `(~sym @@~'this)` -- an unqualified,
    ///    deliberately-captured `this`, which is proof enough of the
    ///    binding without a separate oracle probe). Dispatch is shared
    ///    with `reify`/`deftype` and always passes the receiver as arg0,
    ///    so the bridge is to SYNTHESIZE the missing first parameter:
    ///    every arity's param vector gets a literal `this` prepended
    ///    before `collect_methods` ever sees it. Nothing downstream needs
    ///    to know: `wrap_method_recur` already skips param 0 (a method's
    ///    `recur` rebinds arguments but not the receiver), which is
    ///    exactly right here for the same reason.
    /// 2. THE HEADS LIVE IN A VECTOR, and the first one is nominally a
    ///    superCLASS rather than an interface. mova has no class
    ///    hierarchy, so both are recorded identically in
    ///    `TypeDef::interfaces` (a name list) -- which is all `instance?`
    ///    consults. A `proxy [java.io.Writer ...]` is therefore an
    ///    `instance?` of `java.io.Writer` and dispatches `.write` to its
    ///    OWN override, which is the entirety of what the vendored
    ///    `clojure.pprint` writer chain asks of the superclass: every one
    ///    of its five proxy sites overrides every method it ever calls.
    ///    No inherited behavior is emulated, and consequently
    ///    `proxy-super` is NOT implemented -- deliberately, having first
    ///    checked that no vendored `clojure.pprint` source uses it (`grep
    ///    proxy-super` over all eight files: zero hits). It errors with a
    ///    clear message rather than pretending.
    /// 3. CTOR ARGS ARE EVALUATED AND DISCARDED. They exist to feed a
    ///    real superclass constructor; with no superclass there is
    ///    nothing to feed. They are still evaluated, left to right, so
    ///    any side effect in the vector happens exactly when a JVM
    ///    `proxy` would run it. Every vendored call site passes `[]`.
    ///
    /// The pre-existing `(proxy [ThreadLocal] [] (initialValue [] ...))`
    /// keyhole (S5, for `test.check`'s `random.clj`) is kept verbatim as
    /// a leading special case: `ThreadLocal` is a real JVM class with
    /// real inherited `.get`/`.set`/`.remove` behavior driving the
    /// `initialValue` override, i.e. precisely the "inherited behavior"
    /// case the general path above declines to fake, so it keeps its
    /// hand-written `hostclass::mk_threadlocal` value.
    pub(super) fn eval_proxy(
        &mut self,
        args: &[Form],
        span: Span,
        env: &Env,
    ) -> Result<Value, RjError> {
        if args.len() < 2 {
            return Err(self.err_here(
                RjError::other("proxy: expected [class-and-interfaces] [args] fs*"),
                span,
            ));
        }
        let FormValue::Vector(heads) = &args[0].value else {
            return Err(self.err_here(
                RjError::other("proxy: first argument must be a vector of class/interface names"),
                args[0].span,
            ));
        };
        let FormValue::Vector(ctor_args) = &args[1].value else {
            return Err(self.err_here(
                RjError::other("proxy: second argument must be a vector of ctor args"),
                args[1].span,
            ));
        };
        if Self::is_threadlocal_proxy(heads) {
            return self.eval_threadlocal_proxy(&args[1..], span, env);
        }
        // Superclass-constructor arguments: evaluated for effect, then
        // dropped (see this fn's doc, difference 3).
        for a in ctor_args {
            self.eval_form_in(a, env)?;
        }

        let mut interfaces: Vec<Str> = Vec::new();
        for h in heads {
            match self.eval_form_in(h, env)? {
                Value::Class(c) => interfaces.push(Str::from(c.name())),
                other => {
                    return Err(self.err_here(
                        RjError::type_err(format!(
                            "proxy: expected a class or interface, got {}",
                            other.type_name()
                        )),
                        h.span,
                    ))
                }
            }
        }

        // `this`-prepending happens on the raw FORMS, before any of the
        // shared method machinery runs -- see difference 1.
        let clauses: Vec<Form> = args[2..]
            .iter()
            .map(Self::add_implicit_this)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|(msg, at)| self.err_here(RjError::other(msg), at))?;
        // `MethodTable` is keyed by `Str`, whose cached-ASCII/char-count
        // flags are `AtomicU8`s -- interior mutability that never
        // participates in hashing or equality. Same `#[allow]` every
        // other `Str`-keyed map in this crate carries (see `env.rs`'s
        // `Env::interned_namespaces`).
        #[allow(clippy::mutable_key_type)]
        let methods = self.collect_methods(&clauses, span, env, None)?;

        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // Real Clojure names a proxy class `<pkg>.proxy$<Super>$<ifaces
        // hash>`; mova has no superclass to name, so the anonymous-class
        // spelling follows `reify`'s own `$reify__N` convention instead.
        let name = format!("{}$proxy__{n}", self.current_ns.replace('-', "_"));
        Ok(Value::Inst(Arc::new(InstVal {
            tdef: Arc::new(TypeDef {
                name: Str::from(name),
                basis: Vec::new(),
                is_record: false,
                interfaces,
                field_tags: Vec::new(),
            mutable: Vec::new(),
                methods,
                // A `proxy` head vector is a list of CLASSES (including a
                // protocol's generated interface, which is how
                // `protocols-test` proxies a protocol) -- never a protocol
                // MAP, so there is no registry key to record and no
                // `satisfies?` question to answer. Empty, deliberately.
                protocols: Vec::new(),
            }),
            data: PMap::new(),
            fields: Mutex::new(PVec::new()),
            meta: None,
        })))
    }

    /// True for the one `proxy` head vector that keeps its pre-D5
    /// hand-written host value: `[ThreadLocal]`/`[java.lang.ThreadLocal]`.
    fn is_threadlocal_proxy(heads: &[Form]) -> bool {
        heads.len() == 1
            && matches!(
                form_as_symbol(&heads[0]),
                Some(s) if s.ns.is_none()
                    && matches!(s.name.as_ref(), "ThreadLocal" | "java.lang.ThreadLocal")
            )
    }

    /// S5's original `proxy` keyhole, unchanged: `args` is everything from
    /// the ctor-arg vector onward.
    fn eval_threadlocal_proxy(
        &mut self,
        args: &[Form],
        span: Span,
        env: &Env,
    ) -> Result<Value, RjError> {
        let unsupported = |msg: &str, at: Span| {
            RjError::other(format!(
                "proxy: {msg} -- mova implements `(proxy [ThreadLocal] [] \
                 (initialValue [] ...))` and nothing else for ThreadLocal \
                 (see crate::hostclass's module doc)"
            ))
            .with_span(at)
        };
        let FormValue::Vector(ctor_args) = &args[0].value else {
            return Err(unsupported("second argument must be a vector", args[0].span));
        };
        if !ctor_args.is_empty() {
            return Err(unsupported("non-empty ctor-args are not supported", args[0].span));
        }
        let mut init_fn = None;
        for clause in &args[1..] {
            let FormValue::List(items) = &clause.value else {
                return Err(unsupported("expected (method [params] body...) clauses", clause.span));
            };
            let Some(mname) = items.first().and_then(form_as_symbol) else {
                return Err(unsupported("method clause must start with a symbol", clause.span));
            };
            if mname.name.as_ref() != "initialValue" {
                return Err(unsupported("only an `initialValue` override is supported", clause.span));
            }
            if init_fn.is_some() {
                return Err(unsupported("only one `initialValue` clause is supported", clause.span));
            }
            init_fn = Some(self.eval_fn_form(&items[1..], clause.span, env)?);
        }
        let _ = span;
        Ok(crate::hostclass::mk_threadlocal(init_fn))
    }

    /// D5: rewrite one `proxy` method clause so its param vectors gain the
    /// leading `this` that `proxy` leaves implicit (see `eval_proxy`'s
    /// doc, difference 1). Handles both clause spellings `collect_methods`
    /// accepts -- `(m [x] body...)` and `(m ([x] body...) ([x y] body...))`
    /// -- because the second is what `column_writer.clj`'s multi-arity
    /// `write` override is written in. Errors carry their own span.
    fn add_implicit_this(clause: &Form) -> Result<Form, (String, Span)> {
        let FormValue::List(items) = &clause.value else {
            return Err(("proxy: expected a (method [params] body...) clause".into(), clause.span));
        };
        let Some(head) = items.first() else {
            return Err(("proxy: empty method clause".into(), clause.span));
        };
        if form_as_symbol(head).is_none() {
            return Err(("proxy: method clause must start with a symbol".into(), clause.span));
        }
        let this_form = |span: Span| Form {
            meta: None,
            value: FormValue::Atom(Value::Sym(Symbol::simple("this"))),
            span,
        };
        // `(m [params] body...)`: one arity, params in slot 1.
        // `(m ([params] body...) ...)`: several arities, each a list.
        let multi = matches!(items.get(1).map(|f| &f.value), Some(FormValue::List(_)));
        let mut out = vec![head.clone()];
        let prepend = |arity: &Form| -> Result<Form, (String, Span)> {
            let FormValue::List(parts) = &arity.value else {
                return Err(("proxy: expected an ([params] body...) arity list".into(), arity.span));
            };
            let Some((params_form, body)) = parts.split_first() else {
                return Err(("proxy: arity list needs a parameter vector".into(), arity.span));
            };
            let FormValue::Vector(params) = &params_form.value else {
                return Err(("proxy: method parameters must be a vector".into(), params_form.span));
            };
            let mut new_params = vec![this_form(params_form.span)];
            new_params.extend(params.iter().cloned());
            let mut new_parts = vec![Form {
                meta: params_form.meta.clone(),
                value: FormValue::Vector(new_params),
                span: params_form.span,
            }];
            new_parts.extend(body.iter().cloned());
            Ok(Form { meta: None, value: FormValue::List(new_parts), span: arity.span })
        };
        if multi {
            for arity in &items[1..] {
                out.push(prepend(arity)?);
            }
        } else {
            let single = Form {
                meta: None,
                value: FormValue::List(items[1..].to_vec()),
                span: clause.span,
            };
            let FormValue::List(parts) = prepend(&single)?.value else { unreachable!() };
            out.extend(parts);
        }
        Ok(Form { meta: clause.meta.clone(), value: FormValue::List(out), span: clause.span })
    }

    /// S4: `(import spec...)` -- a MACRO in real Clojure (its args are
    /// never evaluated), hence a special form here too. Each arg form is
    /// either bare data (`(java.lang Boolean)`, a package symbol + short
    /// names; or a bare already-dotted symbol like `java.lang.Boolean`) or
    /// an explicit `(quote data)` wrapper -- measured: real `import`'s
    /// macroexpansion strips a leading `quote` off each arg before
    /// treating it as a spec (`clojure.core`'s own source), which is why
    /// `(import 'java.lang.Boolean)` and `(import java.lang.Boolean)` both
    /// work identically; `strip_quote` below reproduces exactly that.
    /// Returns the LAST class processed (measured: `(import '(java.lang
    /// Boolean Long))` => `java.lang.Long`, the list's last entry --
    /// real `import` macroexpands to a `do` of one `import1` per spec, so
    /// its value is simply the last body form's).
    pub(super) fn eval_import(&mut self, args: &[Form], span: Span, _env: &Env) -> Result<Value, RjError> {
        let mut last = Value::Nil;
        for arg in args {
            let spec = strip_quote(arg);
            last = self.import_spec_value(&spec, span)?;
        }
        Ok(last)
    }

    /// One import spec, already a plain `Value` (shared by `eval_import`
    /// above and anything that hands `import` an already-evaluated spec):
    /// a bare symbol (`java.lang.Boolean`, already fully qualified --
    /// mova symbols never split on `.` the way they do on `/`) or a
    /// `(java.lang Boolean Long)`-shaped package-prefix list/vector.
    /// Resolves against `builtins::types::class_by_full_name` (S3's
    /// builtin-class table) ONLY -- mova has no JVM classpath, so anything
    /// outside that table is an honest, measured-shape error (real
    /// Clojure: `ClassNotFoundException` whose message is the bare class
    /// name; mova has no exception-class taxonomy -- see
    /// `tests/conformance/DEVIATIONS.md` -- so this is an ordinary,
    /// catchable `RjError` instead).
    pub(crate) fn import_spec_value(&mut self, spec: &Value, span: Span) -> Result<Value, RjError> {
        let mut last: Option<Value> = None;
        match spec {
            Value::Sym(sym) if sym.ns.is_none() => {
                last = Some(self.import_one(&sym.name, span)?);
            }
            Value::List(items) | Value::Vector(items) => {
                let mut it = items.iter();
                let Some(Value::Sym(pkg)) = it.next() else {
                    return Err(self.err_here(
                        RjError::other("import: expected a package symbol"),
                        span,
                    ));
                };
                for cls in it {
                    let Value::Sym(cls_sym) = cls else {
                        return Err(self.err_here(
                            RjError::other("import: expected a class symbol"),
                            span,
                        ));
                    };
                    let full = format!("{}.{}", pkg.name, cls_sym.name);
                    last = Some(self.import_one(&full, span)?);
                }
            }
            other => {
                return Err(self.err_here(
                    RjError::type_err(format!(
                        "import: expected a symbol or a (package Class...) spec, got {}",
                        other.type_name()
                    )),
                    span,
                ))
            }
        }
        Ok(last.unwrap_or(Value::Nil))
    }

    /// Resolves one fully-qualified class name, binds its SHORT name
    /// (`java.lang.Boolean` -> `Boolean`) into the CURRENT namespace, and
    /// returns the class value.
    /// W4-EVAL task 5: a SECOND lookup source, tried only after the S3
    /// builtin table misses -- `full_name` bound bare as a `Value::Class`
    /// by a live `deftype`/`defrecord` (`eval_deftype_like` above,
    /// `self.globals.set(Symbol::simple(full_name), class_val)`, the SAME
    /// bare cell `(exporter.ReimportMe. 1)`'s own ctor-literal resolution
    /// already reads). Measured (`ns_libs.clj`'s `reimporting-deftypes`
    /// deftest): `(import exporter.ReimportMe)` from another namespace
    /// must resolve exactly that cell, and a LATER re-`defrecord` of the
    /// same name (redefining what the bare cell holds) followed by a
    /// re-`import` must see the CHANGED class -- both fall out for free
    /// from reading the bare cell fresh on every `import_one` call rather
    /// than caching anything, since `Env::set`'s redefinition contract
    /// already keeps that cell's identity stable while swapping its
    /// value. Builtin-table-first, deftype-registry-second: a user type
    /// can never actually collide with an S3 entry in practice (every
    /// `deftype`/`defrecord` full name carries its defining namespace as
    /// a prefix, `java.lang`/`clojure.lang` are not namespaces anything
    /// can `ns` into), so the ordering is unobserved either way -- kept
    /// builtin-first as the conservative default (never let a user type
    /// shadow a real host class spelling) rather than probed further.
    fn import_one(&mut self, full_name: &str, span: Span) -> Result<Value, RjError> {
        let class_val = crate::builtins::types::class_by_full_name(full_name)
            .or_else(|| match self.globals.get_exact(&Symbol::simple(full_name)) {
                Some(v @ Value::Class(_)) => Some(v),
                _ => None,
            })
            .ok_or_else(|| {
                self.err_here(
                    RjError::other(format!(
                        "import: unknown class {full_name} (mova has no JVM classpath -- only the S3 builtin-class table is importable)"
                    )),
                    span,
                )
            })?;
        self.globals.set(
            self.qualify_def(&Symbol::simple(short_class_name(full_name))),
            class_val.clone(),
        );
        Ok(class_val)
    }

    /// Parses `[Proto (m [x] ..) (m [x y] ..) Proto2 (q [x] ..)]` groups
    /// (the `extend-type`/`defrecord`-body shape): a non-list form starts
    /// a new protocol group; lists are method impls, multiple lists with
    /// the same method name merge into one multi-arity fn.
    fn parse_impl_groups(
        &mut self,
        forms: &[Form],
        span: Span,
        env: &Env,
        record_fields: Option<&[Str]>,
    ) -> Result<Vec<(Value, MethodTable)>, RjError> {
        let mut out = Vec::new();
        for (head, clauses) in self.split_impl_groups(forms, env)? {
            let methods = self.collect_methods(&clauses, span, env, record_fields)?;
            out.push((head, methods));
        }
        Ok(out)
    }

    /// S5: the first half of `parse_impl_groups` -- evaluate each group's
    /// HEAD (protocol or interface) and hand back its method clauses
    /// still unevaluated. Split out so `defrecord`/`deftype` can learn
    /// which interfaces a body names before minting the `TypeDef` that
    /// has to record them (see `eval_deftype_like`).
    fn split_impl_groups(
        &mut self,
        forms: &[Form],
        env: &Env,
    ) -> Result<Vec<(Value, Vec<Form>)>, RjError> {
        let mut out = Vec::new();
        let mut idx = 0;
        while idx < forms.len() {
            let head = self.eval_form_in(&forms[idx], env)?;
            idx += 1;
            let start = idx;
            while idx < forms.len() && matches!(&forms[idx].value, FormValue::List(_)) {
                idx += 1;
            }
            out.push((head, forms[start..idx].to_vec()));
        }
        Ok(out)
    }

    /// Builds one fn per method name from `(m [params] body...)` clauses,
    /// merging same-name clauses into a multi-arity fn. When
    /// `record_fields` is set (inline `defrecord` impls), each clause's
    /// body is wrapped in a `let` binding every basis field from the
    /// first param (fields NOT shadowed by params -- measured JVM scoping).
    fn collect_methods(
        &mut self,
        clauses: &[Form],
        span: Span,
        env: &Env,
        record_fields: Option<&[Str]>,
    ) -> Result<MethodTable, RjError> {
        self.collect_methods_mut(clauses, span, env, record_fields, &[])
    }

    /// The mutable-field-aware sibling of [`collect_methods`](Self::
    /// collect_methods) -- see `wrap_fields_let`'s doc. `collect_methods`
    /// forwards here with an empty `mutable_fields` slice so its other
    /// four call sites (none of which ever pass `record_fields` at all,
    /// let alone a MUTABLE one) are unaffected byte-for-byte.
    fn collect_methods_mut(
        &mut self,
        clauses: &[Form],
        span: Span,
        env: &Env,
        record_fields: Option<&[Str]>,
        mutable_fields: &[bool],
    ) -> Result<MethodTable, RjError> {
        let mut by_name: Vec<(Str, Vec<Form>)> = Vec::new();
        for clause in clauses {
            let FormValue::List(items) = &clause.value else {
                return Err(self.err_here(
                    RjError::other("expected a method implementation list"),
                    clause.span,
                ));
            };
            let msym = items.first().and_then(form_as_symbol).ok_or_else(|| {
                self.err_here(
                    RjError::other("method implementation must start with a symbol"),
                    clause.span,
                )
            })?;
            let mname = msym.name.clone();
            // Two measured spellings: `(m [x] body...)` (one arity) and
            // `(m ([x] body...) ([x y] body...))` (several arities in one
            // form). Normalize both into `(params body...)` clause lists.
            let raw_clauses: Vec<Form> =
                if matches!(items.get(1).map(|f| &f.value), Some(FormValue::List(_))) {
                    items[1..].to_vec()
                } else {
                    vec![Form {
                        meta: None,
                        value: FormValue::List(items[1..].to_vec()),
                        span: clause.span,
                    }]
                };
            for arity_clause in raw_clauses {
                // D1: innermost, so a `defrecord`'s field `let` (below)
                // stays OUTSIDE the loop and its fields are read once per
                // call rather than once per `recur`.
                let arity_clause = Self::wrap_method_recur(arity_clause, clause.span);
                let arity_clause = match record_fields {
                    Some(fields) => self.wrap_fields_let(
                        arity_clause,
                        fields,
                        mutable_fields,
                        clause.span,
                    )?,
                    None => arity_clause,
                };
                match by_name.iter_mut().find(|(n, _)| *n == mname) {
                    Some((_, list)) => list.push(arity_clause),
                    None => by_name.push((mname.clone(), vec![arity_clause])),
                }
            }
        }
        let mut table = MethodTable::new();
        for (mname, arity_clauses) in by_name {
            // W3d2: two clauses of one name at the SAME parameter count
            // are not the multi-arity spelling -- they are JVM method
            // OVERLOADS, distinguished by their parameter `^Tag`s
            // (`reify-test`'s `(hinted [_ ^int i])` / `(hinted [_ ^String
            // s])` pair on `ExampleInterface`). `eval_fn_form` cannot
            // represent them: a Clojure `fn` dispatches on arity alone, so
            // the second clause would simply be unreachable.
            //
            // Grafted INTO this one table rather than beside it: the
            // overload set collapses to an ordinary `Value` (a native that
            // picks a variant by runtime argument type), so `MethodTable`
            // stays `name -> Value`, and every consumer --
            // `lookup_interface_method`, `lookup_method`, the protocol
            // `:impls` mirror -- is untouched and unaware. There is still
            // exactly ONE method-dispatch mechanism.
            //
            // Non-overloaded names (every method in the corpus but that
            // one pair) take the byte-identical path they always did.
            let arities: Vec<usize> =
                arity_clauses.iter().map(|c| clause_param_count(c).unwrap_or(usize::MAX)).collect();
            let overloaded = arities
                .iter()
                .enumerate()
                .any(|(i, a)| *a != usize::MAX && arities[..i].contains(a));
            if !overloaded {
                let f = self.eval_fn_form(&arity_clauses, span, env)?;
                table.insert(mname, f);
                continue;
            }
            let mut variants: Vec<(usize, Vec<Option<Str>>, Value)> =
                Vec::with_capacity(arity_clauses.len());
            for ac in &arity_clauses {
                let params = clause_param_count(ac).unwrap_or(0);
                let tags = self.method_param_tags(ac, env)?;
                let f = self.eval_fn_form(std::slice::from_ref(ac), span, env)?;
                variants.push((params, tags, f));
            }
            table.insert(mname.clone(), overload_dispatcher(mname, variants));
        }
        Ok(table)
    }

    /// W3d2: the `^Tag` of each parameter AFTER `this` in a `(params
    /// body...)` method clause, canonicalized to the class's full name
    /// where the tag names a known class (`^String` -> `java.lang.String`,
    /// the same expansion `eval_defprotocol` gives a method tag).
    /// Primitive spellings (`^int`, `^long`, ...) name no class and stay
    /// bare, exactly as the reader produced them. `None` per unhinted
    /// parameter.
    fn method_param_tags(
        &mut self,
        arity_clause: &Form,
        env: &Env,
    ) -> Result<Vec<Option<Str>>, RjError> {
        let FormValue::List(items) = &arity_clause.value else {
            return Ok(Vec::new());
        };
        let Some(Form { value: FormValue::Vector(params), .. }) = items.first() else {
            return Ok(Vec::new());
        };
        let mut tags = Vec::with_capacity(params.len().saturating_sub(1));
        for p in params.iter().skip(1) {
            let Some(meta_form) = &p.meta else {
                tags.push(None);
                continue;
            };
            let Value::Map(m) = self.eval_meta_form(meta_form, env)? else {
                tags.push(None);
                continue;
            };
            let tag = match m.get(&Value::Keyword(Keyword::from("tag"))) {
                Some(Value::Sym(s)) if s.ns.is_none() => s.name.clone(),
                Some(Value::Str(s)) => s.clone(),
                _ => {
                    tags.push(None);
                    continue;
                }
            };
            match self.lookup_global(&Symbol::simple(tag.clone())) {
                Some(Value::Class(c)) => tags.push(Some(Str::from(c.name()))),
                _ => tags.push(Some(tag)),
            }
        }
        Ok(tags)
    }

    /// D1: `recur` inside a `deftype`/`defrecord`/`reify` METHOD body
    /// rebinds the method's arguments but NOT `this` -- measured
    /// (`protocols.clj`'s `reify-test` "methods can recur":
    /// `(reify java.util.List (get [_ index] (if (zero? index) :done
    /// (recur (dec index)))))` recurs with ONE argument for a
    /// two-parameter method). Methods here are ordinary closures whose
    /// first parameter IS `this`, so a bare `recur` would demand that
    /// extra argument; wrapping the body in `(loop [p p ...] body...)`
    /// over the non-`this` parameters retargets `recur` at exactly the
    /// arguments real Clojure rebinds, using machinery that already
    /// exists rather than a second recur protocol.
    ///
    /// Applied ONLY when the body actually mentions `recur` (so no method
    /// pays for a loop frame it never uses) and when every parameter is a
    /// plain symbol (a destructuring parameter has no single name to
    /// rebind; no vendored method both destructures and recurs).
    fn wrap_method_recur(clause: Form, span: Span) -> Form {
        let FormValue::List(items) = &clause.value else { return clause };
        let Some((params_form, body)) = items.split_first() else { return clause };
        let FormValue::Vector(params) = &params_form.value else { return clause };
        if !body.iter().any(mentions_recur) {
            return clause;
        }
        let names: Vec<&Symbol> = params.iter().filter_map(form_as_symbol).collect();
        if names.len() != params.len() || names.iter().any(|s| s.name.as_ref() == "&") {
            return clause;
        }
        let sym_form = |s: &Symbol| Form {
            meta: None,
            value: FormValue::Atom(Value::Sym(s.clone())),
            span,
        };
        let mut bindings: Vec<Form> = Vec::new();
        for p in names.iter().skip(1) {
            bindings.push(sym_form(p));
            bindings.push(sym_form(p));
        }
        let mut loop_form = vec![
            sym_form(&Symbol::simple("loop")),
            Form { meta: None, value: FormValue::Vector(bindings), span },
        ];
        loop_form.extend(body.iter().cloned());
        Form {
            meta: None,
            value: FormValue::List(vec![
                params_form.clone(),
                Form { meta: None, value: FormValue::List(loop_form), span },
            ]),
            span: clause.span,
        }
    }

    /// Wraps one `(params body...)` clause as `(params (let [f (.f this)
    /// ...] body...))`, binding each basis field not shadowed by a param.
    /// Only applies when the first param is a plain symbol (it always is
    /// in real suite code; anything fancier just skips the sugar).
    ///
    /// # W-PROTO: only fields the body actually NAMES are bound
    ///
    /// This wrapper was, by a wide margin, the single biggest cost in
    /// protocol-method dispatch on a `defrecord` -- measured on this tree,
    /// `(area rec)` for `(defrecord Circle [r] Area (area [c] (* 3 (:r
    /// c))))` cost 1.40s per 2M in-loop calls WITH the wrapper and 0.61s
    /// with it skipped (a `sample` profile put 55% of the whole run
    /// inside `eval_let`, against a method body that mentions no field at
    /// all). The reason is that the generated binding is not a cheap
    /// field read: each one is a tree-walked `(.f this)` -- a fresh child
    /// `Env`, a `this` symbol resolution up the local chain, an
    /// `eval_dot_form`, an `inst_field` (which allocates a `Str` per call
    /// for a record), and a hash insert to bind the result -- per field,
    /// per call, whether or not the body ever reads it. `(:r c)` reads
    /// the field through the KEYWORD, so the `r` binding in that example
    /// is pure overhead.
    ///
    /// So a field is bound iff its name occurs as a symbol anywhere in
    /// the body ([`mentions_symbol`]) -- the same "cheap syntactic
    /// over-approximation, erring toward doing the work" policy
    /// [`Self::wrap_method_recur`] already uses for `recur`. An
    /// unmentioned binding is unobservable: a `defrecord`/`deftype` field
    /// is a plain lexical local, nothing can reach it except by naming
    /// it, and any name that reaches this scan -- shadowed, quoted,
    /// namespace-qualified, buried in metadata -- counts as a mention and
    /// keeps the binding.
    ///
    /// The one shape this does NOT see is a MACRO in the body whose
    /// expansion introduces the bare field symbol without the call site
    /// spelling it (`~'r` inside the macro). That is deliberate,
    /// unhygienic local capture; real Clojure's own syntax-quote emits
    /// namespace-qualified symbols precisely so it can't happen by
    /// accident, and the 10871-form conformance corpus contains no such
    /// form.
    /// `mutable`: parallel to `fields` (empty slice when the caller has no
    /// mutability info -- every non-deftype `collect_methods` caller).
    /// clojure-lsp campaign (mova/PLAN.md): for a field flagged mutable,
    /// this ALSO binds a hidden owner-marker symbol (`__mutfield_owner_
    /// <name>` -> `this`) alongside the ordinary `f -> (.f this)` binding,
    /// so `eval_set_bang` can find, from JUST the field's local binding
    /// being in scope, which instance to write the mutation back into --
    /// see that fn's doc. Zero cost when `mutable` is empty or all-false
    /// (no marker generated, no behaviour change from before this task).
    fn wrap_fields_let(
        &mut self,
        clause: Form,
        fields: &[Str],
        mutable: &[bool],
        span: Span,
    ) -> Result<Form, RjError> {
        let FormValue::List(items) = &clause.value else {
            return Ok(clause);
        };
        let Some(params_form) = items.first() else {
            return Ok(clause);
        };
        let FormValue::Vector(params) = &params_form.value else {
            return Ok(clause);
        };
        let Some(this_sym) = params.first().and_then(form_as_symbol) else {
            return Ok(clause);
        };
        let param_names: Vec<&Str> = params
            .iter()
            .filter_map(form_as_symbol)
            .map(|s| &s.name)
            .collect();
        let body = &items[1..];
        let mut bindings: Vec<Form> = Vec::new();
        for (i, f) in fields.iter().enumerate() {
            if param_names.iter().any(|p| *p == f) {
                continue;
            }
            // W-PROTO: see this fn's doc -- an unmentioned field's binding
            // is unobservable, and it is the dominant per-call cost.
            if !body.iter().any(|b| mentions_symbol(b, f)) {
                continue;
            }
            bindings.push(Form {
                meta: None,
                value: FormValue::Atom(Value::Sym(Symbol::simple(f.clone()))),
                span,
            });
            bindings.push(Form {
                meta: None,
                value: FormValue::List(vec![
                    Form {
                        meta: None,
                        // K1: `.-f` (field-only) compiles to `Ir::FieldGet`; `.f` escaped to the tree-walker.
                        // Same answer on an Inst receiver except the universal-Object names `.f` intercepts.
                        value: FormValue::Atom(Value::Sym(Symbol::simple(
                            if matches!(f.as_ref(), "equals" | "hashCode" | "getName" | "size" | "iterator" | "isArray") {
                                format!(".{f}")
                            } else {
                                format!(".-{f}")
                            },
                        ))),
                        span,
                    },
                    Form {
                        meta: None,
                        value: FormValue::Atom(Value::Sym(this_sym.clone())),
                        span,
                    },
                ]),
                span,
            });
            if mutable.get(i).copied().unwrap_or(false) {
                bindings.push(Form {
                    meta: None,
                    value: FormValue::Atom(Value::Sym(Symbol::simple(format!(
                        "__mutfield_owner_{f}"
                    )))),
                    span,
                });
                bindings.push(Form {
                    meta: None,
                    value: FormValue::Atom(Value::Sym(this_sym.clone())),
                    span,
                });
            }
        }
        if bindings.is_empty() {
            return Ok(clause);
        }
        let mut let_form = vec![
            Form {
                meta: None,
                value: FormValue::Atom(Value::Sym(Symbol::simple("let"))),
                span,
            },
            Form {
                meta: None,
                value: FormValue::Vector(bindings),
                span,
            },
        ];
        let_form.extend_from_slice(&items[1..]);
        let new_clause = vec![
            params_form.clone(),
            Form {
                meta: None,
                value: FormValue::List(let_form),
                span,
            },
        ];
        Ok(Form {
            meta: None,
            value: FormValue::List(new_clause),
            span: clause.span,
        })
    }

    /// Registers `methods` for `class_val` under protocol `proto_val`:
    /// updates the shared registry AND writes the protocol var's `:impls`
    /// back as a plain map (measured introspection surface --
    /// `extenders`/`(:impls P)` are plain data in real Clojure).
    pub(crate) fn register_protocol_impls(
        &mut self,
        proto_val: &Value,
        class_val: &Value,
        methods: MethodTable,
    ) -> Result<(), RjError> {
        self.register_protocol_impls_ex(proto_val, class_val, methods, false)
    }

    /// The INLINE sibling of `register_protocol_impls` above -- what
    /// `eval_deftype_like`'s own `defrecord`/`deftype`-body protocol
    /// groups call. Never rejects (a type declaring a protocol inline
    /// can't conflict with itself); MARKS the pair as inline instead, so
    /// a LATER `extend`/`extend-type`/`extend-protocol` on the same
    /// `(protocol, class)` pair can reject it -- see
    /// `builtins::types::mark_inline_protocol_impl`'s doc.
    pub(crate) fn register_protocol_impls_inline(
        &mut self,
        proto_val: &Value,
        class_val: &Value,
        methods: MethodTable,
    ) -> Result<(), RjError> {
        self.register_protocol_impls_ex(proto_val, class_val, methods, true)
    }

    fn register_protocol_impls_ex(
        &mut self,
        proto_val: &Value,
        class_val: &Value,
        methods: MethodTable,
        is_inline: bool,
    ) -> Result<(), RjError> {
        let key = crate::builtins::types::proto_key(proto_val, "extend")?;
        let ck = crate::builtins::types::key_for_class_value(class_val, "extend")?;
        // C14 (protocols): measured -- `extend`ing a protocol a type
        // already implements INLINE throws (real Clojure:
        // `IllegalArgumentException: class .. already directly
        // implements interface .. for protocol:..`). Never checked for
        // the inline registration itself (`is_inline` true): a type
        // declaring a protocol inline can't conflict with its own
        // declaration.
        if !is_inline && crate::builtins::types::is_inline_protocol_impl(key, &ck) {
            let class_name = match class_val {
                Value::Class(c) => c.name().to_string(),
                other => other.type_name().to_string(),
            };
            let proto_display = match proto_val {
                Value::Map(pm) => match pm.get(&Value::Keyword(Keyword::from("var"))) {
                    Some(Value::Var(cell)) => match &cell.name.ns {
                        Some(ns) => format!("#'{ns}/{}", cell.name.name),
                        None => format!("#'{}", cell.name.name),
                    },
                    _ => "?".to_string(),
                },
                _ => "?".to_string(),
            };
            // W3a: measured -- `IllegalArgumentException`, not the
            // `ClassCastException` `ErrorKind::TypeErr` maps to.
            return Err(RjError::type_err(format!(
                "class {class_name} already directly implements interface {class_name} for protocol:{proto_display}"
            ))
            .with_class(JvmClass::IllegalArgument));
        }
        {
            let mut reg = crate::sync::lock_write(&self.protocols.0);
            crate::jit::PROTO_EPOCH.fetch_add(1, std::sync::atomic::Ordering::Release);
            let proto = reg.entry(key).or_default();
            // Re-extension REPLACES the class's table (measured: a second
            // extend-type on the same class silently wins).
            proto.impls.insert(ck.clone(), (class_val.clone(), methods.clone()));
            // W-PROTO: `impls` just changed, so every answer the inline
            // cache is holding is now suspect -- not only for `ck` itself
            // (an `Object` row added here can also change what an
            // ALREADY-cached user type resolves to). Drop the whole cache
            // rather than trying to invalidate selectively: we are under
            // the registry WRITE guard, which excludes every concurrent
            // probe by construction, and extension is a load-time event
            // while dispatch is the hot path -- so paying a full re-warm
            // per `extend`/`extend-type`/inline `defrecord` group is free
            // and makes visibility unconditional. See `types::ProtoIc`.
            proto.ic = proto.ic.fresh();
        }
        if is_inline {
            crate::builtins::types::mark_inline_protocol_impl(key, ck);
        }
        // Mirror into the var's :impls plain map for introspection.
        if let Value::Map(pm) = proto_val {
            if let Some(Value::Var(cell)) = pm.get(&Value::Keyword(Keyword::from("var"))) {
                if let Some(Value::Map(cur)) = cell.get() {
                    let impls_k = Value::Keyword(Keyword::from("impls"));
                    let mut impls = match cur.get(&impls_k) {
                        Some(Value::Map(m)) => m.clone(),
                        _ => PMap::new(),
                    };
                    let mut mm = PMap::new();
                    for (name, f) in &methods {
                        mm.insert(Value::Keyword(Keyword::from(name)), f.clone());
                    }
                    impls.insert(class_val.clone(), Value::Map(mm));
                    let mut new_map = cur.clone();
                    new_map.insert(impls_k, Value::Map(impls));
                    cell.store(Value::Map(new_map), false);
                }
            }
        }
        Ok(())
    }
}

/// W-PROTO: does `form` mention a symbol whose NAME is `name` anywhere?
/// The `defrecord`/`deftype` field-binding filter -- see
/// [`Interp::wrap_fields_let`]'s doc for why this over-approximates on
/// purpose, and what the one shape it can't see is.
///
/// Deliberately name-only (a namespace-qualified `some.ns/r` can never BE
/// the local `r`, but counting it keeps the scan a single comparison and
/// only ever costs a binding nobody reads) and deliberately blind to
/// quoting/shadowing (`'r`, and an inner `let [r ..]`, both count).
/// Metadata forms are scanned too: `^{:doc r}` is a real, if exotic,
/// reference.
fn mentions_symbol(form: &Form, name: &Str) -> bool {
    if let Some(meta) = &form.meta {
        if mentions_symbol(meta, name) {
            return true;
        }
    }
    match &form.value {
        FormValue::Atom(Value::Sym(s)) => s.name == *name,
        FormValue::Atom(_) => false,
        FormValue::List(items) | FormValue::Vector(items) | FormValue::Set(items) => {
            items.iter().any(|f| mentions_symbol(f, name))
        }
        FormValue::Map(pairs) => pairs
            .iter()
            .any(|(k, v)| mentions_symbol(k, name) || mentions_symbol(v, name)),
    }
}

/// D1: does this form mention the symbol `recur` anywhere? A deliberate
/// over-approximation (a nested `fn`/`loop` owns its own `recur`, and a
/// quoted `'recur` mentions nothing at all) -- both only cost an unused
/// loop frame, never a wrong binding, because the wrapper this gates
/// rebinds the method's own parameters to themselves.
fn mentions_recur(form: &Form) -> bool {
    match &form.value {
        FormValue::Atom(Value::Sym(s)) => s.ns.is_none() && s.name.as_ref() == "recur",
        FormValue::Atom(_) => false,
        FormValue::List(items) | FormValue::Vector(items) | FormValue::Set(items) => {
            items.iter().any(mentions_recur)
        }
        FormValue::Map(pairs) => pairs.iter().any(|(k, v)| mentions_recur(k) || mentions_recur(v)),
    }
}

/// S5: the interface NAME an `implements`-position head evaluated to, or
/// `None` when the head is a protocol (or anything else, which
/// `register_protocol_impls` then rejects with its own message).
///
/// kondo-wave: `Object` (bare or `java.lang.Object`) is a real Clojure
/// idiom in a `defrecord`/`deftype` body -- `(defrecord R [...] P (m [_]
/// ..) Object (toString [this] ..))`, overriding `toString`/`equals`/
/// `hashCode` alongside a protocol impl (clj-kondo's own rewrite-clj
/// `KeywordNode`/`TokenNode`/... all do exactly this for `toString`).
/// `Object` evaluates to a `ClassVal::Builtin` (see `types::
/// builtin_classes()`), not `ClassVal::Interface` -- deliberately left
/// that way everywhere else (`class`/`instance?`/`new Object` all depend
/// on it being the ordinary builtin-class row it already is) -- so this
/// is the one place that treats it as an interface name, reusing the
/// SAME `register_interface_impls`/`lookup_interface_method` table every
/// real `definterface`/host-interface impl already goes through (see
/// this fn's two call sites in `eval_deftype_like`) rather than routing
/// it into `register_protocol_impls_inline`, which requires a genuine
/// protocol map and would reject `Object` with "interface
/// java.lang.Object is not a protocol" (measured: this was the bug
/// before this fix -- `proto_key`'s message, `builtins::types.rs`, meant
/// for a real user error, `(extend R java.lang.Comparable {..})`, not
/// this one).
fn interface_name_of(head: &Value) -> Option<Str> {
    match head {
        Value::Class(c) => match c.as_ref() {
            ClassVal::Interface { name } => Some(name.clone()),
            ClassVal::Builtin { name, .. } if *name == "java.lang.Object" => {
                Some(Str::from("java.lang.Object"))
            }
            _ => None,
        },
        _ => None,
    }
}

/// clojure-lsp campaign (mova/PLAN.md): an `implements`-position head of
/// literal `Object` (real Clojure resolves the bare symbol to `java.lang.
/// Object`) inside `defrecord`/`deftype`/`extend-type`/`extend-protocol`.
/// `rewrite-clj.node.comment/CommentNode` (and several sibling node
/// records) declare `Object (toString [node] ...)` right alongside their
/// real protocol impl, exactly the way a JVM class overrides
/// `toString`/`equals`/`hashCode` -- `Object` is never itself a Clojure
/// protocol, so routing its methods through `register_protocol_impls_
/// inline` throws "interface java.lang.Object is not a protocol"
/// (measured: that message IS real Clojure's for `extend`ing an actual
/// interface as if it were a protocol, `builtins::types::proto_key`'s
/// doc; nonsensical here since `Object` was never handed to `extend` at
/// all in real Clojure -- the compiler special-cases it).
///
/// mova has no JVM method-override table to file `toString`/`equals`/
/// `hashCode` into (no consumer calls them: `str`/`pr-str`/`=`/`hash` on
/// a `Value::Inst` are mova-native, not virtual dispatch), so this is
/// parse-and-discard -- the same treatment `eval_definterface` already
/// gives method SIGNATURES nothing here can reflect on. Not a library
/// hack: it is the general rule "`Object` in an impl-group head is never
/// a protocol", independent of which library's `deftype` triggers it.
fn is_object_class(head: &Value) -> bool {
    matches!(
        head,
        Value::Class(c) if matches!(c.as_ref(), ClassVal::Builtin { name, .. } if *name == "java.lang.Object")
    )
}

/// clojure-lsp campaign (mova/PLAN.md): same "route to the interface
/// method registry, not `register_protocol_impls_inline`" treatment as
/// `is_object_class` above, for a fixed list of `clojure.lang.*` marker
/// interfaces that ALSO have a real `types::builtin_classes()` row (an
/// `is_map`/`is_fn`/... predicate backs their OWN `instance?`/`class`
/// semantics for mova's native map/fn/... values) -- so they resolve as
/// `ClassVal::Builtin`, not `ClassVal::Interface`, and `interface_name_of`
/// alone can't route them. `data.priority-map`'s `PersistentPriorityMap`
/// deftype declares several of these heads purely so `.assoc`/`.invoke`/
/// `.withMeta`/... interop on ITS OWN instances reaches the override;
/// mova's native `assoc`/`conj`/`get`/... fns never consult this
/// registry for a `Value::Map`, so filing these methods here is a no-op
/// for anything except direct `.method` interop on a
/// `PersistentPriorityMap` instance -- same "interop-only, no native
/// dispatch" scope as `IPersistentStack`/`Reversible` in `types::
/// builtin_interfaces()`. Grow this list only from a real blocker, same
/// rule as everywhere else in this campaign.
fn known_builtin_interface_name(head: &Value) -> Option<Str> {
    const NAMES: &[&str] = &[
        "clojure.lang.IPersistentMap",
        "clojure.lang.IFn",
        "clojure.lang.IObj",
        "clojure.lang.Sorted",
        "java.util.Map",
        "clojure.lang.IPersistentCollection",
    ];
    match head {
        Value::Class(c) => match c.as_ref() {
            ClassVal::Builtin { name, .. } if NAMES.contains(name) => Some(Str::from(*name)),
            _ => None,
        },
        _ => None,
    }
}

/// W3d2: the parameter count (INCLUDING `this`) of a normalized `(params
/// body...)` method clause; `None` when the clause has no parameter
/// vector, which `eval_fn_form` will reject with its own message.
fn clause_param_count(arity_clause: &Form) -> Option<usize> {
    let FormValue::List(items) = &arity_clause.value else {
        return None;
    };
    match items.first() {
        Some(Form { value: FormValue::Vector(params), .. }) => Some(params.len()),
        _ => None,
    }
}

/// W3d2: does a runtime value satisfy a parameter's `^Tag`?
///
/// Used ONLY to pick between clauses that share a name AND an arity --
/// clauses the author already wrote, so this is a tie-break, never a type
/// CHECK. An unknown tag deliberately matches nothing rather than
/// erroring: the sibling clause then wins, and if none matches the
/// dispatcher reports it.
fn tag_matches(tag: &str, v: &Value) -> bool {
    match tag {
        "int" | "long" | "short" | "byte" | "java.lang.Integer" | "java.lang.Long"
        | "java.lang.Short" | "java.lang.Byte" => matches!(v, Value::Int(_)),
        "float" | "double" | "java.lang.Float" | "java.lang.Double" => {
            matches!(v, Value::Float(_))
        }
        "boolean" | "java.lang.Boolean" => matches!(v, Value::Bool(_)),
        "char" | "java.lang.Character" => matches!(v, Value::Char(_)),
        "objects" | "java.lang.Object" => true,
        other => crate::types::class_isa(crate::types::builtin_class_name(v), other),
    }
}

/// W3d2: collapses one method name's SAME-ARITY overload set into a single
/// ordinary `Value`, so `MethodTable` stays `name -> Value` and no
/// consumer learns that overloads exist (see `collect_methods`' own
/// comment).
///
/// Resolution is by runtime argument type against each variant's `^Tag`s,
/// in source order, first match wins -- real Clojure resolves the same
/// overload STATICALLY at the call site from the hint on the receiver
/// expression, which a hostless runtime has no equivalent of; the runtime
/// types are the only evidence available. An arity nothing implements is
/// `AbstractMethodError`, matching what the JVM raises for a method the
/// receiver's class does not define (oracle-measured).
fn overload_dispatcher(mname: Str, variants: Vec<(usize, Vec<Option<Str>>, Value)>) -> Value {
    let display = mname.to_string();
    Value::Native(Arc::new(NativeFn::new(display.clone(), move |interp, call_args| {
        let n = call_args.len();
        let mut arity_seen = false;
        for (params, tags, f) in &variants {
            if *params != n {
                continue;
            }
            arity_seen = true;
            // `call_args[0]` is `this`; tags are per-parameter AFTER it.
            let matched = tags
                .iter()
                .zip(call_args.iter().skip(1))
                .all(|(t, v)| match t {
                    Some(t) => tag_matches(t, v),
                    None => true,
                });
            if matched {
                return interp.apply_value(f, call_args, Span { start: 0, end: 0 });
            }
        }
        if arity_seen {
            return Err(RjError::type_err(format!(
                "{display}: no overload matches the argument types"
            )));
        }
        Err(RjError::other(format!(
            "no implementation of {display} at arity {}",
            n.saturating_sub(1)
        ))
        .with_class(crate::error::JvmClass::AbstractMethod))
    })))
}

/// W3d2, measured (`hinting-test`'s "that invalid primitive types on
/// hinted defrecord fields fails"): a PRIMITIVE `^long`/`^byte`/
/// `^boolean`/... hint on a `defrecord`/`deftype` basis field is a real
/// constraint on the constructed value, not decoration -- real Clojure
/// compiles the field to a primitive slot and every factory
/// (`->R`/`map->R`/`R/create`/`(R. ..)`/the ctor literal) coerces or
/// throws. All six of that sub-test's rows only assert THAT it throws
/// (with `ClassCastException`, which `RjError::type_err` already maps to),
/// so this reproduces the constraint, not the JVM's exact coercion table.
///
/// Deliberately narrow, in both directions:
/// - only PRIMITIVE tags are checked. A reference hint (`^String a`) is
///   covariant on the JVM and that row of the same deftest is explicitly
///   `(testing "covariant hints -- deferred")` upstream, so checking one
///   would be unmeasured surface that could only reject working code.
/// - `nil` always passes. `map->R` fills a missing basis key with `nil`
///   (measured, and `test-record-factory-fns` depends on it), so rejecting
///   `nil` here would break record construction unrelated to hinting.
fn check_field_tags(tdef: &TypeDef, vals: &[Value]) -> Result<(), RjError> {
    for (i, v) in vals.iter().enumerate() {
        let Some(Value::Sym(tag)) = tdef.field_tags.get(i) else {
            continue;
        };
        let ok = match tag.name.as_ref() {
            "long" | "int" | "short" | "byte" => matches!(
                v,
                Value::Nil | Value::Int(_) | Value::BigInt(_) | Value::BigInteger(_)
            ),
            "double" | "float" => matches!(v, Value::Nil | Value::Float(_) | Value::Int(_)),
            "boolean" => matches!(v, Value::Nil | Value::Bool(_)),
            "char" => matches!(v, Value::Nil | Value::Char(_)),
            _ => true,
        };
        if !ok {
            return Err(RjError::type_err(format!(
                "ClassCastException: {} cannot be cast to {} (field {} of {})",
                crate::types::builtin_class_name(v),
                tag.name,
                tdef.basis.get(i).map(|s| s.as_ref()).unwrap_or("?"),
                tdef.name
            )));
        }
    }
    Ok(())
}

/// Builds an instance from positional field values.
fn make_instance(tdef: &Arc<TypeDef>, vals: &[Value]) -> Value {
    if tdef.is_record {
        // K6: one-shot build (basis keys are distinct) -- same end shape as the old insert-per-field loop.
        let pairs: Vec<(Value, Value)> = tdef.basis.iter().zip(vals).map(|(f, v)| (Value::Keyword(Keyword::from(f)), v.clone())).collect();
        let data = if pairs.len() <= crate::value::PMAP_SMALL_MAX {
            PMap::Small(Arc::new(pairs))
        } else if let Some(sm) = crate::shaped_map::ShapedMap::from_pairs(pairs.iter().map(|(k, v)| (k, v))) {
            PMap::Shaped(sm)
        } else {
            PMap::from_unique_pairs(pairs)
        };
        Value::Inst(Arc::new(InstVal {
            tdef: tdef.clone(),
            data,
            fields: Mutex::new(PVec::new()),
            meta: None,
        }))
    } else {
        Value::Inst(Arc::new(InstVal {
            tdef: tdef.clone(),
            data: PMap::new(),
            fields: Mutex::new(vals.iter().cloned().collect()),
            meta: None,
        }))
    }
}

/// S4 `import`: strips a leading `(quote X)` wrapper off an arg FORM,
/// returning `X` as a `Value` -- else the form itself, converted as-is.
/// This is what lets `(import '(java.lang Boolean))` and `(import
/// (java.lang Boolean))` parse identically (see `eval_import`'s doc).
fn strip_quote(form: &Form) -> Value {
    if let FormValue::List(items) = &form.value {
        if items.len() == 2 {
            if let Some(sym) = form_as_symbol(&items[0]) {
                if sym.ns.is_none() && sym.name.as_ref() == "quote" {
                    return crate::reader::form_to_value(&items[1]);
                }
            }
        }
    }
    crate::reader::form_to_value(form)
}

/// W4B-WARNINGS scope addendum: real JVM `.hashCode()` for the two
/// numeric-tower shapes `control.clj`'s hash-collision row needs --
/// `(int)(x ^ (x >>> 32))` for a `long`
/// (`.oracle/clojure-src`'s own `java.lang.Long.hashCode`, measured
/// directly: `(.hashCode -1)` is `0`, `(.hashCode Long/MIN_VALUE)` is
/// `-2147483648`), and `clojure.lang.BigInt.hashCode`'s own two-path rule
/// for a `BigInt` (`.oracle/clojure-src/src/jvm/clojure/lang/BigInt.java`):
/// a value that fits in a `long` hashes via that SAME long formula (not
/// `BigInteger.hashCode()` -- measured divergence: `(.hashCode -1N)` is
/// `0`, but `(.hashCode (.toBigInteger -1N))` is `-1`); one that doesn't
/// fit uses `bignum::BigIntVal::java_hash_code` (the real
/// `BigInteger.hashCode()` word algorithm, already implemented there for
/// `hash`/`hasheq`'s own big-BigInt fallback). `None` for anything else,
/// so the caller keeps its prior `hash_value`-based behavior for every
/// other type -- not a JVM-exact `.hashCode` in general, only for these
/// two shapes, which is all this row measures.
fn jvm_long_hash_code(n: i64) -> i32 {
    (n ^ ((n as u64 >> 32) as i64)) as i32
}

fn jvm_hash_code_for(target: &Value) -> Option<i64> {
    match target {
        Value::Int(n) => Some(jvm_long_hash_code(*n) as i64),
        Value::BigInt(b) => Some(match b.to_i64_exact() {
            Some(n) => jvm_long_hash_code(n) as i64,
            None => b.java_hash_code() as i64,
        }),
        _ => None,
    }
}

/// The short name `import` binds a fully-qualified class name under:
/// everything after the last `.` (`java.lang.Boolean` -> `Boolean`).
fn short_class_name(full_name: &str) -> &str {
    full_name.rsplit('.').next().unwrap_or(full_name)
}

/// `(.f inst)` field read -- records by keyword, deftypes by basis index.
pub(crate) fn inst_field(inst: &InstVal, field: &str) -> Option<Value> {
    if inst.tdef.is_record {
        inst.data.get(&Value::Keyword(Keyword::from(field))).cloned()
    } else {
        let idx = inst.tdef.basis.iter().position(|b| b.as_ref() == field)?;
        crate::sync::lock_mutex(&inst.fields).get_owned(idx)
    }
}

// ---- heap-image gate-1: factored native constructors ----
/// Heap-image gate-1: defprotocol's per-method dispatch native, factored out
/// so `src/image.rs` can re-create it from its recipe.
#[allow(clippy::too_many_arguments)]
pub(crate) fn proto_dispatch_native(
    mname_for_fn: Str,
    var_display_for_fn: String,
    iface_for_fn: Str,
    protocol_name_for_fn: Str,
    arity_hint: (usize, usize),
    cell: Arc<crate::env::VarCell>,
    epoch: u64,
    ic_midx: usize,
) -> NativeFn {
    let key = Arc::as_ptr(&cell) as usize;
    let recipe = crate::image::Recipe::Proto {
        mname: mname_for_fn.clone(),
        var_display: var_display_for_fn.clone(),
        iface: iface_for_fn.clone(),
        pname: protocol_name_for_fn.clone(),
        arity: arity_hint,
        cell: cell.clone(),
        epoch,
        midx: ic_midx,
    };
    let mut n = NativeFn::new(mname_for_fn.to_string(), move |interp, call_args| {
                if call_args.is_empty()
                    || call_args.len() < arity_hint.0
                    || call_args.len() > arity_hint.1
                {
                    return Err(RjError::arity(format!(
                        "{}: wrong number of args ({})",
                        mname_for_fn,
                        call_args.len()
                    )));
                }
                #[cfg(feature = "k2-count")]
                crate::k2count::PROTO_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let target = &call_args[0];
                match crate::builtins::types::lookup_method(
                    &interp.protocols,
                    key,
                    epoch,
                    ic_midx,
                    target,
                    &mname_for_fn,
                ) {
                    Some(f) => {
                        // W3d2, oracle-measured: calling a protocol method
                        // at an arity a `reify` did NOT implement is
                        // `AbstractMethodError` on the JVM ("Receiver class
                        // ... does not define or inherit an implementation
                        // of the resolved method ..."), not the arity error
                        // the underlying closure raises. `protocols-test`'s
                        // "you can implement just part of a protocol"
                        // asserts exactly that class.
                        //
                        // Error path only: a successful call never reaches
                        // the `match`, and a non-anonymous receiver never
                        // enters the remap at all.
                        let anonymous = matches!(
                            target, Value::Inst(i) if !i.tdef.methods.is_empty()
                        );
                        let out = interp.apply_value(&f, call_args, Span { start: 0, end: 0 });
                        match out {
                            Err(e) if anonymous && e.kind == crate::error::ErrorKind::Arity => {
                                Err(RjError::other(format!(
                                    "{} does not implement method :{} at this arity",
                                    match target {
                                        Value::Inst(i) => i.tdef.name.to_string(),
                                        other => other.type_name().to_string(),
                                    },
                                    mname_for_fn
                                ))
                                .with_class(crate::error::JvmClass::AbstractMethod))
                            }
                            other => other,
                        }
                    }
                    // W4B-MESSAGES (protocols.clj's "you can redefine a
                    // protocol with different methods"): a NO-IMPL result
                    // splits into two genuinely different real
                    // conditions, and this is the one checked FIRST. If
                    // the protocol this dispatch fn was minted for has
                    // since been REDEFINED without this method at all
                    // (`ProtoDef::declared_methods`, rebuilt fresh by
                    // every `defprotocol` call -- see that field's doc),
                    // real Clojure reports a DIFFERENT `IllegalArgument
                    // Exception`: "No method of interface: ... found for
                    // function: ... of protocol: ... (The protocol method
                    // may have been defined before and removed.)" --
                    // measured, compat/w4b-protocols-oracle-
                    // transcript.txt's first probe. A protocol that no
                    // longer EXISTS at all (its var cell's `key` absent
                    // from the registry) is treated the same way -- both
                    // mean "this dispatch fn's own method no longer
                    // belongs to protocol", which is exactly this
                    // message's condition.
                    None if {
                        let reg = crate::sync::lock_read(&interp.protocols.0);
                        !reg.get(&key).is_some_and(|p| p.declared_methods.contains(&mname_for_fn))
                    } =>
                    {
                        Err(RjError::other(format!(
                            "No method of interface: {iface_for_fn} found for function: {mname_for_fn} of protocol: {protocol_name_for_fn} (The protocol method may have been defined before and removed.)"
                        ))
                        .with_class(JvmClass::IllegalArgument))
                    }
                    // W3a: measured -- real Clojure's protocol dispatch
                    // raises `java.lang.IllegalArgumentException` with
                    // EXACTLY this message (`(foo 10)` on a protocol with
                    // no Long impl => "No implementation of method: :foo of
                    // protocol: #'user/P found for class: java.lang.Long").
                    // protocols.clj asserts it by class AND message.
                    None => Err(RjError::other(format!(
                        // W3d2: real Clojure's no-impl throw is an
                        // `IllegalArgumentException` (oracle-measured);
                        // the class name is spelled out here VERBATIM so
                        // `special_forms::error_kind_class_chain`'s sniff
                        // can map it -- same technique, and same reason,
                        // as `builtins::recorddot`'s `unsupported()`.
                        // The measured message text follows the prefix
                        // unchanged, so the suite's own `re-find` on it
                        // still matches.
                        "IllegalArgumentException: No implementation of method: :{} of protocol: {} found for class: {}",
                        mname_for_fn,
                        var_display_for_fn,
                        match target {
                            Value::Nil => "nil".to_string(),
                            Value::Inst(i) => i.tdef.name.to_string(),
                            other => crate::types::builtin_class_name(other).to_string(),
                        }
                    ))
                    .with_class(JvmClass::IllegalArgument)),
                }
            });
    n.image_recipe = Some(Box::new(recipe));
    n
}

pub(crate) fn record_ctor_native(tdef: Arc<TypeDef>, ctor_name: String) -> NativeFn {
    let n_fields = tdef.basis.len();
    let who_owned = ctor_name.clone();
    let recipe = crate::image::Recipe::Ctor { tdef: tdef.clone(), name: ctor_name.clone() };
    let mut n = NativeFn::new(ctor_name.clone(), move |_i, call_args| {
                if call_args.len() != n_fields {
                    return Err(RjError::arity(format!(
                        "{}: expected {} args, got {}",
                        who_owned,
                        n_fields,
                        call_args.len()
                    )));
                }
                check_field_tags(&tdef, call_args)?;
                Ok(make_instance(&tdef, call_args))
            });
    n.image_recipe = Some(Box::new(recipe));
    n
}

pub(crate) fn get_basis_native(tdef: Arc<TypeDef>) -> NativeFn {
    let basis_syms = tdef.basis.clone();
    let field_tags_for_basis = tdef.field_tags.clone();
    let recipe = crate::image::Recipe::Basis { tdef: tdef.clone() };
    let mut n = NativeFn::new("getBasis".to_string(), move |_i, call_args| {
                if !call_args.is_empty() {
                    return Err(RjError::arity(format!(
                        "getBasis: expected 0 args, got {}",
                        call_args.len()
                    )));
                }
                let mut v = PVec::new();
                for (f, tag) in basis_syms.iter().zip(field_tags_for_basis.iter()) {
                    let sym = Value::Sym(Symbol::simple(f.clone()));
                    let tagged = match tag {
                        Value::Nil => sym,
                        t => {
                            let mut m = PMap::new();
                            m.insert(Value::Keyword(Keyword::from("tag")), t.clone());
                            Value::attach_meta(sym, Value::Map(m))
                        }
                    };
                    v.push_back(tagged);
                }
                Ok(Value::Vector(v))
            });
    n.image_recipe = Some(Box::new(recipe));
    n
}

pub(crate) fn map_ctor_native(tdef: Arc<TypeDef>, map_ctor_name: String) -> NativeFn {
    let who_owned = map_ctor_name.clone();
    let recipe = crate::image::Recipe::MapCtor { tdef: tdef.clone(), name: map_ctor_name.clone() };
    let mut n = NativeFn::new(map_ctor_name.clone(), move |interp, call_args| {
                if call_args.len() != 1 {
                    return Err(RjError::arity(format!(
                        "{}: expected 1 arg, got {}",
                        who_owned,
                        call_args.len()
                    )));
                }
                let src = match &call_args[0] {
                    Value::Map(m) => m.clone(),
                    Value::Inst(i) if i.tdef.is_record => i.data.clone(),
                    Value::HostStruct(hs) => crate::host_struct::as_pmap(hs).clone(),
                    Value::LazyMap(hs) => crate::lazy_map::as_pmap(hs).clone(),
                    // W3d2: the `java.util.HashMap` veneer -- measured
                    // (`test-record-factory-fns`' "record equality"
                    // sub-test): `(map->R (java.util.HashMap.))` is
                    // `(R. nil nil)` on the real JVM, because
                    // `RecordName/create` takes an `IPersistentMap` and
                    // Clojure's `PersistentHashMap/create` accepts any
                    // `java.util.Map`. Reads a SNAPSHOT of the (mutable)
                    // veneer's backing map, same as every other reader of
                    // `HostState::HashMap`.
                    Value::HostInst(h) if h.kind == crate::hostclass::HostKind::HashMap => {
                        let guard = crate::sync::lock_mutex(&h.state);
                        let crate::hostclass::HostState::HashMap(m) = &*guard else {
                            unreachable!("HostKind::HashMap always holds HostState::HashMap");
                        };
                        m.clone()
                    }
                    Value::Nil => PMap::new(),
                    other => {
                        return Err(RjError::type_err(format!(
                            "{}: expected a map, got {}",
                            who_owned,
                            other.type_name()
                        )))
                    }
                };
                let _ = interp;
                // Measured: every basis key present (missing -> nil),
                // extra keys ride along as ext entries.
                let mut data = src;
                for f in &tdef.basis {
                    let k = Value::Keyword(Keyword::from(f));
                    if data.get(&k).is_none() {
                        data.insert(k, Value::Nil);
                    }
                }
                // W3d2: the same primitive-hint constraint the positional
                // factory enforces -- `hinting-test` calls both
                // `(map->RecordToTestLongHint {:a ""})` and
                // `(RecordToTestLongHint/create {:a ""})` (the SAME fn
                // under two names) and requires each to throw.
                let basis_vals: Vec<Value> = tdef
                    .basis
                    .iter()
                    .map(|f| data.get(&Value::Keyword(Keyword::from(f))).cloned().unwrap_or(Value::Nil))
                    .collect();
                check_field_tags(&tdef, &basis_vals)?;
                Ok(Value::Inst(Arc::new(InstVal {
                    tdef: tdef.clone(),
                    data,
                    fields: Mutex::new(PVec::new()),
                    meta: None,
                })))
            });
    n.image_recipe = Some(Box::new(recipe));
    n
}
