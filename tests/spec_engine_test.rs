//! SPEC-W1 (`docs/SPEC-ALPHA-CAMPAIGN.md`, wave 1 / branch `spec/engine`):
//! the engine gaps `clojure.spec.alpha` and its `clojure.test.check`
//! generator backend need, each measured against the spelling upstream
//! actually uses.
//!
//! Every case runs through BOTH tiers -- the compiled one and the
//! tree-walker (`Interp::with_compile_enabled(false)`) -- the same
//! discipline `tests/ns_test.rs` and `tests/differential_test.rs` apply,
//! because several of these features sit on paths the two tiers reach
//! differently (`instance?` is a native either way, but `get`/keyword
//! invocation and `.method` dispatch are not).

use mova::internal::Interp;

/// Evaluates `src` in a fresh interpreter of each tier, asserts the two
/// agree, and returns the shared `pr-str` of the result.
fn ev(src: &str) -> String {
    fn one(mut interp: Interp, src: &str) -> String {
        match interp.eval_str("spec-engine-test", src) {
            Ok(v) => match interp.realize_deep(&v) {
                Ok(r) => mova::internal::pr_str(&r),
                Err(e) => format!("ERR: {}", e.message),
            },
            Err(e) => format!("ERR: {}", e.message),
        }
    }
    let compiled = one(Interp::new(), src);
    let walked = one(Interp::with_compile_enabled(false), src);
    assert_eq!(compiled, walked, "tier divergence for: {src}");
    compiled
}

fn is(src: &str, expected: &str) {
    assert_eq!(ev(src), expected, "for: {src}");
}

// ---------------------------------------------------------------------------
// Task 3: `instance? Throwable` agrees with what `catch Throwable` catches
// ---------------------------------------------------------------------------

/// `clojure.spec.alpha`'s `validate-fn` is `(let [ret (try (apply f args)
/// (catch Throwable t t))] (if (instance? Throwable ret) ...))`, and
/// `clojure.test.check.properties`' `exception?` is literally `(instance?
/// Throwable x)`. Both read a `false` here as "the call succeeded".
#[test]
fn instance_throwable_is_true_for_an_ex_info_value() {
    is("(instance? Throwable (ex-info \"x\" {}))", "true");
    is("(instance? Exception (ex-info \"x\" {}))", "true");
    is("(instance? RuntimeException (ex-info \"x\" {}))", "true");
    // The same value once it has been thrown and caught.
    is(
        "(try (throw (ex-info \"x\" {:a 1})) (catch Throwable t \
         [(instance? Throwable t) (ex-data t) (.getMessage t)]))",
        "[true {:a 1} \"x\"]",
    );
}

/// A caught INTERNAL error binds to `error_to_info_map`'s `{:type
/// :error/<kind> :message ..}` map. `catch Throwable` catches it, so
/// `instance? Throwable` must agree -- and `.getMessage` on it must work,
/// because `validate-fn` reads exactly that on the value its `instance?`
/// test just accepted.
#[test]
fn instance_throwable_is_true_for_a_caught_internal_error() {
    is(
        "(try (/ 1 0) (catch Throwable t \
         [(instance? Throwable t) (instance? Exception t) (.getMessage t)]))",
        "[true true \"Divide by zero\"]",
    );
    is(
        "(try (nth [1] 9) (catch Throwable t (instance? Throwable t)))",
        "true",
    );
    // `.getData`/`.getCause` are nil on an internal error, exactly as they
    // are on a real `RuntimeException` with neither.
    is(
        "(try (/ 1 0) (catch Throwable t [(.getData t) (.getCause t)]))",
        "[nil nil]",
    );
}

