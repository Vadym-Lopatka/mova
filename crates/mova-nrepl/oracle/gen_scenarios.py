#!/usr/bin/env python3
"""Writes scenarios/*.json (the data files the oracle runs). Edit here, rerun, commit the JSON."""
import json, os
D = os.path.join(os.path.dirname(os.path.abspath(__file__)), "scenarios")
os.makedirs(D, exist_ok=True)
for f in os.listdir(D):
    if f.endswith(".json"):
        os.remove(os.path.join(D, f))

def ev(i, code, s="$s1", **kw):
    m = {"op": "eval", "id": str(i), "code": code}
    if s:
        m["session"] = s
    m.update(kw)
    return {"send": m}

def op(name, i, s="$s1", **kw):
    m = {"op": name, "id": str(i)}
    if s:
        m["session"] = s
    m.update(kw)
    return {"send": m}

def sc(name, steps, doc="", sessions=("s1",), **kw):
    if name.startswith("f0"):  # let the evaluating thread finish its replies before the next step
        out = []
        for st in steps:
            out.append(st)
            if "await" in st and isinstance(st["await"], dict):
                out.append({"wait": 0.4})
        steps = out
    d = {"name": name, "doc": doc, "sessions": list(sessions), "steps": steps}
    if name.startswith("f0"):
        d["flaky"] = "race: eval-thread replies (err/ex/value/out) vs interrupt-thread replies (interrupted/done)"
    d.update(kw)
    json.dump(d, open(os.path.join(D, name + ".json"), "w"), indent=1)

P = "nrepl.middleware.print/"
C = "nrepl.middleware.caught/"

sc("a01_describe", [op("describe", 1, s=None), op("describe", 2, s=None, **{"verbose?": "true"}),
                    op("describe", 3), op("describe", 4, s=None, **{"verbose?": "false"}), op("describe", 5, s=None, **{"verbose?": "1"})])
sc("a02_edge_messages", [
    {"send": {"op": "nosuchop", "id": "1"}},
    {"send": {"op": "nosuchop", "id": "2", "session": "$s1"}},
    {"send": {"id": "3"}},
    {"send": {"id": "4", "session": "$s1"}},
    {"send": {"op": "describe"}},
    {"send": {"op": "eval", "code": "(+ 1 2)"}},
    {"send": {"op": "eval", "id": "7"}},
    {"send": {"op": "eval", "id": "8", "code": "(+ 1 2)", "session": "00000000-0000-0000-0000-000000000000"}},
    {"send": {"op": "eval", "id": "9", "code": "(+ 1 2)", "session": "not-a-uuid"}},
    {"send": {"op": "eval", "id": "10", "code": "(+ 1 2)", "ns": "no.such.ns"}},
    {"send": {"op": "eval", "id": "11", "code": 5}},
    {"send": {"op": "", "id": "12"}},
    ev(13, "(+ 1 2)", s=None),
    ev(14, "(def nosess 1)", s=None),
    ev(15, "nosess", s=None),
], doc="unknown op, missing op, no id, no session, bad args")
sc("a03_sessions", [
    op("clone", 1, s=None, **{}),
    {"send": {"op": "clone", "id": "2"}, "save_as": "c1"},
    {"send": {"op": "clone", "id": "3", "session": "$s1"}, "save_as": "c2"},
    op("ls-sessions", 4, s=None),
    op("ls-sessions", 5),
    op("close", 6, s="$c1"),
    op("ls-sessions", 7, s=None),
    ev(8, "(+ 1 2)", s="$c1"),
    op("close", 9, s="$c1"),
    op("close", 10, s=None),
    op("close", 11, s="no-such-session"),
    op("describe", 12, s="$c1"),
    op("clone", 13, s="$c1"),
    op("ls-sessions", 14, s="$c1"),
    op("interrupt", 15, s="$c1"),
    op("stdin", 16, s="$c1", stdin="x"),
], doc="clone/close/ls-sessions, use of closed or unknown session")
sc("a04_clone_copies", [
    ev(1, "(set! *print-length* 2)"), ev(2, "(def cl-a 5)"), ev(3, "(in-ns 'clone.ns)"),
    {"send": {"op": "clone", "id": "4", "session": "$s1"}, "save_as": "c"},
    ev(5, "(range 10)", s="$c"), ev(6, "(str *ns*)", s="$c"), ev(7, "(range 10)", s="$s1"),
    {"send": {"op": "clone", "id": "8"}, "save_as": "d"},
    ev(9, "(range 10)", s="$d"), ev(10, "(str *ns*)", s="$d"),
], doc="clone copies parent bindings; plain clone does not")

