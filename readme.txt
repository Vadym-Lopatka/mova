This file exists so tests/clojure-suite/vendor/sequences.clj's
test-iteration deftest has a real, present file to open.

That deftest's own `readme` helper does:

    (java.nio.file.Files/newBufferedReader (.toPath (java.io.File. "readme.txt")))

opening a path relative to whatever directory tools/clojure-suite-run.sh
is invoked from (the repo root, per that script's own usage and
CONFORMANCE-GUARANTEE.md's reproduction instructions) -- exactly mirroring
how the real Clojure test suite relies on a readme.txt at ITS OWN project
root when run for real. The assertion itself only checks that reading
this file two different ways (a hand-rolled iteration over .readLine,
and line-seq) produce the same lines, so this file's actual content does
not matter -- only that it exists and has more than one line.

Second line.
Third line, no trailing content after this one matters either.