/// `instance?` stays NARROWER than `catch`, deliberately: `catch Exception`
/// is total over every thrown value (see `thrown_value_class_chain`'s doc
/// and tests/conformance/DEVIATIONS.md), `instance?` still answers the
/// JVM-faithful "is this really shaped like one". An ordinary map is not an
/// exception -- which matters, because `test.check`'s `exception?` runs on
/// arbitrary property RETURN values and re-throws whatever it accepts.
#[test]
fn instance_throwable_stays_false_for_ordinary_values() {
    is("(instance? Throwable 42)", "false");
    is("(instance? Throwable \"boom\")", "false");
    is("(instance? Throwable {:a 1})", "false");
    is("(instance? Throwable {:type :other/thing :message \"m\"})", "false");
    is("(instance? Throwable nil)", "false");
    is("(instance? Throwable [1 2])", "false");
    // ... and a host exception instance keeps its own, more specific answer.
    is(
        "[(instance? Throwable (Exception. \"e\")) \
          (instance? IllegalArgumentException (Exception. \"e\"))]",
        "[true false]",
    );
}

// ---------------------------------------------------------------------------
// Task 4: `#inst` / `inst?` / `inst-ms`
// ---------------------------------------------------------------------------

/// `clojure.spec.alpha`'s `inst-in-range?` is `(and (inst? inst) (let [t
/// (inst-ms inst)] ...))` -- both halves have to work on a `#inst` literal
/// and on a `java.util.Date`, which are now the same value shape.
#[test]
fn inst_literal_is_a_real_instant() {
    is("(inst? #inst \"2020-01-01\")", "true");
    is("(inst? (java.util.Date. 0))", "true");
    is("(inst-ms #inst \"1970-01-01T00:00:00.100\")", "100");
    is("(inst-ms #inst \"1970-01-01T00:00:00.100-00:00\")", "100");
    is("(inst-ms (java.util.Date. 1234))", "1234");
    is("(class #inst \"2020-01-01\")", "java.util.Date");
    is("(.getTime #inst \"1970-01-01T00:00:00.250Z\")", "250");
}

/// Every optional field defaults to its lowest legal value, and a `+HH:MM`
/// / `-HH:MM` / `Z` offset is applied -- `clojure.instant/parse-timestamp`'s
/// own grammar.
#[test]
fn inst_literal_grammar_matches_clojures() {
    is("(inst-ms #inst \"1970\")", "0");
    is("(inst-ms #inst \"1970-01-01\")", "0");
    is("(inst-ms #inst \"1970-01-01T00:00:00Z\")", "0");
    is("(inst-ms #inst \"1970-01-01T01:00:00+01:00\")", "0");
    is("(inst-ms #inst \"1969-12-31T23:00:00-01:00\")", "0");
    is("(inst-ms #inst \"1969-12-31T23:59:59.999Z\")", "-1");
    // Sub-millisecond digits truncate, like the JVM's own Date reader.
    is("(inst-ms #inst \"1970-01-01T00:00:00.123456Z\")", "123");
    // Leap years are real: 2020 has a Feb 29, 1900 does not.
    is("(inst-ms #inst \"2020-02-29\")", "1582934400000");
    is("(pr-str #inst \"2020-02-29\")", "\"#inst \\\"2020-02-29T00:00:00.000-00:00\\\"\"");
}

/// Text outside the grammar is a READ-time error, exactly as it is on the
/// JVM (`clojure.instant/parse-timestamp` throws from the reader).
#[test]
fn a_malformed_inst_literal_is_a_reader_error() {
    assert!(
        ev("#inst \"not-a-date\"").contains("Unrecognized date/time syntax"),
        "got: {}",
        ev("#inst \"not-a-date\"")
    );
    assert!(ev("#inst \"2020-13-01\"").contains("Unrecognized date/time syntax"));
    assert!(ev("#inst \"2019-02-29\"").contains("Unrecognized date/time syntax"));
    assert!(ev("#inst 2020").contains("#inst data reader expected string"));
}

/// `inst-ms` on a non-instant reports the same condition real Clojure's
/// protocol dispatch does, rather than answering something.
#[test]
fn inst_ms_rejects_a_non_instant() {
    assert!(ev("(inst-ms 1)").contains("no implementation of method"));
    assert!(ev("(inst-ms \"2020-01-01\")").contains("no implementation of method"));
}

// ---------------------------------------------------------------------------
// Task 5: `clojure.lang.MultiFn`'s `.dispatchFn` / `.getMethod`
// ---------------------------------------------------------------------------