sc("b01_eval_basic", [
    ev(1, "(+ 1 2)"), ev(2, "1 2 3"), ev(3, '(+ 1 2) (println "x") :k "s"'),
    ev(4, ""), ev(5, "; just a comment"), ev(6, "   \n  "), ev(7, "nil"), ev(8, '"str\\n\\"q\\""'),
    ev(9, "(do (println \"a\") 42)"), ev(10, "#_(ignored)"), ev(11, "1 #_2"),
    ev(12, "(def zz 1)"), ev(13, "(defn fff [] 1)"), ev(14, "(var map)"), ev(15, "(fn [] 1)"), ev(16, "(Object.)"),
    ev(17, "(java.util.Date. 0)"), ev(18, "(atom 1)"), ev(19, "(list 1 2)"), ev(20, "1.5"), ev(21, "1/3"), ev(22, "\\a"), ev(23, "{:a 1}"),
    ev(24, "(take 5 (range))"),
])
sc("b02_eval_ns", [
    ev(1, "(str *ns*)", ns="clojure.string"), ev(2, "(str *ns*)"), ev(3, "(join \",\" [1 2])", ns="clojure.string"),
    ev(4, "(str *ns*)", ns="no.such.ns"), ev(5, "(str *ns*)", ns=""),
    ev(6, "(in-ns 'foo.bar)"), ev(7, "(str *ns*)"), ev(8, "(def zz 1)"), ev(9, "foo.bar/zz"),
    ev(10, "(str *ns*)", ns="user"), ev(11, "(str *ns*)"), ev(12, "(ns baz.qux (:require [clojure.set :as s]))"), ev(13, "(s/union #{1} #{2})"),
    ev(14, "(str *ns*)", s=None), ev(15, "(str *ns*)", s="$s2"),
], sessions=("s1", "s2"))
sc("b03_eval_def", [ev(1, "(def abc-b03 41)"), ev(2, "(inc abc-b03)"), ev(3, "abc-b03", s="$s2"), ev(4, "(defn f-b03 [x] (* 2 x))"),
                    ev(5, "(f-b03 4)", s="$s2"), ev(6, "(defmacro m-b03 [] 1)"), ev(7, "(m-b03)")], sessions=("s1", "s2"))
sc("b04_stars", [ev(1, "10"), ev(2, "20"), ev(3, "30"), ev(4, "[*1 *2 *3]"), ev(5, "[*1 *2 *3]"),
                 ev(6, "(/ 1 0)"), ev(7, "(class *e)"), ev(8, "(.getMessage *e)"), ev(9, "[*1 *2 *3]"),
                 ev(10, "[*1 *2 *3]", s="$s2"), ev(11, "(class *e)", s="$s2"),
                 ev(12, "1 2"), ev(13, "[*1 *2]"), ev(14, "(do 5 (/ 1 0))"), ev(15, "*1"), ev(16, "(throw (ex-info \"x\" {}))"), ev(17, "(ex-message *e)")],
   sessions=("s1", "s2"))
