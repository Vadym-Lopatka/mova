//! nREPL interpreter gaps: dynamic `*1 *2 *3 *e`, exceptions as values,
//! error facts for a host (`errinfo`), `trampoline`, and form-by-form reading
//! (`formfeed`). Expected values are from real Clojure 1.13.0-alpha6 and the
//! JVM nREPL goldens (`crates/mova-nrepl/oracle`).

use mova::errinfo::{error_info, ErrorCtx, Phase};
use mova::formfeed::FormFeed;
use mova::internal::{pr_str, Interp, Value};

fn eval(src: &str) -> String {
    let mut i = Interp::new();
    match i.eval_str("t", src) {
        Ok(v) => pr_str(&v),
        Err(e) => panic!("{src}: {e}"),
    }
}

// ---- task 1 ----

#[test]
fn star_vars_are_dynamic_and_settable() {
    assert_eq!(eval("[*1 *2 *3 *e]"), "[nil nil nil nil]");
    assert_eq!(
        eval("(binding [*1 10 *2 20 *3 30 *e :x] (set! *3 *2) (set! *2 *1) (set! *1 99) [*1 *2 *3 *e])"),
        "[99 10 20 :x]"
    );
    // no leak out of the binding
    assert_eq!(eval("(binding [*1 1] (set! *1 2)) *1"), "nil");
    assert_eq!(eval("(:dynamic (meta #'*e))"), "true");
}

#[test]
fn host_can_push_and_set_star_vars() {
    // The host API is the existing `VarCell`: `push_binding`, `set_binding`, `pop_binding`.
    let mut i = Interp::new();
    let cell = |i: &Interp, n: &str| i.resolve_var_cell(&mova::internal::Symbol { ns: Some("clojure.core".into()), name: n.into() });
    let c1 = cell(&i, "*1");
    c1.push_binding(Value::Nil);
    assert!(c1.set_binding(Value::Int(42)));
    assert_eq!(pr_str(&i.eval_str("t", "*1").unwrap()), "42");
    c1.pop_binding();
    assert_eq!(pr_str(&i.eval_str("t", "*1").unwrap()), "nil");
}

// ---- task 2 ----

fn caught(expr: &str) -> String {
    eval(&format!(
        "(try {expr} (catch Throwable e [(.getName (class e)) (ex-message e) (ex-data e) (some-> (ex-cause e) class .getName)]))"
    ))
}