/// `clojure.spec.alpha`'s `multi-spec-impl` calls both on a deref'd
/// multimethod var: `#(let [^clojure.lang.MultiFn mm @mmvar] (and
/// (.getMethod mm ((.dispatchFn mm) %)) (mm %)))`, plus `dval`
/// `#((.dispatchFn ^clojure.lang.MultiFn @mmvar) %)`.
#[test]
fn multifn_dispatch_fn_and_get_method_are_reachable_by_interop() {
    is(
        "(do (defmulti shape :kind)\n\
         \x20   (defmethod shape :circle [m] [:circle m])\n\
         \x20   (defmethod shape :default [m] :other)\n\
         \x20   [((.dispatchFn shape) {:kind :circle})\n\
         \x20    ((.getMethod shape :circle) {:kind :circle})\n\
         \x20    ((.getMethod shape :nope) {:kind :nope})\n\
         \x20    (= (.getMethod shape :circle) (get-method shape :circle))])",
        "[:circle [:circle {:kind :circle}] :other true]",
    );
}

/// Exactly the upstream spelling, through a VAR deref, with the dispatch
/// value computed rather than written literally.
#[test]
fn multi_spec_impl_shaped_call_works_end_to_end() {
    is(
        "(do (defmulti event-type :event/type)\n\
         \x20   (defmethod event-type :search [_] :search-spec)\n\
         \x20   (let [mmvar (var event-type)\n\
         \x20         dval #((.dispatchFn @mmvar) %)\n\
         \x20         predx #(let [mm @mmvar] (and (.getMethod mm ((.dispatchFn mm) %)) (mm %)))]\n\
         \x20     [(dval {:event/type :search})\n\
         \x20      (predx {:event/type :search})\n\
         \x20      (predx {:event/type :missing})]))",
        "[:search :search-spec nil]",
    );
}

/// The receiver is validated by the same delegate `get-method` uses, so an
/// ordinary fn is a clean type error rather than a wrong answer.
#[test]
fn dispatch_fn_on_a_non_multimethod_is_a_type_error() {
    assert!(ev("(.dispatchFn inc)").contains("expected a multimethod"));
    assert!(ev("(.getMethod inc :k)").contains("expected a multimethod"));
}

// ---------------------------------------------------------------------------
// Task 6: `reify clojure.lang.ILookup`
// ---------------------------------------------------------------------------

/// `clojure.spec.alpha`'s `fspec-impl` reifies exactly this so `(:args
/// aspec)`/`(:ret aspec)` read the spec map back off the returned object.
#[test]
fn reify_ilookup_routes_get_and_keyword_lookup_through_val_at() {
    let src = "(let [specs {:args :A :ret :R}\n\
        \x20      o (reify clojure.lang.ILookup\n\
        \x20          (valAt [this k] (get specs k))\n\
        \x20          (valAt [_ k not-found] (get specs k not-found)))]\n\
        \x20 [(get o :args) (get o :ret) (get o :nope) (get o :nope :dflt)\n\
        \x20  (:args o) (:nope o) (:nope o :dflt)\n\
        \x20  (.valAt o :args) (.valAt o :nope :dflt)\n\
        \x20  (instance? clojure.lang.ILookup o)])";
    is(src, "[:A :R nil :dflt :A nil :dflt :A :dflt true]");
}

/// The 3-arity is NOT synthesized from the 2-arity: `(get o k nf)` calls
/// the 3-arity clause as written, so a `valAt` that ignores `not-found`
/// is observable (which is what makes the pair worth implementing
/// separately).
#[test]
fn both_val_at_arities_are_dispatched_as_written() {
    is(
        "(let [o (reify clojure.lang.ILookup\n\
         \x20        (valAt [_ k] [:two k])\n\
         \x20        (valAt [_ k nf] [:three k nf]))]\n\
         \x20 [(get o :k) (get o :k :nf) (:k o) (:k o :nf)])",
        "[[:two :k] [:three :k :nf] [:two :k] [:three :k :nf]]",
    );
}

/// A `deftype`/`reify` that declares no ILookup keeps the old behavior
/// (the default, never an error), and a record still reads its own map --
/// this hook adds a case, it does not reroute the existing ones.
#[test]
fn instances_that_do_not_declare_ilookup_are_unaffected() {
    is("(do (deftype Plain [a]) [(get (Plain. 1) :a) (get (Plain. 1) :a :d) (:a (Plain. 1))])",
       "[nil :d nil]");
    is("(do (defrecord R [a b]) [(get (->R 1 2) :a) (:b (->R 1 2)) (get (->R 1 2) :z :d)])",
       "[1 2 :d]");
}