sc("b05_set_bang", [
    ev(1, "(set! *print-length* 3)"), ev(2, "(range 10)"), ev(3, "(range 10)", s="$s2"),
    ev(4, "*print-length*"), ev(5, "*print-length*", s="$s2"),
    ev(6, "(set! *warn-on-reflection* true)"), ev(7, "*warn-on-reflection*"), ev(8, "*warn-on-reflection*", s="$s2"),
    ev(9, "(set! *print-level* 1)"), ev(10, "[[1 [2]] [3]]"), ev(11, "(set! *unchecked-math* true)"), ev(12, "*unchecked-math*"),
    ev(13, "(set! *print-length* nil)"), ev(14, "(range 10)"),
    ev(15, "(set! *print-length* 2) (range 10)"), ev(16, "(range 10)"),
    ev(17, "(binding [*print-length* 1] (set! *print-length* 5))"), ev(18, "(range 10)"),
    ev(19, "(set! *no-such-star* 1)"), ev(20, "(def ^:dynamic *dyn-b05* 1)"), ev(21, "(set! *dyn-b05* 2)"), ev(22, "*dyn-b05*"),
    ev(23, "(set! *ns* (the-ns 'clojure.string))"), ev(24, "(str *ns*)"),
], sessions=("s1", "s2"))
sc("b06_pos_params", [
    ev(1, "(def pv1 1)\n(def pv2 2)\n(select-keys (meta #'pv2) [:line :column :file])", file="foo/bar.clj", line=10, column=5),
    ev(2, "(def pv3 1)", file="/abs/path/baz.clj", line=3),
    ev(3, "(select-keys (meta #'pv3) [:line :column :file])"),
    ev(4, "(/ 1 0)", file="foo/bar.clj", line=10, column=5),
    ev(5, "(+ 1 2)\n(/ 1 0)", file="foo/bar.clj", line=20),
    ev(6, "(foo-unresolved)", file="foo/bar.clj", line=30, column=2),
    ev(7, "(def pv4 1)", file="x.clj"),
    ev(8, "(select-keys (meta #'pv4) [:line :column :file])"),
    ev(9, "(def pv5 1)", line=7),
    ev(10, "(select-keys (meta #'pv5) [:line :column :file])"),
    ev(11, "*file*", file="q.clj"), ev(12, "*source-path*", file="d/q.clj"), ev(13, "(+ 1", file="r.clj", line=5, column=3),
    ev(14, "(def pv6 1)\n\n(def pv7 2)", line=100, column=1), ev(15, "(map #(:line (meta %)) [#'pv6 #'pv7])"),
    ev(16, "(/ 1 0)", line="12") | {"timeout": 3}, ev(17, "(/ 1 0)", line=0),
])
sc("b07_read_errors", [
    ev(1, "(+ 1"), ev(2, ")"), ev(3, "{:a}"), ev(4, "(+ 1 2) (+ 1"), ev(5, "#<foo>"), ev(6, "\"unterminated"), ev(7, "[1 2"),
    ev(8, "(+ 1 2) ) (+ 3 4)"), ev(9, "1 2 )"), ev(10, "#=(+ 1 2)"), ev(11, "::a/b"), ev(12, "#foo/bar 1"), ev(13, "'"), ev(14, "\\"),
    ev(15, "{:a 1 :a 2}"), ev(16, "#{1 1}"), ev(17, "(println 1) (+ 1"), ev(18, "1/0"), ev(19, "09"), ev(20, "#?(:clj 1)"),
])
sc("b08_read_cond", [
    ev(1, "#?(:clj 1 :cljs 2)"), ev(2, "#?(:clj 1 :cljs 2)", **{"read-cond": "allow"}),
    ev(3, "#?(:clj 1 :cljs 2)", **{"read-cond": "preserve"}), ev(4, "#?(:cljs 2)", **{"read-cond": "allow"}),
    ev(5, "#?(:cljs 2)", **{"read-cond": "preserve"}), ev(6, "[#?@(:clj [1 2])]", **{"read-cond": "allow"}),
    ev(7, "#?(:clj 1)", **{"read-cond": "bogus"}), ev(8, "(+ 1 #?(:clj 2 :default 3))", **{"read-cond": "allow"}),
    ev(9, "#?(:clj 1 :cljs 2)", **{"read-cond": ""}),
])

# ---- output
sc("c01_output_basic", [
    ev(0, "(require 'clojure.pprint 'clojure.repl)"),
    ev(1, '(println "a")'), ev(2, '(print "a")'), ev(3, '(print "a") (print "b")'),
    ev(4, '(binding [*out* *err*] (println "e"))'), ev(5, '(binding [*out* *err*] (print "e"))'),
    ev(6, '(println "a") (binding [*out* *err*] (println "e")) (println "b") 1'),
    ev(7, '(prn "x" :y)'), ev(8, '(printf "%d-%s\\n" 1 "z")'), ev(9, '(.write *out* "w")'), ev(10, '(print "a") (flush) (print "b")'),
    ev(11, '(println "multi\\nline\\nout")'), ev(12, '(print "no-newline-then-value") 1'), ev(13, '(println "")'),
    ev(14, '(println "é ü 日本 😀")'), ev(15, '(print "") 1'), ev(16, '(pr-str 1)'),
    ev(17, '(doseq [i (range 5)] (print i))'), ev(18, '(doseq [i (range 5)] (println i))'),
    ev(19, '(print "a") (/ 1 0)'), ev(20, '(print "partial") (println)'), ev(21, '(.println System/out "sysout-only")'),
    ev(22, '(.println System/err "syserr-only")'), ev(23, '(clojure.pprint/pprint {:a 1})'), ev(24, '(clojure.pprint/pprint (range 40))'),
    ev(25, '(println *1)'), ev(26, '(clojure.repl/doc map)'), ev(27, '(time 1)'), ev(28, '(print (str (char 0) "z"))'),
])
sc("c02_output_big", [
    ev(1, '(print (apply str (repeat 5000 "a")))'), ev(2, '(println (apply str (repeat 5000 "b")))'),
    ev(3, '(dotimes [i 3000] (println i))'), ev(4, '(print (apply str (repeat 1024 "c")))'), ev(5, '(print (apply str (repeat 1025 "d")))'),
    ev(6, '(print (apply str (repeat 1023 "e")))'), ev(7, '(dotimes [i 300] (print "ab"))'), ev(8, '(dotimes [i 600] (print "abc"))'),
    ev(9, '(print (apply str (repeat 3000 "日")))'), ev(10, '(print (apply str (repeat 1500 "é")))'),
    ev(11, '(binding [*out* *err*] (print (apply str (repeat 3000 "f"))))'), ev(12, '(dotimes [i 2000] (print "x") (when (zero? (mod i 500)) (flush)))'),
    ev(13, '(print (apply str (repeat 2048 "g")))'), ev(14, '(print (apply str (repeat 2047 "h")))'),
    ev(15, '(print (apply str (repeat 1023 "i"))) (print "jj")'), ev(16, '(dotimes [i 3000] (print i " "))'),
], doc="chunk boundaries (1024-byte buffer) and ordering vs value")
sc("c03_output_future", [
    ev(1, '(future (Thread/sleep 400) (println "late") (binding [*out* *err*] (println "late-err")) :f)'),
    {"wait": 1.0},
    ev(2, '(println "next")'),
    ev(3, '(do (future (Thread/sleep 100) (println "f2")) (Thread/sleep 400) 5)'),
    ev(4, '(doto (Thread. (fn [] (Thread/sleep 200) (println "raw-thread"))) (.start))'),
    {"wait": 0.8},
    ev(5, '(.println System/out "sys-from-eval")'),
    ev(6, '(println "bg") (future (Thread/sleep 200) (println "after done"))'), {"wait": 0.8},
    ev(7, '(def pr-agent (agent 0))'), ev(8, '(send pr-agent (fn [x] (println "agent out") x))'), {"wait": 0.4},
    ev(9, '(pmap (fn [x] (println "pm" x) x) [1])'), {"wait": 0.3},
])