#[test]
fn exceptions_as_values() {
    assert_eq!(caught("(/ 1 0)"), r#"["java.lang.ArithmeticException" "Divide by zero" nil nil]"#);
    assert_eq!(
        caught(r#"(throw (ex-info "x" {:a 1}))"#),
        r#"["clojure.lang.ExceptionInfo" "x" {:a 1} nil]"#
    );
    assert_eq!(caught(r#"(throw (Exception. "x"))"#), r#"["java.lang.Exception" "x" nil nil]"#);
    assert_eq!(
        caught(r#"(throw (RuntimeException. "r" (Exception. "c")))"#),
        r#"["java.lang.RuntimeException" "r" nil "java.lang.Exception"]"#
    );
    assert_eq!(
        caught(r#"(throw (ex-info "o" {} (Exception. "inner")))"#),
        r#"["clojure.lang.ExceptionInfo" "o" {} "java.lang.Exception"]"#
    );
    // unresolved symbol: CompilerException with a RuntimeException cause
    let r = caught("(eval 'foo-unresolved)");
    assert!(r.starts_with(r#"["clojure.lang.Compiler$CompilerException" "Syntax error compiling at (t:"#), "{r}");
    assert!(r.ends_with(r#""java.lang.RuntimeException"]"#), "{r}");
    assert_eq!(
        eval("(try (eval 'foo-unresolved) (catch Exception e (ex-message (ex-cause e))))"),
        r#""Unable to resolve symbol: foo-unresolved in this context""#
    );
    assert_eq!(
        eval(r#"(try (read-string "(1 2") (catch Exception e [(.getName (class e)) (ex-message e)]))"#),
        r#"["java.lang.RuntimeException" "EOF while reading"]"#
    );
    assert_eq!(caught("(+ 1 :a)").split('"').nth(1).unwrap(), "java.lang.ClassCastException");
    assert_eq!(caught("(nth [] 5)"), r#"["java.lang.IndexOutOfBoundsException" nil nil nil]"#);
    assert_eq!(
        eval("(try (/ 1 0) (catch Exception e (instance? java.lang.ArithmeticException e)))"),
        "true"
    );
    assert_eq!(eval("(try (/ 1 0) (catch Exception e (.getMessage e)))"), r#""Divide by zero""#);
    assert_eq!(eval("(try (/ 1 0) (catch Exception e (= (class e) (type e))))"), "true");
}

// ---- task 3 ----

fn info_for(code: &str, ctx: ErrorCtx) -> mova::errinfo::ErrorInfo {
    let mut i = Interp::new();
    let mut feed = FormFeed::new(&mut i, "REPL", code, 1, 1, false);
    loop {
        match feed.next_form(&mut i) {
            Ok(Some(f)) => {
                if let Err(e) = i.eval_form(&f.form) {
                    return error_info(&mut i, &e, &ctx);
                }
            }
            Ok(None) => panic!("no error in {code}"),
            Err(e) => {
                let ctx = ErrorCtx { phase: Some(Phase::ReadSource), ..ctx };
                return error_info(&mut i, &e, &ctx);
            }
        }
    }
}

#[test]
fn error_text_divide_by_zero() {
    let e = info_for("(/ 1 0)", ErrorCtx { eval_id: 7, ..Default::default() });
    assert_eq!(e.text, "Execution error (ArithmeticException) at user/eval7 (REPL:1).\nDivide by zero\n");
    assert_eq!(e.ex(), "class java.lang.ArithmeticException");
    assert_eq!(e.root_ex(), "class java.lang.ArithmeticException");
    assert_eq!(e.phase, Phase::Execution);
    assert_eq!(e.message.as_deref(), Some("Divide by zero"));
}

#[test]
fn error_text_ex_info_and_causes() {
    let e = info_for(r#"(throw (ex-info "boom" {:a 1}))"#, ErrorCtx { eval_id: 3, ..Default::default() });
    assert_eq!(e.text, "Execution error (ExceptionInfo) at user/eval3 (REPL:1).\nboom\n");
    assert_eq!(e.ex(), "class clojure.lang.ExceptionInfo");
    // RuntimeException prints no class; root is the Exception
    let e = info_for(r#"(throw (RuntimeException. "outer" (Exception. "inner")))"#, ErrorCtx { eval_id: 3, ..Default::default() });
    assert_eq!(e.ex(), "class java.lang.RuntimeException");
    assert_eq!(e.root_ex(), "class java.lang.Exception");
    let e = info_for(r#"(throw (Exception. "multi\nline"))"#, ErrorCtx { eval_id: 3, ..Default::default() });
    assert_eq!(e.text, "Execution error at user/eval3 (REPL:1).\nmulti\nline\n");
    let e = info_for(r#"(assert false)"#, ErrorCtx { eval_id: 3, ..Default::default() });
    assert_eq!(e.text, "Execution error (AssertionError) at user/eval3 (REPL:1).\nAssert failed: false\n");
}

#[test]
fn error_text_compile_syntax_check() {
    let e = info_for("(foo-unresolved 1)", ErrorCtx::default());
    assert_eq!(e.text, "Syntax error compiling at (REPL:1:1).\nUnable to resolve symbol: foo-unresolved in this context\n");
    assert_eq!(e.ex(), "class clojure.lang.Compiler$CompilerException");
    assert_eq!(e.root_ex(), "class clojure.lang.Compiler$CompilerException");
    assert_eq!(e.phase, Phase::CompileSyntaxCheck);
    assert_eq!((e.line, e.column), (Some(1), Some(1)));
}

#[test]
fn error_text_read_source() {
    // forms before a read error are returned first
    let mut i = Interp::new();
    let mut feed = FormFeed::new(&mut i, "REPL", "(+ 1 2) (+ 1", 1, 1, false);
    let f = feed.next_form(&mut i).unwrap().unwrap();
    assert_eq!(pr_str(&i.eval_form(&f.form).unwrap()), "3");
    let err = feed.next_form(&mut i).unwrap_err();
    let e = error_info(&mut i, &err, &ErrorCtx::default());
    assert_eq!(e.text, "Syntax error reading source at (REPL:2:1).\nEOF while reading, starting at line 1\n");
    assert_eq!(e.ex(), "class clojure.lang.ExceptionInfo");
    assert_eq!(e.root_ex(), "class java.lang.RuntimeException");
    assert!(feed.next_form(&mut i).unwrap().is_none());

    let e = info_for(")", ErrorCtx::default());
    assert_eq!(e.text, "Syntax error reading source at (REPL:1:2).\nUnmatched delimiter: )\n");
}

#[test]
fn ex_triage_and_ex_str_on_data() {
    // Same data shape as `Throwable->map` gives on the JVM.
    let src = r#"
      (clojure.main/ex-str
        (clojure.main/ex-triage
          {:via [{:type 'clojure.lang.ExceptionInfo :message "boom" :data {:a 1}}]
           :trace [['user$f 'invoke "REPL" 4]]
           :cause "boom"}))"#;
    assert_eq!(eval(src), r#""Execution error (ExceptionInfo) at user/f (REPL:4).\nboom\n""#);
    assert_eq!(
        eval(r#"(clojure.main/err->msg (try (/ 1 0) (catch Exception e e)))"#),
        r#""Execution error (ArithmeticException) at (REPL:1).\nDivide by zero\n""#
    );
    assert_eq!(
        eval(r#"(clojure.main/ex-str {:clojure.error/phase :print-eval-result :clojure.error/class 'java.lang.ClassCastException :clojure.error/symbol 'clojure.lang.Numbers/inc :clojure.error/line 139 :clojure.error/source "Numbers.java" :clojure.error/cause "bad"})"#),
        r#""Error printing return value (ClassCastException) at clojure.lang.Numbers/inc (Numbers.java:139).\nbad\n""#
    );
}

#[test]
fn throwable_map_has_phase_and_data() {
    assert_eq!(
        eval("(let [m (Throwable->map (try (eval 'foo-unresolved) (catch Exception e e)))] [(:phase m) (vec (map :type (:via m)))])"),
        "[:compile-syntax-check [clojure.lang.Compiler$CompilerException java.lang.RuntimeException]]"
    );
}

// ---- task 4 ----

#[test]
fn trampoline_works() {
    assert_eq!(eval("(trampoline (fn [] 1))"), "1");
    assert_eq!(
        eval("(letfn [(ev? [n] (if (zero? n) true #(od? (dec n)))) (od? [n] (if (zero? n) false #(ev? (dec n))))] (trampoline ev? 100000))"),
        "true"
    );
    assert_eq!(eval("(trampoline (fn [a b] (if (> a 3) [a b] #(do a))) 1 2)"), "1");
    assert_eq!(eval("(trampoline (fn [x] (if (< x 5) (fn [] (inc x)) x)) 7)"), "7");
}

// ---- task 5 ----

#[test]
fn formfeed_positions_and_rest() {
    let mut i = Interp::new();
    let mut feed = FormFeed::new(&mut i, "foo/bar.clj", "(def pv1 1)\n(def pv2 2)\n(select-keys (meta #'pv2) [:line :column :file])", 10, 5, false);
    let a = feed.next_form(&mut i).unwrap().unwrap();
    assert_eq!((a.line, a.column), (10, 5));
    i.eval_form(&a.form).unwrap();
    assert_eq!(feed.rest().trim_start_matches('\n').chars().next(), Some('('));
    let b = feed.next_form(&mut i).unwrap().unwrap();
    assert_eq!((b.line, b.column), (11, 1));
    i.eval_form(&b.form).unwrap();
    let c = feed.next_form(&mut i).unwrap().unwrap();
    assert_eq!((c.line, c.column), (12, 1));
    let v = i.eval_form(&c.form).unwrap();
    assert_eq!(pr_str(&v), r#"{:line 11, :column 1, :file "foo/bar.clj"}"#);
    assert!(feed.next_form(&mut i).unwrap().is_none());
    assert_eq!(feed.rest(), "");
}

#[test]
fn formfeed_error_location_uses_offset() {
    let e = info_for_at("(+ 1 2)\n(/ 1 0)", 20, 1);
    assert_eq!(e.text, "Execution error (ArithmeticException) at user/eval0 (REPL:21).\nDivide by zero\n");
    let e = info_for_at("(foo-unresolved)", 30, 2);
    assert_eq!(e.text, "Syntax error compiling at (REPL:30:2).\nUnable to resolve symbol: foo-unresolved in this context\n");
}

fn info_for_at(code: &str, line: usize, col: usize) -> mova::errinfo::ErrorInfo {
    let mut i = Interp::new();
    let mut feed = FormFeed::new(&mut i, "REPL", code, line, col, false);
    while let Some(f) = feed.next_form(&mut i).unwrap() {
        if let Err(e) = i.eval_form(&f.form) {
            return error_info(&mut i, &e, &ErrorCtx::default());
        }
    }
    panic!("no error");
}

#[test]
fn reader_metadata_matches_clojure() {
    // Clojure: plain `read-string` attaches no :line/:column to a list.
    assert_eq!(eval(r#"(meta (read-string "(a b)"))"#), "nil");
}

/// Every case of the JVM `b07_read_errors` golden that mova can read.
#[test]
fn read_error_goldens() {
    let cases: &[(&str, &str, &str, &str)] = &[
        // code, err text, ex, root-ex
        ("(+ 1", "Syntax error reading source at (REPL:2:1).\nEOF while reading, starting at line 1\n", "clojure.lang.ExceptionInfo", "java.lang.RuntimeException"),
        (")", "Syntax error reading source at (REPL:1:2).\nUnmatched delimiter: )\n", "clojure.lang.ExceptionInfo", "java.lang.RuntimeException"),
        ("{:a}", "Syntax error reading source at (REPL:1:5).\nMap literal must contain an even number of forms\n", "clojure.lang.ExceptionInfo", "java.lang.RuntimeException"),
        ("#<foo>", "Syntax error reading source at (REPL:1:3).\nUnreadable form\n", "clojure.lang.ExceptionInfo", "java.lang.RuntimeException"),
        ("\"unterminated", "Syntax error reading source at (REPL:2:1).\nEOF while reading string\n", "clojure.lang.ExceptionInfo", "java.lang.RuntimeException"),
        ("[1 2", "Syntax error reading source at (REPL:2:1).\nEOF while reading, starting at line 1\n", "clojure.lang.ExceptionInfo", "java.lang.RuntimeException"),
        ("1 2 )", "Syntax error reading source at (REPL:1:6).\nUnmatched delimiter: )\n", "clojure.lang.ExceptionInfo", "java.lang.RuntimeException"),
        ("::a/b", "Syntax error reading source at (REPL:2:1).\nInvalid token: ::a/b\n", "clojure.lang.ExceptionInfo", "java.lang.RuntimeException"),
        ("'", "Syntax error reading source at (REPL:2:1).\nEOF while reading\n", "clojure.lang.ExceptionInfo", "java.lang.RuntimeException"),
        ("\\", "Syntax error reading source at (REPL:2:1).\nEOF while reading character\n", "clojure.lang.ExceptionInfo", "java.lang.RuntimeException"),
        ("{:a 1 :a 2}", "Syntax error reading source at (REPL:1:12).\nDuplicate key: :a\n", "clojure.lang.ExceptionInfo", "java.lang.IllegalArgumentException"),
        ("#{1 1}", "Syntax error reading source at (REPL:1:7).\nDuplicate key: 1\n", "clojure.lang.ExceptionInfo", "java.lang.IllegalArgumentException"),
        ("1/0", "Syntax error reading source at (REPL:2:1).\nDivide by zero\n", "clojure.lang.ExceptionInfo", "java.lang.ArithmeticException"),
        ("09", "Syntax error reading source at (REPL:2:1).\nInvalid number: 09\n", "clojure.lang.ExceptionInfo", "java.lang.NumberFormatException"),
    ];
    for (code, text, ex, root) in cases {
        let e = info_for(code, ErrorCtx::default());
        assert_eq!(&e.text, text, "code {code:?}");
        assert_eq!(e.class, *ex, "code {code:?}");
        assert_eq!(e.root_class, *root, "code {code:?}");
    }
}

/// The `e01_errors` golden cases that mova can run, `eval<N>` normalized.
#[test]
fn execution_error_goldens() {
    let cases: &[(&str, &str, &str)] = &[
        ("(clojure.core/+ 1 :a)", "Execution error (ClassCastException) at user/eval0 (REPL:1).\nclass clojure.lang.Keyword cannot be cast to class java.lang.Number (clojure.lang.Keyword is in unnamed module of loader 'app'; java.lang.Number is in module java.base of loader 'bootstrap')\n", "java.lang.ClassCastException"),
        ("(nth [] 5)", "Execution error (IndexOutOfBoundsException) at user/eval0 (REPL:1).\nnull\n", "java.lang.IndexOutOfBoundsException"),
        ("(apply + (range 3) 1)", "Execution error (IllegalArgumentException) at user/eval0 (REPL:1).\nDon't know how to create ISeq from: java.lang.Long\n", "java.lang.IllegalArgumentException"),
        ("(throw (Exception.))", "Execution error at user/eval0 (REPL:1).\nnull\n", "java.lang.Exception"),
        ("(throw (Error. \"err\"))", "Execution error (Error) at user/eval0 (REPL:1).\nerr\n", "java.lang.Error"),
        ("(throw (ex-info \"no data\" {}))", "Execution error (ExceptionInfo) at user/eval0 (REPL:1).\nno data\n", "clojure.lang.ExceptionInfo"),
    ];
    for (code, text, class) in cases {
        let e = info_for(code, ErrorCtx::default());
        assert_eq!(&e.text, text, "code {code}");
        assert_eq!(e.class, *class, "code {code}");
    }
}

// ---- round 2, task 3: Mova's rich report next to the JVM text ----

/// Runs `codes` in one interpreter; reports the error of the last one.
fn report_for(codes: &[&str], last_line: usize) -> mova::errinfo::ErrorInfo {
    let mut i = Interp::new();
    for (n, c) in codes.iter().enumerate() {
        let line = if n + 1 == codes.len() { last_line } else { 1 };
        let mut feed = FormFeed::new(&mut i, "REPL", c, line, 1, false);
        loop {
            match feed.next_form(&mut i) {
                Ok(Some(f)) => {
                    if let Err(e) = i.eval_form(&f.form) {
                        return error_info(&mut i, &e, &ErrorCtx::default());
                    }
                }
                Ok(None) => break,
                Err(e) => return error_info(&mut i, &e, &ErrorCtx::default()),
            }
        }
    }
    panic!("no error in {codes:?}");
}

#[test]
fn report_unresolved_symbol_did_you_mean() {
    let e = report_for(&["(map inc (rang 3))"], 1);
    assert_eq!(e.text, "Syntax error compiling at (REPL:1:10).\nUnable to resolve symbol: rang in this context\n");
    assert_eq!(
        e.report.unwrap(),
        "   ,-[REPL:1:11]\n 1 | (map inc (rang 3))\n   :           ^^|^\n   :             `-- `rang` is not defined in namespace user\n   `----\n  help: did you mean `rand`, `range`?\n"
    );
}

#[test]
fn report_arity_error_shows_arglists() {
    let e = report_for(&["(defn f [a b] a) (f 1)"], 1);
    assert_eq!(
        e.report.unwrap(),
        "   ,-[REPL:1:18]\n 1 | (defn f [a b] a) (f 1)\n   :                  ^^|^^\n   :                    `-- Wrong number of args (1) passed to: user/f\n   `----\n  help: `user/f` accepts: [a b]\n"
    );
}

#[test]
fn report_divide_by_zero_in_nested_call_has_frames() {
    let e = report_for(&["(defn g [x] (/ x 0)) (defn h [y] (g y)) (h 1)"], 1);
    let r = e.report.unwrap();
    assert!(r.contains("the divisor is zero here"), "{r}");
    assert!(r.ends_with("  at user/g (REPL:1:22)\n  at user/h (REPL:1:41)\n"), "{r}");
}

#[test]
fn report_read_error_shows_the_opener() {
    let e = report_for(&["(+ 1"], 1);
    assert_eq!(e.text, "Syntax error reading source at (REPL:2:1).\nEOF while reading, starting at line 1\n");
    assert_eq!(
        e.report.unwrap(),
        "   ,-[REPL:1:1]\n 1 | (+ 1\n   : |\n   : `-- unclosed list, opened here\n   `----\n  help: add the missing closing delimiter, or remove this opening one\n"
    );
}

#[test]
fn report_macro_syntax_error_and_text() {
    let e = report_for(&["(let [x] 1)"], 1);
    assert_eq!(
        e.text,
        "Syntax error macroexpanding clojure.core/let at (REPL:1:1).\n[x] - failed: even-number-of-forms? at: [:bindings] spec: :clojure.core.specs.alpha/bindings\n"
    );
    assert_eq!(e.phase, Phase::MacroSyntaxCheck);
    assert_eq!(
        e.report.unwrap(),
        "   ,-[REPL:1:6]\n 1 | (let [x] 1)\n   :      ^|^\n   :       `-- let: bindings must be an even number of forms\n   `----\n  help: every binding needs a name and a value; add the missing value or remove the name\n  note: `let` was macroexpanded with the bindings [x]\n"
    );
    let e = report_for(&["(if)"], 1);
    assert_eq!(e.text, "Syntax error compiling if at (REPL:1:1).\nToo few arguments to if\n");
}

#[test]
fn report_class_cast_keeps_mova_wording() {
    let e = report_for(&["(+ 1 :a)"], 1);
    assert!(e.text.starts_with("Execution error (ClassCastException) at user/eval0 (REPL:1).\nclass clojure.lang.Keyword cannot be cast"));
    assert_eq!(
        e.report.unwrap(),
        "   ,-[REPL:1:1]\n 1 | (+ 1 :a)\n   : ^^^^|^^^\n   :     `-- +: expected a number, got keyword\n   `----\n"
    );
}

#[test]
fn report_error_in_fn_from_an_earlier_eval() {
    // `g` and `h` were defined by an earlier eval (earlier buffer); the call is on line 20.
    let e = report_for(&["(defn g [x] (/ x 0)) (defn h [y] (g y))", "(h 5)"], 20);
    assert_eq!(e.text, "Execution error (ArithmeticException) at user/g (REPL:1).\nDivide by zero\n");
    let r = e.report.unwrap();
    // snippet is the earlier buffer's line 1, not the padded new buffer
    assert!(r.starts_with("   ,-[REPL:1:1]\n 1 | (defn g [x] (/ x 0)) (defn h [y] (g y))"), "{r}");
    assert!(r.ends_with("  at user/g (REPL:1:22)\n  at user/h (REPL:20:1)\n"), "{r}");
}

#[test]
fn error_phase_goldens_round2() {
    let e = report_for(&["(new Nope)"], 1);
    assert_eq!(e.text, "Syntax error (IllegalArgumentException) compiling new at (REPL:1:1).\nUnable to resolve classname: Nope\n");
    let e = report_for(&["(Nope/x)"], 1);
    assert_eq!(e.text, "Syntax error compiling at (REPL:1:1).\nNo such namespace: Nope\n");
    let e = report_for(&["foo-unresolved"], 1);
    assert_eq!(e.text, "Syntax error compiling at (REPL:0:0).\nUnable to resolve symbol: foo-unresolved in this context\n");
    let e = report_for(&["(throw 1)"], 1);
    assert_eq!(e.class, "java.lang.ClassCastException");
    let e = report_for(&["#foo/bar 1"], 1);
    assert_eq!(e.text, "Syntax error reading source at (REPL:2:1).\nNo reader function for tag foo/bar\n");
    let e = report_for(&["(inc)"], 1);
    assert_eq!(e.text, "Execution error (ArityException) at user/eval0 (REPL:1).\nWrong number of args (0) passed to: clojure.core/inc\n");
}

// ---- round 2, tasks 1-2: printer and pprint, against real Clojure output ----

#[test]
fn pprint_matches_clojure() {
    // `pprint_cases.expected` was produced by Clojure 1.13.0-alpha6 on the same file.
    let src = std::fs::read_to_string("tests/fixtures/pprint_cases.clj").unwrap();
    let want = std::fs::read_to_string("tests/fixtures/pprint_cases.expected").unwrap();
    let mut i = Interp::new();
    // `prn` writes to the real stdout; capture by evaluating the last form's value instead.
    let body = src.replace("(prn ", "(identity ");
    let v = i.eval_str("t", &body).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(pr_str(&v) + "\n", want);
}

#[test]
fn nrepl_util_print_signatures() {
    assert_eq!(
        eval(r#"(require '[nrepl.util.print :as p]) (let [w (java.io.StringWriter.)] (p/pprint (vec (range 12)) w {:right-margin 20}) (str w))"#),
        r#""[0\n 1\n 2\n 3\n 4\n 5\n 6\n 7\n 8\n 9\n 10\n 11]""#
    );
    assert_eq!(
        eval(r#"(require '[nrepl.util.print :as p]) (let [w (java.io.StringWriter.)] (p/pr (range 100) w {:print-length 3}) (p/pr {:a 1} w) (str w))"#),
        r#""(0 1 2 ...){:a 1}""#
    );
    assert_eq!(
        eval(r#"(require '[nrepl.util.print :as p]) (let [w (java.io.StringWriter.)] (p/pprint (range 30) w {:length 5}) (str w))"#),
        r#""(0 1 2 3 4 ...)""#
    );
}

#[test]
fn printer_matches_clojure() {
    assert_eq!(eval("(def x 1)"), "#'user/x");
    assert_eq!(eval("(defn f [] 1)"), "#'user/f");
    assert_eq!(eval("(defmacro m [] 1)"), "#'user/m");
    assert_eq!(eval("(var map)"), "#'clojure.core/map");
    assert_eq!(eval("(str (class 1))"), r#""class java.lang.Long""#);
    assert_eq!(eval("(pr-str (class 1))"), r#""java.lang.Long""#);
    assert_eq!(eval("(pr-str {:a/b 1 :a/c 2})"), r##""#:a{:b 1, :c 2}""##);
    assert_eq!(eval(r#"(binding [*print-readably* false] (pr-str "a\"b" \a))"#), r#""a\"b a""#);
    assert_eq!(eval(r"(pr-str \backspace \formfeed (str \backspace))"), r#""\\backspace \\formfeed \"\\b\"""#);
    assert_eq!(eval("(pr-str 5e-324)"), r#""4.9E-324""#);
    let a = eval("(pr-str (atom 1))");
    assert!(a.starts_with("\"#object[clojure.lang.Atom 0x") && a.ends_with(" {:status :ready, :val 1}]\""), "{a}");
    let d = eval("(pr-str (delay 1))");
    assert!(d.ends_with(" {:status :pending, :val nil}]\""), "{d}");
    let n = eval("(pr-str *ns*)");
    assert!(n.starts_with("\"#object[clojure.lang.Namespace 0x") && n.ends_with(r#" \"user\"]""#), "{n}");
    let f = eval("(defn ff [] 1) (pr-str ff)");
    assert!(f.starts_with("\"#object[user$ff 0x") && f.contains(r#" \"user$ff@"#), "{f}");
    assert_eq!(
        eval(r#"(pr-str (ex-info "x" {:a 1}))"#),
        r##""#error {\n :cause \"x\"\n :data {:a 1}\n :via\n [{:type clojure.lang.ExceptionInfo\n   :message \"x\"\n   :data {:a 1}}]\n :trace\n []}""##
    );
}

#[test]
fn colour_option_adds_ansi() {
    let mut i = Interp::new();
    let mut feed = FormFeed::new(&mut i, "REPL", "(foo-unresolved 1)", 1, 1, false);
    let f = feed.next_form(&mut i).unwrap().unwrap();
    let err = i.eval_form(&f.form).unwrap_err();
    let plain = error_info(&mut i, &err, &ErrorCtx::default()).report.unwrap();
    let col = error_info(&mut i, &err, &ErrorCtx { colour: true, ..Default::default() }).report.unwrap();
    assert!(!plain.contains('\u{1b}'));
    assert!(col.contains('\u{1b}'), "{col:?}");
}

#[test]
fn read_cond_without_allow_is_a_read_error() {
    let e = info_for("#?(:clj 1)", ErrorCtx::default());
    assert_eq!(e.text, "Syntax error reading source at (REPL:1:3).\nConditional read not allowed\n");
    let mut i = Interp::new();
    let mut feed = FormFeed::new(&mut i, "REPL", "#?(:clj 1 :cljs 2)", 1, 1, true);
    let f = feed.next_form(&mut i).unwrap().unwrap();
    assert_eq!(pr_str(&i.eval_form(&f.form).unwrap()), "1");
}

// ---- round 2, task 4: docstrings and var meta from the static table ----

#[test]
fn core_var_meta_matches_clojure() {
    // Values below were read from real Clojure 1.13.0-alpha6.
    assert_eq!(
        eval("(mapv (fn [v] (let [m (meta v)] [(:added m) (pr-str (:arglists m)) (count (:doc m))])) [#'map #'inc #'filter #'defn #'assoc])"),
        r#"[["1.0" "([f] [f coll] [f c1 c2] [f c1 c2 c3] [f c1 c2 c3 & colls])" 371] ["1.2" "([x])" 108] ["1.0" "([pred] [pred coll])" 180] ["1.0" "([name doc-string? attr-map? [params*] prepost-map? body] [name doc-string? attr-map? ([params*] prepost-map? body) + attr-map?])" 261] ["1.0" "([map key val] [map key val & kvs])" 261]]"#
    );
    assert_eq!(eval("(str (ns-name (:ns (meta #'inc))))"), r#""clojure.core""#);
    // a user var that shadows a core name gets no core docs
    assert_eq!(eval("(def map 1) (:doc (meta #'map))"), "nil");
}

#[test]
fn doc_prints_like_clojure() {
    assert_eq!(
        eval("(with-out-str (doc inc))"),
        r#""-------------------------\nclojure.core/inc\n([x])\n  Returns a number one greater than num. Does not auto-promote\n  longs, will throw on overflow. See also: inc'\n""#
    );
    assert_eq!(
        eval("(with-out-str (doc when))"),
        r#""-------------------------\nclojure.core/when\n([test & body])\nMacro\n  Evaluates test. If logical true, evaluates body in an implicit do.\n""#
    );
}

// ---- gate closing: values below are from real Clojure 1.13.0-alpha6 / the JVM nREPL goldens ----

fn eval_strict(src: &str) -> Result<String, String> {
    let mut i = Interp::new();
    i.eval_str("t", src).map(|v| pr_str(&v)).map_err(|e| e.message.to_string())
}

#[test]
fn in_ns_into_a_new_namespace_has_no_clojure_core__always() {
    // (golden b02 / a04) `str` is unresolved after `(in-ns 'foo.bar)`
    let e = eval_strict("(in-ns 'strict.one) (str 1)").unwrap_err();
    assert!(e.contains("Unable to resolve symbol: str"), "{e}");
    // fully qualified names, `ns` and `refer` bring core back
    assert_eq!(eval_strict("(in-ns 'strict.two) (clojure.core/str 1)").unwrap(), "\"1\"");
    assert_eq!(eval_strict("(ns strict.three) (str 1)").unwrap(), "\"1\"");
    assert_eq!(eval_strict("(in-ns 'strict.four) (clojure.core/refer 'clojure.core) (str 1)").unwrap(), "\"1\"");
    // `user` keeps core
    assert_eq!(eval_strict("(in-ns 'user) (str 1)").unwrap(), "\"1\"");
    // a plain script follows the same rule now (no flag)
    assert!(eval_strict("(in-ns 'lenient.one) (str 1)").is_err());
}

#[test]
fn set_bang_of_a_program_var_without_a_thread_binding_fails_like_the_jvm() {
    let e = eval_strict("(def ^:dynamic *dyn-x* 1) (set! *dyn-x* 2)").unwrap_err();
    assert_eq!(e, "Can't change/establish root binding of: *dyn-x* with set");
    // non-dynamic and core vars fail the same way (JVM)
    assert_eq!(eval_strict("(def plain-x 1) (set! plain-x 2)").unwrap_err(), "Can't change/establish root binding of: plain-x with set");
    assert_eq!(eval_strict("(set! clojure.core/map 2)").unwrap_err(), "Can't change/establish root binding of: map with set");
    // inside `binding` it works and the root stays
    assert_eq!(eval("(def ^:dynamic *dyn-z* 1) [(binding [*dyn-z* 5] (set! *dyn-z* 6) *dyn-z*) *dyn-z*]"), "[6 1]");
}

#[test]
fn agents_run_actions_in_order_and_print_like_clojure() {
    assert_eq!(eval("(let [a (agent 0)] (send a inc) (send-off a + 10) (await a) @a)"), "11");
    let p = eval("(pr-str (agent 0))");
    assert!(p.starts_with("\"#object[clojure.lang.Agent 0x") && p.ends_with(" {:status :ready, :val 0}]\""), "{p}");
    // a failing action stops the agent until it is restarted
    assert_eq!(
        eval("(let [a (agent 1)] (send a (fn [_] (throw (ex-info \"boom\" {})))) (await a) [(ex-message (agent-error a)) (try (send a inc) (catch Exception e (ex-message e))) (do (restart-agent a 5) (send a inc) (await a) @a)])"),
        r#"["boom" "Agent is failed, needs restart" 6]"#
    );
}

#[test]
fn time_prints_elapsed_and_returns_the_value() {
    assert_eq!(eval("(let [s (with-out-str (def r (time (+ 1 2))))] [r (boolean (re-matches #\"\\\"Elapsed time: [0-9.]+ msecs\\\"\\n\" s))])"), "[3 true]");
}

#[test]
fn objects_print_a_jvm_sized_hash_and_threads_and_writers_print_like_the_jvm() {
    let o = eval("(pr-str (Object.))");
    let hash = o.split("0x").nth(1).unwrap().split(' ').next().unwrap();
    assert!(hash.len() <= 8, "{o}");
    let t = eval("(pr-str (Thread. (fn [])))");
    assert!(t.starts_with("\"#object[java.lang.Thread 0x") && t.contains(r#"\"Thread[#"#) && t.contains(",Thread-"), "{t}");
    // pr shows the text quoted, print shows it raw
    let w = eval("(pr-str (java.io.StringWriter.))");
    assert!(w.starts_with("\"#object[java.io.StringWriter 0x") && w.ends_with(r#" \"\"]""#), "{w}");
    let w = eval("(with-out-str (print (java.io.StringWriter.)))");
    assert!(w.starts_with("\"#object[java.io.StringWriter 0x") && w.ends_with(" ]\""), "{w}");
}

#[test]
fn reflection_errors_exceptions_and_deref_of_a_failed_future() {
    // `(.foo 1)`: IllegalArgumentException at run time, not an unresolved symbol
    assert_eq!(
        eval("(try (.foo 1) (catch IllegalArgumentException e (ex-message e)))"),
        "\"No matching field found: foo for class java.lang.Long\""
    );
    assert_eq!(
        eval("(try (Integer/parseInt \"x\") (catch NumberFormatException e [(class e) (ex-message e)]))"),
        "[java.lang.NumberFormatException \"For input string: \\\"x\\\"\"]"
    );
    assert_eq!(eval("(try (Thread/sleep :a) (catch IllegalArgumentException e (ex-message e)))"), "\"No matching method sleep found taking 1 args\"");
    assert_eq!(eval("(try (throw (StackOverflowError.)) (catch Error e [(class e) (ex-message e)]))"), "[java.lang.StackOverflowError nil]");
    // deref of a failed future wraps the cause in ExecutionException, as on the
    // JVM. A thrown non-exception value such as `:boom` has no JVM counterpart to
    // wrap, so `deref` re-raises it as is (`conc_test` pins that).
    assert_eq!(
        eval_strict("(try @(future (/ 1 0)) (catch Exception e [(class e) (ex-message e) (class (ex-cause e))]))").unwrap(),
        "[java.util.concurrent.ExecutionException \"java.lang.ArithmeticException: Divide by zero\" java.lang.ArithmeticException]"
    );
    // a missing namespace is a FileNotFoundException with the JVM's text in the host's error text
    let e = info_for("(require 'no.such.ns)", ErrorCtx::default());
    assert_eq!(e.class, "java.io.FileNotFoundException");
    assert!(e.text.ends_with("Could not locate no/such/ns__init.class, no/such/ns.clj or no/such/ns.cljc on classpath.\n"), "{}", e.text);
}

#[test]
fn print_phase_errors_are_wrapped_in_exception_info() {
    // golden d01 id 18, d03 id 7: `ex` is ExceptionInfo, `root-ex` the original class
    let mut i = Interp::new();
    let mut feed = FormFeed::new(&mut i, "REPL", "(/ 1 0)", 1, 1, false);
    let f = feed.next_form(&mut i).unwrap().unwrap();
    let err = i.eval_form(&f.form).unwrap_err();
    let info = error_info(&mut i, &err, &ErrorCtx { phase: Some(Phase::PrintEvalResult), ..Default::default() });
    assert_eq!(info.ex(), "class clojure.lang.ExceptionInfo");
    assert_eq!(info.root_ex(), "class java.lang.ArithmeticException");
    assert!(info.text.starts_with("Error printing return value (ArithmeticException) at "), "{}", info.text);
}

#[test]
fn line_zero_is_kept_and_def_records_the_hosts_file() {
    // golden b06 id 17: `line: 0` gives `REPL:0`
    let e = info_for_at("(/ 1 0)", 0, 1);
    assert!(e.text.starts_with("Execution error (ArithmeticException) at user/eval") && e.text.contains("(REPL:0)."), "{}", e.text);
    let mut i = Interp::new();
    let mut feed = FormFeed::new(&mut i, "REPL", "(def a 1)\n(def b 2)", 0, 1, false);
    let mut lines = vec![];
    while let Some(f) = feed.next_form(&mut i).unwrap() {
        lines.push(f.line);
        i.eval_form(&f.form).unwrap();
    }
    assert_eq!(lines, vec![0, 1]);
    assert_eq!(pr_str(&i.eval_str("t", "(:line (meta #'b))").unwrap()), "1");
    // `def_file`: what a def records as `:file` (nREPL: NO_SOURCE_PATH without a `file` param)
    i.def_file = Some("NO_SOURCE_PATH".into());
    assert_eq!(pr_str(&i.eval_str("t", "(def c 3) (:file (meta #'c))").unwrap()), "\"NO_SOURCE_PATH\"");
}

#[test]
fn read_eval_and_preserved_reader_conditionals() {
    // golden b07 id 10: `#=(+ 1 2)` is evaluated by the reader
    let mut i = Interp::new();
    let mut feed = FormFeed::new(&mut i, "REPL", "#=(+ 1 2)", 1, 1, false);
    let f = feed.next_form(&mut i).unwrap().unwrap();
    assert_eq!(pr_str(&i.eval_form(&f.form).unwrap()), "3");
    // golden b08: `preserve` keeps the conditional, which prints as `#?(...)`
    for (code, want) in [("#?(:clj 1 :cljs 2)", "#?(:clj 1 :cljs 2)"), ("#?(:cljs 2)", "#?(:cljs 2)"), ("[1 #?@(:clj [2 3])]", "[1 #?@(:clj [2 3])]")] {
        let mut i = Interp::new();
        let mut feed = FormFeed::new(&mut i, "REPL", code, 1, 1, true);
        feed.set_preserve_read_cond(true);
        let f = feed.next_form(&mut i).unwrap().unwrap();
        assert_eq!(pr_str(&i.eval_form(&f.form).unwrap()), want);
    }
}