/// `clojure.lang.ILookup` declares only `valAt`, so a misspelled clause is
/// rejected at reify time rather than silently ignored -- the same
/// method-inventory check `java.util.List`/`Collection`/`Object` get.
#[test]
fn an_undeclared_ilookup_method_is_rejected() {
    assert!(
        ev("(reify clojure.lang.ILookup (valAtt [_ k] k))").contains("declares no such method"),
        "got: {}",
        ev("(reify clojure.lang.ILookup (valAtt [_ k] k))")
    );
}

// ---------------------------------------------------------------------------
// Task 7: `BigDecimal/valueOf` + `java.net.URI/create`
// ---------------------------------------------------------------------------

/// `clojure.test.check.generators`' gen-builtins spell these two exactly
/// this way (`#(BigDecimal/valueOf %)` and `#(java.net.URI/create (str
/// "http://" % ".com"))`).
#[test]
fn gen_builtin_statics_resolve_onto_the_existing_backing_types() {
    is("(BigDecimal/valueOf 1.5)", "1.5M");
    is("(java.math.BigDecimal/valueOf 1.5)", "1.5M");
    is("(BigDecimal/valueOf 3)", "3M");
    is("(decimal? (BigDecimal/valueOf 1.5))", "true");
    is("(class (BigDecimal/valueOf 1.5))", "java.math.BigDecimal");
    // `valueOf` is the SHORTEST decimal (Double.toString), the double
    // CONSTRUCTOR is the exact binary value -- measured Java semantics.
    is("(BigDecimal/valueOf 0.1)", "0.1M");
    is(
        "(BigDecimal. 0.1)",
        "0.1000000000000000055511151231257827021181583404541015625M",
    );

    is("(uri? (java.net.URI/create \"http://a.com\"))", "true");
    is("(str (java.net.URI/create \"http://a.com\"))", "\"http://a.com\"");
    is("(class (java.net.URI/create \"http://a.com\"))", "java.net.URI");
    is(
        "(= (java.net.URI/create \"http://a.com\") (java.net.URI. \"http://a.com\"))",
        "true",
    );
}

// ---------------------------------------------------------------------------
// SPEC-W3: the wave-2 defect ledger (docs/SPEC-PORT-PATCHES.md section D)
//
// Each of these was a `MOVA-PATCH` in the port -- a place where
// `clojure.spec.alpha` could not be spelled the way upstream spells it --
// or a generator that could not run. Every expectation below was measured
// against Clojure 1.13.0-alpha6 first.
// ---------------------------------------------------------------------------

/// D5: `clojure.core/newline`. Unblocks port patch P7: spec's
/// `explain-printer` calls `(newline)` after every problem it prints, and
/// had been patched to `(print "\n")`.
#[test]
fn newline_writes_one_lf_and_returns_nil() {
    is("(newline)", "nil");
    is("(with-out-str (newline))", "\"\\n\"");
    // Two in a row, and interleaved with `print`, so the ORDER through
    // `*out*` is pinned, not just the byte.
    is("(with-out-str (print \"a\") (newline) (print \"b\"))", "\"a\\nb\"");
    is("(with-out-str (prn (newline)))", "\"\\nnil\\n\"");
}