# ---- print
sc("d01_print", [
    ev(1, "(zipmap (range 12) (range 12))", **{P + "print": "nrepl.util.print/pprint"}),
    ev(2, "(zipmap (range 12) (range 12))", **{P + "print": "nrepl.util.print/pprint", P + "options": {"right-margin": 20}}),
    ev(18, "{:a 1}", **{P + "print": "clojure.pprint/pprint"}),
    ev(3, "{:a 1}", **{P + "print": "clojure.core/prn"}),
    ev(4, "{:a 1}", **{P + "print": "clojure.core/pr-str"}),
    ev(5, "{:a 1}", **{P + "print": "no.such/fn"}),
    ev(6, "{:a 1}", **{P + "print": "nrepl.util.print/pr"}),
    ev(7, "(range 100)", **{P + "options": {"length": 3}, P + "print": "nrepl.util.print/pr"}),
    ev(8, "{:a \"x\"}", **{P + "print": "clojure.core/print"}),
    ev(9, "{:a 1}", **{P + "print": "clojure.core/str"}),
    ev(10, "(range 30)", **{P + "print": "nrepl.util.print/pprint", P + "options": {"length": 5}}),
    ev(11, "(/ 1 0)", **{P + "print": "nrepl.util.print/pprint"}),
    ev(12, "(println \"o\") 1", **{P + "print": "nrepl.util.print/pprint"}),
    ev(17, "(range 10) (range 5)", **{P + "print": "nrepl.util.print/pprint", P + "options": {"length": 2}}),
])
sc("d04_print_keys", [
    ev(1, "{:a 1}", s="$s1", **{P + "keys": ["value", "ns"]}) | {"timeout": 3},
    ev(2, "{:a 1}", s="$s2", **{P + "keys": ["ns"]}) | {"timeout": 3},
    ev(3, "{:a 1}", s="$s3", **{P + "keys": []}) | {"timeout": 3},
    ev(4, "{:a 1}", s="$s4", **{P + "keys": ["value"]}) | {"timeout": 3},
    ev(5, "{:a 1}", s="$s5", **{P + "keys": "value"}) | {"timeout": 3},
    ev(6, "(print 1)", s="$s6", **{P + "keys": ["ns"]}) | {"timeout": 3},
    ev(7, "{:a 1}", s="$s7", **{P + "keys": ["value", "ns"], P + "print": "clojure.core/prn"}) | {"timeout": 3},
], sessions=("s1", "s2", "s3", "s4", "s5", "s6", "s7"), settle=1.0,
   doc="::keys sent from a client; hung evals get no reply (timeouts recorded)")

