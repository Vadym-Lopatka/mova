//! clojure.repl/special-doc-map (generated from Clojure 1.12): (name, forms, doc, url).
pub static SPECIAL_FORMS: &[(&str, &[&str], &str, &str)] = &[
    (".", &["(.instanceMember instance args*)", "(.instanceMember Classname args*)", "(Classname/staticMethod args*)", "Classname/staticField"], "The instance member form works for both fields and methods.\n  They all expand into calls to the dot operator at macroexpansion time.", "java_interop#dot"),
    ("def", &["(def symbol doc-string? init?)"], "Creates and interns a global var with the name\n  of symbol in the current namespace (*ns*) or locates such a var if\n  it already exists.  If init is supplied, it is evaluated, and the\n  root binding of the var is set to the resulting value.  If init is\n  not supplied, the root binding of the var is unaffected.", ""),
    ("do", &["(do exprs*)"], "Evaluates the expressions in order and returns the value of\n  the last. If no expressions are supplied, returns nil.", ""),
    ("if", &["(if test then else?)"], "Evaluates test. If not the singular values nil or false,\n  evaluates and yields then, otherwise, evaluates and yields else. If\n  else is not supplied it defaults to nil.", ""),
    ("monitor-enter", &["(monitor-enter x)"], "Synchronization primitive that should be avoided\n  in user code. Use the 'locking' macro.", ""),
    ("monitor-exit", &["(monitor-exit x)"], "Synchronization primitive that should be avoided\n  in user code. Use the 'locking' macro.", ""),
    ("new", &["(Classname. args*)", "(new Classname args*)"], "The args, if any, are evaluated from left to right, and\n  passed to the constructor of the class named by Classname. The\n  constructed object is returned.", "java_interop#new"),
    ("quote", &["(quote form)"], "Yields the unevaluated form.", ""),
    ("recur", &["(recur exprs*)"], "Evaluates the exprs in order, then, in parallel, rebinds\n  the bindings of the recursion point to the values of the exprs.\n  Execution then jumps back to the recursion point, a loop or fn method.", ""),
    ("set!", &["(set! var-symbol expr)", "(set! (. instance-expr instanceFieldName-symbol) expr)", "(set! (. Classname-symbol staticFieldName-symbol) expr)"], "Used to set thread-local-bound vars, Java object instance\nfields, and Java class static fields.", "vars#set"),
    ("throw", &["(throw expr)"], "The expr is evaluated and thrown, therefore it should\n  yield an instance of some derivee of Throwable.", ""),
    ("try", &["(try expr* catch-clause* finally-clause?)"], "catch-clause => (catch classname name expr*)\n  finally-clause => (finally expr*)\n\n  Catches and handles Java exceptions.", ""),
    ("var", &["(var symbol)"], "The symbol must resolve to a var, and the Var object\nitself (not its value) is returned. The reader macro #'x expands to (var x).", ""),
];