/// D5: `clojure.core/locking`. Unblocks port patch P11:
/// `clojure.spec.gen.alpha/dynaload` wraps its `require` in `(locking
/// dynalock ...)`.
///
/// The monitor is process-wide and reentrant rather than per-object -- a
/// documented deviation, see `builtins::conc::MONITOR_STATE`. What is
/// pinned here is the CONTRACT: the body's value comes back, the lock is
/// released on a normal exit AND on a throw (a leak would hang the next
/// case), nesting does not self-deadlock, and `x` is evaluated.
#[test]
fn locking_runs_its_body_and_always_releases() {
    is("(locking (Object.) :ok)", ":ok");
    is("(locking :a (locking :b [:in :both]))", "[:in :both]");
    is(
        "(try (locking :a (throw (ex-info \"boom\" {}))) (catch Throwable t :caught))",
        ":caught",
    );
    // Would block forever if the throw above had leaked the monitor.
    is("(locking :a :released)", ":released");
    // `x` is evaluated, exactly as upstream's `(let [lockee# ~x] ...)` does.
    is("(let [a (atom 0)] (locking (swap! a inc) @a))", "1");
    // `monitorenter` on `null` throws on the JVM; the body never runs.
    assert!(
        ev("(locking nil :never)").starts_with("ERR:"),
        "got: {}",
        ev("(locking nil :never)")
    );
}

/// D6: `clojure.core/mapcat`'s multi-collection arity. Unblocks port patch
/// P8 (three `(mapcat vector A B)` sites had been rewritten as
/// `(interleave A B)`).
#[test]
fn mapcat_accepts_multiple_collections() {
    is("(mapcat vector [1 2 3] [:a :b] [4 5 6])", "(1 :a 4 2 :b 5)");
    is("(mapcat vector [1 2] [:a :b])", "(1 :a 2 :b)");
    // Shortest wins, including against an INFINITE partner -- exactly what
    // spec's `(mapcat vector (c/or (seq ks) (repeat :_)) forms)` relies on.
    is("(mapcat vector [1 2] (repeat :_))", "(1 :_ 2 :_)");
    is("(mapcat vector [1 2] nil)", "()");
    // The 1- and 2-arity forms are unchanged.
    is("(mapcat (fn [x] [x x]) [1 2])", "(1 1 2 2)");
    is("(into [] (mapcat (fn [x] [x x])) [1 2])", "[1 1 2 2]");
}

/// D3: `repeat` takes any number for its count and TRUNCATES it, exactly
/// as `clojure.lang.Repeat/create`'s `RT.longCast`ed `long` does. This is
/// deliberately NOT `take`/`drop`'s ceiling rule -- both are measured.
#[test]
fn repeat_truncates_a_non_integer_count() {
    is("(repeat 2.0 :x)", "(:x :x)");
    is("(repeat 2.7 :x)", "(:x :x)");
    is("(repeat 5/2 :x)", "(:x :x)");
    is("(repeat 2.5M :x)", "(:x :x)");
    is("(repeat -1.5 :x)", "()");
    is("(repeat 0.5 :x)", "()");
    // Unchanged for the ordinary int case, and still a type error for a
    // non-number.
    is("(repeat 3 :x)", "(:x :x :x)");
    assert!(ev("(repeat :two :x)").starts_with("ERR:"));
    // `take`'s rule is still the OTHER one -- the two must not converge.
    is("(take 1.5 [1 2 3])", "(1 2)");
}

/// D4: `(java.util.UUID. msb lsb)`, the only public `UUID` constructor.
/// `clojure.test.check.generators/uuid` builds every UUID it makes this
/// way, so `(s/gen uuid?)` could not generate without it.
#[test]
fn uuid_two_long_constructor() {
    is(
        "(java.util.UUID. 1 2)",
        "#uuid \"00000000-0000-0001-0000-000000000002\"",
    );
    is("(java.util.UUID. 0 0)", "#uuid \"00000000-0000-0000-0000-000000000000\"");
    // Both halves are reinterpreted as UNSIGNED before packing -- a Java
    // `long` is signed, the 128 bits are not.
    is(
        "(str (java.util.UUID. -1 -1))",
        "\"ffffffff-ffff-ffff-ffff-ffffffffffff\"",
    );
    is("(uuid? (java.util.UUID. 1 2))", "true");
    is("(class (java.util.UUID. 1 2))", "java.util.UUID");
    is(
        "(= (java.util.UUID. 1 2) (java.util.UUID/fromString \
         \"00000000-0000-0001-0000-000000000002\"))",
        "true",
    );
    assert!(ev("(java.util.UUID. 1)").starts_with("ERR:"));
    assert!(ev("(java.util.UUID. \"a\" \"b\")").starts_with("ERR:"));
}