sc("d02_print_stream", [
    ev(1, "(range 50)", **{P + "stream?": "1", P + "buffer-size": 10}),
    ev(2, "(range 50)", **{P + "stream?": "1"}),
    ev(3, "(range 50)", **{P + "buffer-size": 10}),
    ev(4, "(range 3000)", **{P + "stream?": "1"}),
    ev(5, "(range 50)", **{P + "stream?": "1", P + "buffer-size": 1}),
    ev(6, "(range 50)", **{P + "stream?": "1", P + "buffer-size": 0}),
    ev(7, "(range 50)", **{P + "stream?": "1", P + "buffer-size": 4096}),
    ev(8, "(range 50)", **{P + "stream?": "1", P + "buffer-size": 10, P + "print": "nrepl.util.print/pprint"}),
    ev(10, "(/ 1 0)", **{P + "stream?": "1", C + "print?": "1"}),
    ev(11, "(print \"o\") (range 5)", **{P + "stream?": "1", P + "buffer-size": 3}),
    ev(12, "\"é日本\"", **{P + "stream?": "1", P + "buffer-size": 3}),
    ev(13, "(range 50)", **{P + "stream?": "", P + "buffer-size": 10}),
    ev(14, "(range 50)", **{P + "stream?": "0", P + "buffer-size": 10}),
    ev(15, "(range 50)", **{P + "stream?": "false", P + "buffer-size": 10}),
])
sc("d03_print_quota", [
    ev(1, "(range 100)", **{P + "quota": 20}),
    ev(2, "(range 100)", **{P + "quota": 20, P + "stream?": "1"}),
    ev(3, "(range 100)", **{P + "quota": 20, P + "stream?": "1", P + "buffer-size": 5}),
    ev(4, "(range 5)", **{P + "quota": 1000}), ev(5, "(range 5)", **{P + "quota": 11}), ev(6, "(range 5)", **{P + "quota": 10}),
    ev(7, "(range 100)", **{P + "quota": 0}), ev(8, "(range 100)", **{P + "quota": 20, P + "print": "nrepl.util.print/pprint"}),
    ev(9, "(range 100) (range 100)", **{P + "quota": 20}),
    ev(11, "\"日本日本日本\"", **{P + "quota": 5}),
    ev(12, "(/ 1 0)", **{P + "quota": 5, C + "print?": "1"}),
    ev(13, "(apply str (repeat 100 \"ab\"))", **{P + "quota": 20, P + "stream?": "1"}),
])

# ---- errors
sc("e01_errors", [
    ev(0, "(require 'clojure.pprint 'clojure.repl)"),
    ev(1, "(/ 1 0)"), ev(2, '(throw (ex-info "boom" {:a 1}))'), ev(3, "(foo-unresolved 1)"), ev(4, "foo-unresolved"),
    ev(5, '(throw (RuntimeException. "outer" (Exception. "inner")))'), ev(6, "(assert false)"), ev(7, '(throw (Exception. "multi\\nline"))'),
    ev(8, "(println 1) (/ 1 0) (println 2)"), ev(9, "(.foo 1)"), ev(10, "(nth [] 5)"), ev(11, "(Integer/parseInt \"x\")"),
    ev(12, "(let [x] 1)"), ev(13, "(if)"), ev(14, "(throw (Exception.))"), ev(15, "(throw (ex-info \"no data\" {}))"),
    ev(16, "(throw (Error. \"err\"))"), ev(17, "(throw (StackOverflowError.))"), ev(18, "(defn f [] (f)) (f)"),
    ev(19, "(clojure.core/+ 1 :a)"), ev(20, "(require 'no.such.ns)"), ev(21, "(ex-info \"x\" {:a (Object.)})"),
    ev(22, "(throw (ex-info \"o\" {} (ex-info \"i\" {:b 2})))"), ev(23, "(map inc [1 2 :a])"), ev(24, "(doall (map inc [1 2 :a]))"),
    ev(25, "(apply + (range 3) 1)"), ev(26, "(new Nope)"), ev(27, "(Nope/x)"), ev(28, "(throw nil)"), ev(29, "(throw 1)"),
    ev(30, "(binding [*err* *out*] (/ 1 0))"), ev(31, "(.printStackTrace (Exception. \"pst\"))"), ev(32, "(clojure.repl/pst (Exception. \"pst2\"))"),
    ev(33, "(do (future (/ 1 0)) (Thread/sleep 200) 1)"), ev(34, "(deref (future (/ 1 0)))"),
    ev(35, "(ns bad.ns (:require [no.such.lib]))"), ev(36, "(def)"), ev(38, "(Thread/sleep :a)"),
])
sc("e02_caught", [
    ev(1, "(/ 1 0)", **{C + "print?": "1"}),
    ev(2, "(/ 1 0)", **{C + "print?": "1", P + "print": "nrepl.util.print/pprint"}),
    ev(3, "(/ 1 0)", **{C + "caught": "clojure.core/prn"}),
    ev(4, "(/ 1 0)", **{C + "caught": "clojure.core/prn", C + "print?": "1"}),
    ev(5, "(/ 1 0)", **{C + "caught": "no.such/fn"}),
    ev(6, "(/ 1 0)", **{C + "caught": "clojure.core/identity"}),
    ev(7, "(/ 1 0)", **{C + "print?": ""}), ev(8, "(/ 1 0)", **{C + "print?": "0"}),
    ev(9, "(foo-unresolved)", **{C + "print?": "1"}),
    ev(10, "(+ 1", **{C + "print?": "1"}),
    ev(11, "(throw (ex-info \"b\" {:a 1}))", **{C + "print?": "1"}),
    ev(12, "(throw (RuntimeException. \"o\" (Exception. \"i\")))", **{C + "print?": "1"}),
    ev(13, "(/ 1 0)", **{C + "caught": "clojure.core/prn", P + "stream?": "1", C + "print?": "1"}),
    ev(14, "(/ 1 0)", **{C + "print?": "1", P + "quota": 10}),
])

# ---- interrupt
sc("f01_interrupt_sleep", [
    ev(1, "(Thread/sleep 100000)", **{}) | {"await": False},
    {"wait": 0.5},
    op("interrupt", 2, **{"interrupt-id": "1"}),
    {"await": {"id": "1", "status": "done"}},
    ev(3, "(+ 1 2)"),
    op("interrupt", 4, **{"interrupt-id": "1"}),
    op("interrupt", 5), op("interrupt", 6, **{"interrupt-id": "zzz"}),
    op("interrupt", 7, s=None, **{"interrupt-id": "1"}),
    op("interrupt", 8, s="no-such-session", **{"interrupt-id": "1"}),
])
sc("f02_interrupt_wrong_id", [
    ev(1, "(Thread/sleep 100000)") | {"await": False}, {"wait": 0.5},
    op("interrupt", 2, **{"interrupt-id": "nope"}),
    op("interrupt", 3, **{"interrupt-id": "1"}), {"await": {"id": "1", "status": "done"}},
    ev(4, "(+ 1 2)"),
])
sc("f03_interrupt_no_id", [
    ev(1, "(Thread/sleep 100000)") | {"await": False}, {"wait": 0.5},
    op("interrupt", 2), {"await": {"id": "1", "status": "done"}},
    ev(3, "(+ 1 2)"),
    ev(4, "(do (print \"before\") (Thread/sleep 100000))") | {"await": False}, {"wait": 0.5},
    op("interrupt", 5, **{"interrupt-id": "4"}), {"await": {"id": "4", "status": "done"}},
    ev(10, "(+ 1 2)"),
])
sc("f07_interrupt_catch_finally", [
    ev(6, "(try (Thread/sleep 100000) (catch InterruptedException e :caught))") | {"await": False}, {"wait": 0.5},
    op("interrupt", 7, **{"interrupt-id": "6"}), {"await": {"id": "6", "status": "done"}},
    ev(8, "(try (Thread/sleep 100000) (finally (println \"fin\")))") | {"await": False}, {"wait": 0.5},
    op("interrupt", 9, **{"interrupt-id": "8"}), {"await": {"id": "8", "status": "done"}},
    ev(10, "(+ 1 2)"),
], flaky="race: replies of the eval thread (value/out after catch/finally) vs `interrupted`/done replies of the interrupt thread")
sc("f04_interrupt_queue", [
    ev(1, "(Thread/sleep 100000)") | {"await": False}, {"wait": 0.3},
    ev(2, "(+ 1 2)") | {"await": False}, ev(3, "(+ 3 4)") | {"await": False}, {"wait": 0.3},
    op("interrupt", 4, **{"interrupt-id": "2"}),
    {"wait": 0.3},
    op("interrupt", 5, **{"interrupt-id": "1"}), {"await": {"id": "3", "status": "done"}},
], doc="queued evals in one session run in order; interrupt a queued one")
sc("f05_interrupt_loop", [
    ev(1, "(loop [] (recur))") | {"await": False}, {"wait": 0.5},
    op("interrupt", 2, **{"interrupt-id": "1"}) | {"timeout": 15},
    {"await": {"id": "1", "status": "done"}, "timeout": 15},
    ev(3, "(+ 1 2)") | {"timeout": 5},
], settle=1.0)
sc("f06_interrupt_idle_other_conn", [
    {"open": "b"},
    ev(1, "(Thread/sleep 100000)") | {"await": False}, {"wait": 0.3},
    {**op("interrupt", 2, **{"interrupt-id": "1"}), "conn": "b"},
    {"await": {"id": "1", "status": "done"}},
    {**ev(3, "(+ 1 2)"), "conn": "b"},
])

# ---- stdin
sc("g01_stdin", [
    ev(1, "(read-line)") | {"await": "need-input"},
    op("stdin", 2, stdin="hello\n"),
    {"await": {"id": "1", "status": "done"}},
    ev(3, "(read)") | {"await": "need-input"},
    op("stdin", 4, stdin="(1 2)\n"),
    {"await": {"id": "3", "status": "done"}},
    op("stdin", 5, stdin="buffered\n"),
    ev(6, "(read-line)"),
    ev(7, "[(read-line) (read-line)]") | {"await": "need-input"},
    op("stdin", 8, stdin="l1\nl2\n"), {"await": {"id": "7", "status": "done"}},
    ev(9, "(read-line)") | {"await": "need-input"},
    op("stdin", 10, stdin="partial"), {"wait": 0.5},
    op("stdin", 11, stdin=" line\n"), {"await": {"id": "9", "status": "done"}},
    ev(14, "(.read *in*)") | {"await": "need-input"},
    op("stdin", 15, stdin="A"), {"await": {"id": "14", "status": "done"}},
    ev(16, "(read-line)") | {"await": "need-input"}, op("stdin", 17, stdin="x\n"), {"await": {"id": "16", "status": "done"}},
    op("stdin", 18), op("stdin", 19, s=None, stdin="x\n"),
    ev(20, "(println (read-line))") | {"await": "need-input"}, op("stdin", 21, stdin="zz\n"), {"await": {"id": "20", "status": "done"}},
    ev(22, "(read-line)") | {"await": "need-input"},
    op("interrupt", 23, **{"interrupt-id": "22"}), {"await": {"id": "22", "status": "done"}},
    ev(24, "(+ 1 2)"),
], doc="need-input / stdin op", settle=0.5)