/// SPEC-W3: `(.shiftLeft big n)` and a bignum-tolerant
/// `Long/numberOfLeadingZeros` -- the two host calls
/// `clojure.test.check.generators`' bigint machinery (`two-pow`,
/// `shrink-long`) makes on the way to `gen/simple-type`.
#[test]
fn biginteger_shift_left_and_leading_zeros() {
    is("(bigint (.shiftLeft (biginteger 1) 40))", "1099511627776N");
    is("(class (.shiftLeft (biginteger 1) 4))", "java.math.BigInteger");
    // A negative distance shifts the other way, as `BigInteger.shiftLeft`
    // documents.
    is("(.shiftLeft (biginteger 1024) -3)", "128");
    is("(.shiftLeft (biginteger 1) 0)", "1");

    // `Long.numberOfLeadingZeros` takes a primitive long, so a BigInt
    // argument reaches it through `Reflector.boxArg`'s `longValue()` --
    // measured: 23 for 2^40, and a throw for a value that cannot be a long.
    is("(Long/numberOfLeadingZeros (bigint 1099511627776))", "23");
    is("(Long/numberOfLeadingZeros 1)", "63");
    is("(Long/numberOfLeadingZeros 0)", "64");
    assert!(ev("(Long/numberOfLeadingZeros (bigint 12345678901234567890))").starts_with("ERR:"));
}

/// D7, the half of `(:refer-clojure :exclude [...])` that IS honoured: a
/// name the namespace excluded does not warn when the namespace defines
/// it. Real Clojure prints nothing there (the mapping was removed); mova
/// printed one line per excluded name, which meant a wall of warnings for
/// `clojure.spec.alpha` and `clojure.test.check.generators`.
///
/// The un-refer half is deliberately NOT implemented -- see
/// `Interp::refer_clojure_excludes` -- so an excluded core name still
/// resolves until the namespace defines its own. That is asserted here
/// too, so the divergence is pinned rather than merely described.
#[test]
fn refer_clojure_exclude_suppresses_the_shadow_warning() {
    // The warning goes to the vendored suite's shim-local `*err*`, so this
    // asserts the state the warning is derived from instead: a namespace
    // that excludes a name and defines it still gets ITS OWN definition,
    // and one that excludes without defining still reaches core.
    is(
        "(ns w3a (:refer-clojure :exclude [and])) (defn and [& xs] :mine) (and 1 2)",
        ":mine",
    );
    is(
        "(ns w3b (:refer-clojure :exclude [merge])) (merge {:a 1} {:b 2})",
        "{:a 1, :b 2}",
    );
}

/// SPEC-W3: metadata is transparent to the `clojure.lang.ILookup` hook
/// too, not only to class/protocol dispatch.
///
/// `clojure.spec.alpha`'s `with-name` puts `::name` in the metadata of
/// EVERY registered spec, so `(s/get-spec ::an-fspec)` hands back a
/// `with-meta`-wrapped `reify`. Before this, `(get that :args)` answered
/// through `valAt` while `(:args that)` -- upstream's own spelling inside
/// `fspec-impl` -- answered `nil`, because the keyword/symbol apply arms
/// tested the un-peeled value's shape. On the JVM `(with-meta o m)` on a
/// `reify` is a new object of the same class implementing the same
/// interfaces, so all four spellings reach the same method.
#[test]
fn ilookup_is_reached_through_a_metadata_wrapper() {
    let setup = "(def r (reify clojure.lang.ILookup \
                 (valAt [_ k] [:got k]) (valAt [_ k nf] [:got k nf]))) \
                 (def rm (with-meta r {:z 1})) ";
    is(&format!("{setup}[(:a rm) (get rm :a) ('a rm)]"), "[[:got :a] [:got :a] [:got a]]");
    is(&format!("{setup}[(:a rm :nf) (get rm :a :nf) ('a rm :nf)]"),
       "[[:got :a :nf] [:got :a :nf] [:got a :nf]]");
    // The metadata itself survives, and is what a `valAt` body would see.
    is(&format!("{setup}(meta rm)"), "{:z 1}");
    // The unwrapped object is unaffected.
    is(&format!("{setup}[(:a r) (get r :a)]"), "[[:got :a] [:got :a]]");
}