sc("g02_stdin_eof", [
    ev(12, "(slurp *in*)") | {"await": "need-input", "timeout": 3},
    op("stdin", 13, stdin=""), {"wait": 0.5},
    ev(14, "(read-line)") | {"timeout": 3},
    ev(15, "(+ 1 2)"),
], settle=0.5, doc="empty stdin closes *in* for that session")

# ---- load-file
sc("h01_load_file", [
    op("load-file", 1, file="(ns lf.a)\n(defn f [] 1)\n(f)", **{"file-name": "a.clj", "file-path": "$TMP/a.clj"}),
    ev(2, "(lf.a/f)"), ev(3, "(select-keys (meta #'lf.a/f) [:file :line])"), ev(4, "(str *ns*)"),
    op("load-file", 5, file="(ns lf.b)\n(def x 1)\n(/ 1 0)\n(def y 2)", **{"file-name": "b.clj", "file-path": "$TMP/b.clj"}),
    ev(6, "lf.b/x"), ev(7, "(resolve 'lf.b/y)"),
    op("load-file", 8, file="(+ 1 2)"),
    op("load-file", 9, file="(+ 1 2)", **{"file-name": "only-name.clj"}),
    op("load-file", 10, file="(println \"in-file\") (+ 1 2)", **{"file-path": "/x/y/only-path.clj"}),
    op("load-file", 11), op("load-file", 12, file="(+ 1"),
    op("load-file", 13, file="(ns lf.c)\n(def v (foo-unresolved))", **{"file-name": "c.clj", "file-path": "$TMP/c.clj"}),
    op("load-file", 14, file="(def lfv 1)\n(def lfw 2)\n(map #(select-keys (meta %) [:file :line :column]) [#'lfv #'lfw])", **{"file-name": "d.clj", "file-path": "/p/d.clj"}),
    ev(15, "(map #(select-keys (meta %) [:file :line :column]) [#'lfv #'lfw])"),
    op("load-file", 16, file="1 2 3", **{"file-name": "e.clj", "file-path": "e.clj"}),
    op("load-file", 17, file="(+ 1 2)", **{"file-name": "f.clj", "file-path": "f.clj", P + "print": "nrepl.util.print/pprint"}),
    op("load-file", 18, file="(+ 1 2)", **{"file-name": "g.clj", "file-path": "g.clj", "ns": "clojure.string"}),
    op("load-file", 19, file="(str *ns*)", **{"file-name": "h.clj", "file-path": "h.clj"}),
    op("load-file", 20, file="(in-ns 'lf.inns)", **{"file-name": "i.clj", "file-path": "i.clj"}), ev(21, "(str *ns*)"),
    op("load-file", 22, file="(+ 1 2)", s=None, **{"file-name": "j.clj", "file-path": "j.clj"}),
    op("load-file", 23, file="(ns lf.m)(def [1)", **{"file-name": "k.clj", "file-path": "k.clj"}),
], files={"a.clj": "(ns lf.a)\n(defn f [] 1)\n(f)"}, doc="load-file uses file content from the message")

# ---- completions / lookup
sc("i01_completions", [
    op("completions", 1, prefix="ma"),
    op("completions", 2, prefix="clojure.string/"),
    op("completions", 3, prefix="join", ns="clojure.string"),
    op("completions", 4, prefix="ma", ns="clojure.string"),
    op("completions", 5, prefix="ma", options={"extra-metadata": ["arglists", "doc"]}),
    op("completions", 6, prefix="clojure.string/jo", options={"extra-metadata": ["arglists", "doc"]}),
    op("completions", 7, prefix=""), op("completions", 8), op("completions", 9, prefix="zzzzqq"),
    op("completions", 10, prefix="str/j", ns="clojure.string"),
    op("completions", 11, prefix="Sys"), op("completions", 12, prefix="System/get"), op("completions", 13, prefix="java.io.File"),
    op("completions", 14, prefix=":ke"), op("completions", 15, prefix="ma", ns="no.such.ns"),
    op("completions", 16, prefix="clojure.str"), op("completions", 17, prefix="ma", options={"extra-metadata": []}),
    ev(18, "(def my-comp-var 1)"), op("completions", 19, prefix="my-comp"),
    ev(20, "(ns comp.ns (:require [clojure.set :as s]))"), op("completions", 21, prefix="s/un", ns="comp.ns"),
    op("completions", 22, prefix="comp.ns/"),
    op("completions", 23, prefix="when-"), op("completions", 24, prefix="if"), op("completions", 25, prefix="ma", s=None),
    op("complete", 26, prefix="ma"),
])
sc("i02_lookup", [
    op("lookup", 1, sym="map"), op("lookup", 2, sym="when"), op("lookup", 3, sym="if"), op("lookup", 4, sym="nosuchsym-xyz"),
    op("lookup", 5, sym="join", ns="clojure.string"), op("lookup", 6, sym="clojure.string/join"), op("lookup", 7, sym="s/join", ns="lk.ns"),
    op("lookup", 8), op("lookup", 9, sym="String"), op("lookup", 10, sym="java.lang.String"), op("lookup", 11, sym="def"),
    op("lookup", 12, sym="*print-length*"), op("lookup", 13, sym="map", ns="no.such.ns"), op("lookup", 14, sym="loop"),
    ev(15, "(ns lk.ns (:require [clojure.string :as s]))"),
    op("lookup", 16, sym="s/join", ns="lk.ns"),
    ev(17, "(defn lk-fn \"doc here\" [a b] 1)"), op("lookup", 18, sym="lk-fn"),
    op("lookup", 19, sym="lk-fn", ns="user"),
    op("lookup", 20, sym="ns"), op("lookup", 21, sym="Thread/sleep"), op("lookup", 22, sym="clojure.core"),
    op("lookup", 23, sym="map", s=None), op("lookup", 24, sym=""), op("lookup", 25, sym="..."), op("lookup", 26, sym="-"),
    op("eldoc", 27, sym="map"), op("info", 28, sym="map"),
])
sc("j01_dynamic_middleware", [
    op("ls-middleware", 1, s=None), op("add-middleware", 2, s=None, middleware=["nrepl.middleware.session/session"]),
    op("swap-middleware", 3, s=None, middleware=[]), op("ls-middleware", 4),
], sessions=(), doc="expected: unknown-op in 1.8.0")

# ---- transport edge
sc("k01_transport", [
    {"write": {"msgs": [{"op": "describe", "id": "1"}, {"op": "eval", "id": "2", "code": "(+ 1 2)", "session": "$s1"}]},
     "await_ids": ["1", "2"]},
    {"write": {"msgs": [{"op": "eval", "id": "3", "code": "(+ 10 20)", "session": "$s1"}], "split": [7, 20]}, "delay": 0.15, "await_ids": ["3"]},
    {"write": {"msgs": [{"op": "eval", "id": "4", "code": "(println 1) (+ 1 1)", "session": "$s1"}, {"op": "eval", "id": "5", "code": "(+ 5 5)", "session": "$s1"},
                        {"op": "clone", "id": "6"}]}, "await_ids": ["4", "5", "6"]},
    {"write": {"msgs": [{"op": "eval", "id": "7", "code": "(+ 7 7)", "session": "$s1"}], "split": [1]}, "delay": 0.3, "await_ids": ["7"]},
    {"send": {"op": "eval", "id": "9", "session": "$s1", "code": {"$repeat": "(+ 1 2) ", "n": 100}}},
    {"send": {"op": "eval", "id": "10", "session": "$s1", "code": {"$repeat": "a", "n": 1048576, "prefix": "(count \"", "suffix": "\")"}}, "timeout": 30},
    {"send": {"op": "eval", "id": "11", "session": "$s1", "code": {"$repeat": "(+ 1 2) ", "n": 2000}}, "timeout": 30},
], doc="two msgs per write; split writes; 1 MB code",
   flaky="race: each incoming message is handled on its own pool thread, so replies to messages sent back-to-back (one TCP write) can overtake each other, even within one session")

# keep last: affects the whole JVM (System.out forwarding)
sc("z01_forward_system_output", [
    op("forward-system-output", 1),
    ev(2, '(.println System/out "fwd-out")', s="$s2"), ev(3, '(.println System/err "fwd-err")', s="$s2"),
    {"wait": 0.3},
    ev(4, '(do (.print System/out "partial") (.flush System/out) 1)', s="$s2"),
    ev(6, '(println "normal-out")', s="$s2"),
    {"wait": 0.3},
    op("forward-system-output", 7, s=None),
    {"wait": 0.2},
], sessions=("s1", "s2"), flaky="race: forwarded System.out output vs the eval's value/done")

sc("z02_forward_big", [
    op("forward-system-output", 1),
    ev(5, '(.println System/out (apply str (repeat 3000 "z")))', s="$s2"),
    {"wait": 0.5},
    op("forward-system-output", 7, s=None),
    {"wait": 0.2},
], sessions=("s1", "s2"), flaky="race: forwarded chunks vs the eval's value/done",
   doc="chunking of forwarded System.out output")
